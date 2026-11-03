//! Routine scheduler and FIFO dispatcher. Port of `RoutineScheduler` in `server/routines.ts`:
//! claims due routines, records capacity refusals as `routineSchedulingError`, and starts the oldest
//! queued work through the shared per-Bot dispatcher. Shares the command queue with HTTP.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use grok_bot_model::{Clock, HttpError, HttpResult, RoutineRun};
use grok_bot_store::Store;

use crate::queue::CommandQueue;
use crate::runtime::Runtime;

#[derive(Clone)]
pub struct SchedulerOptions {
    pub now: Clock,
    pub poll_interval: Duration,
    pub auto_start: bool,
}

impl Default for SchedulerOptions {
    fn default() -> Self {
        Self { now: grok_bot_model::system_clock(), poll_interval: Duration::from_secs(1), auto_start: true }
    }
}

pub struct RoutineScheduler {
    store: Arc<Store>,
    runtime: Arc<Runtime>,
    queue: CommandQueue,
    options: SchedulerOptions,
    closed: AtomicBool,
    failure: Mutex<Option<String>>,
    timer: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Serializes ticks: a tick that arrives while another is in flight waits for it (Node returned
    /// the in-flight promise).
    tick_lock: tokio::sync::Mutex<()>,
}

impl RoutineScheduler {
    pub fn new(store: Arc<Store>, runtime: Arc<Runtime>, queue: CommandQueue, options: SchedulerOptions) -> Arc<Self> {
        Arc::new(Self {
            store,
            runtime,
            queue,
            options,
            closed: AtomicBool::new(false),
            failure: Mutex::new(None),
            timer: Mutex::new(None),
            tick_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn error(&self) -> Option<String> {
        self.failure.lock().unwrap().clone()
    }

    pub fn start(self: &Arc<Self>) {
        if self.closed.load(Ordering::SeqCst) || self.error().is_some() || !self.options.auto_start {
            return;
        }
        let mut timer = self.timer.lock().unwrap();
        if timer.is_some() {
            return;
        }
        let this = Arc::clone(self);
        *timer = Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(this.options.poll_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                // A settled run may leave queued work (the next group member) ready at once.
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = this.runtime.settled() => {}
                }
                if this.tick().await.is_err() {
                    eprintln!("Routine scheduler stopped. Check application state before restarting.");
                    break;
                }
                if this.closed.load(Ordering::SeqCst) {
                    break;
                }
            }
        }));
    }

    pub async fn tick(&self) -> HttpResult<()> {
        if let Some(error) = self.error() {
            return Err(HttpError::unavailable(error));
        }
        if self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        let _tick = self.tick_lock.lock().await;
        let result = self
            .queue
            .run(async {
                if self.closed.load(Ordering::SeqCst) || self.runtime.is_paused() {
                    return Ok(());
                }
                self.process((self.options.now)()).await
            })
            .await;
        if let Err(error) = &result {
            self.halt(error);
        }
        result
    }

    /// Manual run (`POST /api/routines/:id/run`).
    pub async fn run(&self, id: &str) -> HttpResult<RoutineRun> {
        self.claim_now(id, |s, now| s.run_routine(id, now)).await
    }

    /// Webhook run (`POST /hooks/routines/:id`): authenticates `key` and claims the run in one store
    /// mutation (so a concurrent rotation or revocation cannot race it), then dispatches FIFO.
    pub async fn run_webhook(&self, id: &str, key: &str, payload: Option<&str>) -> HttpResult<RoutineRun> {
        self.claim_now(id, |s, now| s.claim_webhook_routine(id, key, payload, now)).await
    }

    /// Claim an immediate run through `claim`, rejecting a concurrent claim of the same routine.
    async fn claim_now(&self, id: &str, claim: impl FnOnce(&mut grok_bot_store::State, DateTime<Utc>) -> HttpResult<RoutineRun>) -> HttpResult<RoutineRun> {
        let latest = |store: &Store| store.read(|s| s.disk().routine_runs.iter().rev().find(|r| r.routine_id == id).map(|r| r.id.clone()));
        let observed = latest(&self.store);
        self.queue
            .run(async {
                if self.closed.load(Ordering::SeqCst) {
                    return Err(HttpError::unavailable("The server is shutting down."));
                }
                if self.runtime.is_paused() {
                    return Err(HttpError::unavailable("The server is paused for update maintenance."));
                }
                if self.error().is_some() {
                    return Err(HttpError::unavailable("Routine scheduling stopped. Restart after checking the application data directory."));
                }
                if latest(&self.store) != observed {
                    return Err(HttpError::conflict("This routine was just claimed by another command."));
                }
                let now = (self.options.now)();
                let run = match self.store.mutate(|s| claim(s, now)).await {
                    Ok(run) => run,
                    Err(error) => {
                        if self.store.read(|s| s.assert_writable()).is_err() {
                            self.halt(&error);
                        }
                        return Err(error);
                    }
                };
                if let Err(error) = self.process(now).await {
                    self.halt(&error);
                    return Err(error);
                }
                Ok(run)
            })
            .await
    }

    async fn process(&self, now: DateTime<Utc>) -> HttpResult<()> {
        self.store.read(|s| s.assert_writable())?;
        self.runtime.expire_bot_chains(now).await?;
        self.store
            .mutate(|s| {
                let scheduling_error = match s.claim_due_routines(now) {
                    Ok(()) => None,
                    Err(error) if error.status == 409 || error.status == 413 => Some(error.message),
                    Err(error) => return Err(error),
                };
                s.set_routine_scheduling_error(scheduling_error);
                Ok(())
            })
            .await?;
        if self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.runtime.dispatch_available().await
    }

    fn halt(&self, error: &HttpError) {
        let mut failure = self.failure.lock().unwrap();
        if failure.is_none() {
            *failure = Some(error.message.clone());
        }
        if let Some(timer) = self.timer.lock().unwrap().take() {
            timer.abort();
        }
    }

    pub async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(timer) = self.timer.lock().unwrap().take() {
            timer.abort();
        }
        // Wait for an in-flight tick.
        let _ = self.tick_lock.lock().await;
    }
}
