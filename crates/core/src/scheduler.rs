//! Routine scheduler and FIFO dispatcher. Port of `RoutineScheduler` in `server/routines.ts`:
//! claims due routines, records capacity refusals as `routineSchedulingError`, and starts the oldest
//! queued routine run or bot delivery when no run is active. Shares the command queue with HTTP.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use grok_bot_model::{Clock, DeliveryStatus, HttpError, HttpResult, ReportStatus, RoutineRun, RunStatus};
use grok_bot_store::Store;
use grok_bot_store::bot_messages::queued_reports;

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

enum Work {
    Routine {
        run_id: String,
        conversation_id: String,
        prompt: String,
    },
    Delivery {
        delivery_id: String,
        conversation_id: String,
        prompt: String,
    },
    /// The next member turn of a group round.
    Group {
        conversation_id: String,
    },
    /// The follow-up turn reporting finished deliveries to their sender conversation (all of its
    /// queued reports in one turn).
    Report {
        conversation_id: String,
    },
    Worker {
        worker_id: String,
        conversation_id: String,
    },
    WorkerReport {
        batch_id: String,
    },
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
                if self.closed.load(Ordering::SeqCst) {
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
        if self.closed.load(Ordering::SeqCst) || self.store.read(|s| s.active().is_some()) {
            return Ok(());
        }
        let Some(work) = self.next_work() else { return Ok(()) };
        let started = match &work {
            Work::Routine { conversation_id, prompt, .. } | Work::Delivery { conversation_id, prompt, .. } => self.runtime.start(conversation_id, prompt).await,
            Work::Group { conversation_id } => self.runtime.start_group_turn(conversation_id).await,
            Work::Report { conversation_id } => self.runtime.start_report(conversation_id).await,
            Work::Worker { conversation_id, .. } => self.runtime.start_worker(conversation_id).await,
            Work::WorkerReport { batch_id } => self.runtime.start_worker_report(batch_id).await,
        };
        if let Err(error) = started {
            self.store.read(|s| s.assert_writable())?;
            match work {
                Work::Routine { run_id, .. } => {
                    let message = if error.status == 500 { "Routine could not start. Check runtime and provider settings.".to_owned() } else { error.message };
                    self.store.mutate(|s| s.fail_routine_run(&run_id, &message, now)).await?;
                }
                Work::Delivery { delivery_id, .. } => {
                    let message =
                        if error.status == 500 { "Bot delivery could not start. Check runtime and provider settings.".to_owned() } else { error.message };
                    self.store.mutate(|s| s.fail_bot_delivery(&delivery_id, &message, now)).await?;
                }
                Work::Group { conversation_id } => {
                    let message = if error.status == 500 {
                        "The next group member could not start. Check runtime and provider settings.".to_owned()
                    } else {
                        error.message
                    };
                    self.store.mutate(|s| s.fail_group_round(&conversation_id, &message)).await?;
                }
                Work::Report { conversation_id } => {
                    // Not retried: the reports end with the error and the conversation is unchanged.
                    let message =
                        if error.status == 500 { "The follow-up turn could not start. Check runtime and provider settings.".to_owned() } else { error.message };
                    self.store.mutate(|s| s.fail_bot_reports(&conversation_id, &message, now)).await?;
                }
                Work::Worker { worker_id, .. } => {
                    let message = if error.status == 500 { "Worker could not start. Check runtime and provider settings.".to_owned() } else { error.message };
                    self.store.mutate(|s| s.fail_worker(&worker_id, &message)).await?;
                }
                Work::WorkerReport { batch_id } => {
                    let message = if error.status == 500 { "The grouped worker follow-up could not start.".to_owned() } else { error.message };
                    self.store.mutate(|s| s.fail_worker_report(&batch_id, &message)).await?;
                }
            }
        }
        Ok(())
    }

    /// Oldest queued routine run or bot delivery; ties break by conversation order.
    fn next_work(&self) -> Option<Work> {
        self.store.read(|s| {
            let disk = s.disk();
            let position = |id: &str| disk.conversations.iter().position(|c| c.id == id).unwrap_or(usize::MAX);
            let parse = |t: &str| DateTime::parse_from_rfc3339(t).map(|t| t.timestamp_millis()).unwrap_or(i64::MAX);
            let routines = disk.routine_runs.iter().filter(|run| {
                run.result.is_none()
                    && run.execution_run_id.is_none()
                    && disk.conversations.iter().any(|c| c.id == run.conversation_id && c.run.status == RunStatus::Idle)
            });
            let deliveries = disk.bot_deliveries.iter().filter(|d| d.status == DeliveryStatus::Queued);
            let mut work: Vec<(i64, usize, Work)> = routines
                .map(|run| {
                    (
                        parse(&run.created_at),
                        position(&run.conversation_id),
                        Work::Routine { run_id: run.id.clone(), conversation_id: run.conversation_id.clone(), prompt: run.prompt.clone() },
                    )
                })
                .chain(deliveries.map(|d| {
                    (
                        parse(&d.created_at),
                        position(&d.recipient_conversation_id),
                        Work::Delivery { delivery_id: d.id.clone(), conversation_id: d.recipient_conversation_id.clone(), prompt: d.text.clone() },
                    )
                }))
                .chain(disk.conversations.iter().filter(|c| !c.run.status.is_active()).filter_map(|c| {
                    let round = c.group_round.as_ref().filter(|round| !round.pending_bot_ids.is_empty())?;
                    Some((parse(&round.created_at), position(&c.id), Work::Group { conversation_id: c.id.clone() }))
                }))
                // A report is queued when its delivery finished; the oldest one orders the turn.
                .chain(disk.conversations.iter().filter(|c| !c.run.status.is_active()).filter_map(|c| {
                    let oldest = queued_reports(&disk.bot_deliveries, &c.id).first()?.finished_at.clone().unwrap_or_default();
                    Some((parse(&oldest), position(&c.id), Work::Report { conversation_id: c.id.clone() }))
                }))
                .chain(disk.worker_batches.iter().filter(|batch| !batch.cancelled && batch.released_at.is_some()).flat_map(|batch| {
                    batch.workers.iter().filter(|worker| worker.status == DeliveryStatus::Queued && worker.execution_run_id.is_none()).map(|worker| {
                        (
                            parse(&worker.created_at),
                            position(&worker.conversation_id),
                            Work::Worker { worker_id: worker.id.clone(), conversation_id: worker.conversation_id.clone() },
                        )
                    })
                }))
                .chain(
                    disk.worker_batches
                        .iter()
                        .filter(|batch| {
                            !batch.cancelled
                                && batch.released_at.is_some()
                                && batch.report.as_ref().is_some_and(|report| report.status == ReportStatus::Queued)
                                && batch.workers.iter().all(|worker| grok_bot_store::workers::worker_terminal(worker.status))
                        })
                        .map(|batch| {
                            let finished = batch.workers.iter().filter_map(|worker| worker.finished_at.as_deref()).map(parse).max().unwrap_or(i64::MAX);
                            (finished, position(&batch.parent_conversation_id), Work::WorkerReport { batch_id: batch.id.clone() })
                        }),
                )
                .collect();
            work.sort_by_key(|(created, position, _)| (*created, *position));
            work.into_iter().next().map(|(_, _, work)| work)
        })
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
