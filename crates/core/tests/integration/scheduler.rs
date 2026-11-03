//! Scheduler regressions replacing the coordinator/race cases in `tests/routines.test.ts` and
//! `tests/routines-api.test.ts`. These drive the real Runtime and HTTP command queue, not a fake
//! `start` method. Provider gates and explicitly polled queue waiters control command ordering.

use crate::common;
#[path = "scheduler/provider.rs"]
pub(super) mod provider;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use grok_bot_core::app::{Backend, BackendOptions};
use grok_bot_core::queue::CommandQueue;
use grok_bot_core::runtime::RuntimeOptions;
use grok_bot_core::scheduler::{RoutineScheduler, SchedulerOptions};
use grok_bot_model::{
    BotInput, Clock, Conversation, DiskState, Routine, RoutineInput, RoutineResultStatus, RoutineRun, RoutineSchedule, RoutineTrigger, RunStatus, SettingsInput,
};
use grok_bot_store::Store;
use provider::Provider;
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

const TIMEOUT: Duration = Duration::from_secs(30);
const DUE: &str = "2026-01-01T00:01:00Z";

fn time(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value).unwrap().with_timezone(&Utc)
}

fn once() -> RoutineSchedule {
    RoutineSchedule::Once { at: DUE.into() }
}

struct Fixture {
    data: PathBuf,
    current: Arc<Mutex<DateTime<Utc>>>,
    store: Arc<Store>,
    scheduler: Arc<RoutineScheduler>,
    queue: CommandQueue,
    provider: Provider,
    port: u16,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    provider_server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new(root: &Path, name: &str, immediate: bool, auto_start: bool) -> Self {
        let data = root.join(name).join("data");
        let workspace = root.join(name).join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let current = Arc::new(Mutex::new(time("2026-01-01T00:00:00Z")));
        let clock = current.clone();
        let now: Clock = Arc::new(move || *clock.lock().unwrap());
        let provider = Provider::new(data.join("state.json"), immediate);
        let (base_url, provider_server) = provider.serve().await;
        let backend = Backend::new(BackendOptions {
            data_dir: data.clone(),
            engine_root: root.join("engine"),
            web_root: None,
            embedded_web: None,
            allowed_hosts: vec![],
            now: Some(now.clone()),
            scheduler: Some(SchedulerOptions { now, poll_interval: Duration::from_millis(20), auto_start }),
            runtime: Some(RuntimeOptions { cancel_deadline: Duration::from_millis(100), ..RuntimeOptions::new(root.join("engine")) }),
        })
        .await
        .unwrap();
        backend
            .store
            .mutate(|state| {
                state.update_settings(SettingsInput {
                    base_url,
                    model: "scheduler-test-model".into(),
                    workspace: workspace.to_string_lossy().into_owned(),
                    api_key: None,
                    clear_api_key: Some(true),
                    // These legacy scheduler scenarios exercise an intentionally single-slot cap.
                    // Cross-Bot overlap/default-three are covered by per_bot_concurrency.rs.
                    max_concurrent_runs: Some(1),
                })
            })
            .await
            .unwrap();
        let (store, scheduler, queue) = (backend.store.clone(), backend.scheduler.clone(), backend.queue.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop, stopped) = oneshot::channel();
        let server = tokio::spawn(backend.serve(listener, async {
            let _ = stopped.await;
        }));
        let fixture = Self { data, current, store, scheduler, queue, provider, port, stop: Some(stop), server: Some(server), provider_server };
        // Wait for serve() to connect Runtime to its real local MCP broker before starting a turn.
        assert_eq!(fixture.api("GET", "/api/state", None).await.0, 200);
        fixture
    }

    fn set_time(&self, value: &str) {
        *self.current.lock().unwrap() = time(value);
    }

    fn bot_id(&self) -> String {
        self.store.snapshot().bots[0].id.clone()
    }

    async fn conversation(&self, bot: &str) -> String {
        self.store.mutate(|state| state.add_conversation(bot)).await.unwrap().id
    }

    async fn routine(&self, prompt: &str, schedule: RoutineSchedule) -> Routine {
        let bot_id = self.bot_id();
        self.store
            .mutate(|state| state.add_routine(RoutineInput { bot_id, name: prompt.into(), prompt: prompt.into(), schedule, enabled: true }))
            .await
            .unwrap()
    }

    async fn api(&self, method: &str, route: &str, body: Option<Value>) -> (u16, Value) {
        let origin = format!("http://127.0.0.1:{}", self.port);
        let headers = vec![
            ("Host".into(), format!("127.0.0.1:{}", self.port)),
            ("Origin".into(), origin),
            ("X-Grok-Bot".into(), "1".into()),
            ("Content-Type".into(), "application/json".into()),
        ];
        let body = body.map(|value| value.to_string());
        let (status, text) = tokio::time::timeout(TIMEOUT, common::raw(self.port, method, route, &headers, body.as_deref())).await.expect("HTTP response");
        (status, serde_json::from_str(&text).unwrap())
    }

    fn get(&self, id: &str) -> Conversation {
        self.store.read(|state| state.conversation(id).unwrap().clone())
    }

    async fn terminal(&self, id: &str, status: RunStatus) {
        let mut changes = self.store.subscribe();
        let conversation = tokio::time::timeout(TIMEOUT, async {
            loop {
                let conversation = self.get(id);
                if conversation.run.status.is_terminal() {
                    return conversation;
                }
                changes.recv().await.expect("store state event");
            }
        })
        .await
        .unwrap_or_else(|_| panic!("waiting for terminal {id}: {:?}", self.get(id).run));
        assert_eq!(conversation.run.status, status, "{:?}", conversation.run);
    }

    async fn persisted(&self) -> DiskState {
        self.store.flush().await.unwrap();
        serde_json::from_slice(&std::fs::read(self.data.join("state.json")).unwrap()).unwrap()
    }

    async fn stop_backend(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.send(()).unwrap();
        }
        if let Some(server) = self.server.take() {
            tokio::time::timeout(TIMEOUT, server).await.expect("shutdown must not wait for provider replies").unwrap().unwrap();
        }
    }

    async fn close(mut self) {
        self.stop_backend().await;
        self.provider.release_all();
        self.provider_server.abort();
        assert!(self.provider_server.await.unwrap_err().is_cancelled());
    }
}

fn assert_queued(disk: &DiskState, run: &RoutineRun) {
    let persisted = disk.routine_runs.iter().find(|item| item.id == run.id).unwrap();
    let conversation = disk.conversations.iter().find(|item| item.id == run.conversation_id).unwrap();
    assert_eq!(conversation.run.status, RunStatus::Idle);
    assert!(conversation.messages.is_empty());
    assert!(persisted.execution_run_id.is_none());
    assert!(persisted.result.is_none());
}

fn assert_result(disk: &DiskState, run: &RoutineRun, status: RoutineResultStatus) {
    let persisted = disk.routine_runs.iter().find(|item| item.id == run.id).unwrap();
    let conversation = disk.conversations.iter().find(|item| item.id == run.conversation_id).unwrap();
    let result = persisted.result.as_ref().expect("durable frozen routine result");
    assert_eq!(result.status, status);
    let conversation_status = match status {
        RoutineResultStatus::Completed => RunStatus::Completed,
        RoutineResultStatus::Cancelled => RunStatus::Cancelled,
        RoutineResultStatus::Error => RunStatus::Error,
        RoutineResultStatus::Interrupted => RunStatus::Interrupted,
    };
    assert_eq!(conversation.run.status, conversation_status);
    assert_eq!(result.finished_at, conversation.run.finished_at.as_deref().unwrap());
    assert!(result.error.is_none(), "{result:?}");
    if let Some(execution) = &persisted.execution_run_id {
        assert_eq!(execution, &conversation.run.id);
        assert!(result.started_at.is_some());
        assert_eq!(conversation.messages.len(), 2, "exactly one user/assistant turn");
        assert_eq!(conversation.messages[0].text, run.prompt);
        if status == RoutineResultStatus::Completed {
            assert_eq!(conversation.messages[1].text, format!("Completed: {}", run.prompt));
        }
    } else {
        assert_eq!(status, RoutineResultStatus::Cancelled);
        assert!(result.started_at.is_none());
        assert!(conversation.messages.is_empty(), "cancelled queued work never reached Runtime");
    }
}

fn assert_provider_claims(provider: &Provider) {
    for request in provider.requests() {
        let active: Vec<_> = request.persisted.conversations.iter().filter(|conversation| conversation.run.status.is_active()).collect();
        assert_eq!(active.len(), 1, "exactly one globally active execution when provider sees {}", request.prompt);
        assert_eq!(active[0].messages[0].text, request.prompt);
        if let Some(claim) = request.persisted.routine_runs.iter().find(|run| run.conversation_id == active[0].id) {
            assert_eq!(claim.prompt, request.prompt);
            assert_eq!(claim.execution_run_id.as_deref(), Some(active[0].run.id.as_str()), "claim and execution were durable before inference");
            assert!(claim.result.is_none());
        }
    }
}

#[test]
fn scheduler_regressions() {
    // The engine's global environment must be ready before Tokio/engine threads exist.
    common::run("scheduler", async |root| {
        tokio::time::timeout(TIMEOUT, global_serialization_and_fifo(&root)).await.expect("global FIFO scenario");
        tokio::time::timeout(TIMEOUT, manual_scheduled_races(&root)).await.expect("manual/scheduled race scenarios");
        tokio::time::timeout(TIMEOUT, duplicate_requests_and_completed_claim(&root)).await.expect("duplicate admission scenarios");
        tokio::time::timeout(TIMEOUT, cancellation_skips_queued_work(&root)).await.expect("cancellation scenario");
        tokio::time::timeout(TIMEOUT, shutdown_preserves_queue_and_rejects_waiting_commands(&root)).await.expect("shutdown scenario");
        tokio::time::timeout(TIMEOUT, automatic_dispatch_and_repeated_start(&root)).await.expect("automatic dispatcher scenario");
    });
}

/// Single-slot configuration: overlapping ticks, capacity contention, recurring phase and FIFO.
async fn global_serialization_and_fifo(root: &Path) {
    let f = Fixture::new(root, "fifo", false, false).await;
    let other_bot =
        f.store.mutate(|state| state.add_bot(BotInput { name: "Other bot".into(), color: None, instructions: None, shape: None, pinned: None })).await.unwrap();
    let blocker = f.conversation(&other_bot.id).await;
    assert_eq!(f.api("POST", &format!("/api/conversations/{blocker}/messages"), Some(json!({"text": "global blocker"}))).await.0, 202);
    f.provider.wait_for(1).await;
    // Define in the opposite order to admission: dispatch must order claims, not routine definitions.
    let scheduled = f.routine("scheduled last", once()).await;
    let second = f.routine("same-time second", RoutineSchedule::Interval { minutes: 5 }).await;
    let first = f.routine("oldest first", RoutineSchedule::Interval { minutes: 5 }).await;
    let first_run = f.scheduler.run(&first.id).await.unwrap();
    let second_run = f.scheduler.run(&second.id).await.unwrap();
    f.set_time("2026-01-01T00:00:59.999Z");
    f.scheduler.tick().await.unwrap();
    assert_eq!(f.store.snapshot().routine_runs.len(), 2);
    f.set_time(DUE);
    for result in futures::future::join_all((0..20).map(|_| f.scheduler.tick())).await {
        result.unwrap();
    }
    let disk = f.persisted().await;
    assert_eq!(disk.routine_runs.len(), 3, "overlapping ticks claim a due one-time routine once");
    assert_eq!(disk.routines.iter().find(|routine| routine.id == first.id).unwrap().next_run_at.as_deref(), Some("2026-01-01T00:05:00.000Z"));
    let scheduled_run = disk.routine_runs.iter().find(|run| run.routine_id == scheduled.id).unwrap().clone();
    assert_eq!(scheduled_run.trigger, RoutineTrigger::Scheduled);
    assert_eq!(scheduled_run.scheduled_for, "2026-01-01T00:01:00.000Z");
    for run in [&first_run, &second_run, &scheduled_run] {
        assert_queued(&disk, run);
    }
    assert_eq!(f.provider.prompts(), ["global blocker"], "queued work cannot prompt while another bot runs");
    assert_eq!(f.api("POST", &format!("/api/conversations/{}/messages", first_run.conversation_id), Some(json!({"text": "foreign message"}))).await.0, 409);
    f.provider.release(0);
    f.terminal(&blocker, RunStatus::Completed).await;
    for (index, run) in [&first_run, &second_run, &scheduled_run].into_iter().enumerate() {
        f.scheduler.tick().await.unwrap();
        f.provider.wait_for(index + 2).await;
        assert_eq!(f.provider.prompts()[index + 1], run.prompt);
        for result in futures::future::join_all((0..20).map(|_| f.scheduler.tick())).await {
            result.unwrap();
        }
        assert_eq!(f.provider.requests().len(), index + 2, "ticks cannot double-start an active turn");
        assert_eq!(f.store.read(|state| state.disk().conversations.iter().filter(|conversation| conversation.run.status.is_active()).count()), 1);
        f.provider.release(index + 1);
        f.terminal(&run.conversation_id, RunStatus::Completed).await;
    }
    f.scheduler.tick().await.unwrap();
    assert_eq!(f.provider.prompts(), ["global blocker", "oldest first", "same-time second", "scheduled last"]);
    let disk = f.persisted().await;
    for run in [&first_run, &second_run, &scheduled_run] {
        assert_result(&disk, run, RoutineResultStatus::Completed);
    }
    assert_provider_claims(&f.provider);
    f.close().await;
}

/// routines.test.ts: both race orderings, also after the winning command has fully completed.
async fn manual_scheduled_races(root: &Path) {
    for immediate in [false, true] {
        for manual_first in [true, false] {
            let f = Fixture::new(root, &format!("race-{manual_first}-{immediate}"), immediate, false).await;
            let routine = f.routine("one-time race", once()).await;
            f.set_time(DUE);
            let release = Notify::new();
            let queue = f.queue.clone();
            let scheduler = f.scheduler.clone();
            let holding = queue.run(release.notified());
            let manual = scheduler.run(&routine.id);
            let tick = scheduler.tick();
            tokio::pin!(holding, manual, tick);
            assert!(futures::poll!(holding.as_mut()).is_pending());
            // Polling once registers each waiter with Tokio's FIFO mutex, without timing sleeps.
            if manual_first {
                assert!(futures::poll!(manual.as_mut()).is_pending());
                assert!(futures::poll!(tick.as_mut()).is_pending());
            } else {
                assert!(futures::poll!(tick.as_mut()).is_pending());
                assert!(futures::poll!(manual.as_mut()).is_pending());
            }
            release.notify_one();
            holding.await;
            let run = if manual_first {
                let run = manual.await.unwrap();
                f.provider.wait_for(1).await;
                if immediate {
                    f.terminal(&run.conversation_id, RunStatus::Completed).await;
                }
                tick.await.unwrap();
                run
            } else {
                tick.await.unwrap();
                let run = f.store.snapshot().routine_runs[0].clone();
                f.provider.wait_for(1).await;
                if immediate {
                    f.terminal(&run.conversation_id, RunStatus::Completed).await;
                }
                assert_eq!(manual.await.unwrap_err().status, 409, "stale manual admission loses even after the scheduled winner finishes");
                run
            };
            assert_eq!(run.trigger, if manual_first { RoutineTrigger::Manual } else { RoutineTrigger::Scheduled });
            if !immediate {
                f.provider.release(0);
            }
            f.terminal(&run.conversation_id, RunStatus::Completed).await;
            f.scheduler.tick().await.unwrap();
            let disk = f.persisted().await;
            assert_eq!(disk.routine_runs.len(), 1);
            assert!(!disk.routines[0].enabled);
            assert!(disk.routines[0].next_run_at.is_none());
            assert_eq!(f.provider.prompts(), [routine.prompt]);
            assert_result(&disk, &run, RoutineResultStatus::Completed);
            assert_provider_claims(&f.provider);
            f.close().await;
        }
    }
}

/// Duplicate routine claims still conflict; distinct user requests share a durable Bot FIFO.
async fn duplicate_requests_and_completed_claim(root: &Path) {
    let f = Fixture::new(root, "duplicates", false, false).await;
    let routine = f.routine("duplicate routine", RoutineSchedule::Interval { minutes: 5 }).await;
    let route = format!("/api/routines/{}/run", routine.id);
    let (first, second) = tokio::join!(f.api("POST", &route, Some(json!({}))), f.api("POST", &route, Some(json!({}))));
    let mut statuses = [first.0, second.0];
    statuses.sort();
    assert_eq!(statuses, [202, 409]);
    f.provider.wait_for(1).await;
    let run = f.store.snapshot().routine_runs[0].clone();
    assert_eq!(f.store.snapshot().routine_runs.len(), 1);
    f.provider.release(0);
    f.terminal(&run.conversation_id, RunStatus::Completed).await;

    let a = f.conversation(&f.bot_id()).await;
    let b = f.conversation(&f.bot_id()).await;
    let a_route = format!("/api/conversations/{a}/messages");
    let b_route = format!("/api/conversations/{b}/messages");
    let (a_result, b_result) =
        tokio::join!(f.api("POST", &a_route, Some(json!({"text": "duplicate start A"}))), f.api("POST", &b_route, Some(json!({"text": "duplicate start B"}))),);
    let mut statuses = [a_result.0, b_result.0];
    statuses.sort();
    assert_eq!(statuses, [202, 202], "distinct accepted user requests are never rejected for a busy Bot");
    f.provider.wait_for(2).await;
    let (winner, loser) = if !f.get(&a).messages.is_empty() { (&a, &b) } else { (&b, &a) };
    assert!(f.get(loser).messages.is_empty());
    let queued = f.store.snapshot().work_queue.into_iter().find(|work| &work.conversation_id == loser).unwrap();
    assert_eq!(f.api("POST", &format!("/api/conversations/{winner}/messages"), Some(json!({"text": "same conversation duplicate"}))).await.0, 202);
    let route = |conversation: &str, id: &str| format!("/api/conversations/{conversation}/queued-turns/{id}/cancel");
    assert_eq!(f.api("POST", &route(loser, &queued.id), Some(json!({}))).await.0, 200);
    let pending = f.store.snapshot().work_queue.into_iter().find(|work| &work.conversation_id == winner).unwrap();
    assert_eq!(f.api("POST", &route(winner, &pending.id), Some(json!({}))).await.0, 200);
    f.provider.release(1);
    f.terminal(winner, RunStatus::Completed).await;
    f.scheduler.tick().await.unwrap();
    assert_eq!(f.provider.requests().len(), 2, "cancelled queued inputs never reach inference");
    assert_eq!(f.get(winner).messages.len(), 2);
    assert_result(&f.persisted().await, &run, RoutineResultStatus::Completed);
    assert_provider_claims(&f.provider);
    f.close().await;

    // Hold the second command until the first execution is terminal: this exercises claim_now's
    // observed-claim check, rather than passing solely because the store sees an active run.
    let f = Fixture::new(root, "duplicate-completed", true, false).await;
    let routine = f.routine("fast duplicate", RoutineSchedule::Interval { minutes: 5 }).await;
    let release = Notify::new();
    let queue = f.queue.clone();
    let scheduler = f.scheduler.clone();
    let holding = queue.run(release.notified());
    let first = scheduler.run(&routine.id);
    let duplicate = scheduler.run(&routine.id);
    tokio::pin!(holding, first, duplicate);
    assert!(futures::poll!(holding.as_mut()).is_pending());
    assert!(futures::poll!(first.as_mut()).is_pending());
    assert!(futures::poll!(duplicate.as_mut()).is_pending());
    release.notify_one();
    holding.await;
    let run = first.await.unwrap();
    f.terminal(&run.conversation_id, RunStatus::Completed).await;
    assert_eq!(duplicate.await.unwrap_err().status, 409);
    f.scheduler.tick().await.unwrap();
    assert_eq!(f.provider.prompts(), [routine.prompt]);
    let disk = f.persisted().await;
    assert_eq!(disk.routine_runs.len(), 1);
    assert_result(&disk, &run, RoutineResultStatus::Completed);
    f.close().await;
}

/// routines-api.test.ts: queued cancellation never prompts; active cancellation unblocks dispatch.
async fn cancellation_skips_queued_work(root: &Path) {
    let f = Fixture::new(root, "cancellation", false, false).await;
    let blocker = f.conversation(&f.bot_id()).await;
    assert_eq!(f.api("POST", &format!("/api/conversations/{blocker}/messages"), Some(json!({"text": "cancel blocker"}))).await.0, 202);
    f.provider.wait_for(1).await;
    let first = f.routine("cancel active routine", once()).await;
    let second = f.routine("never dispatch cancelled queue", once()).await;
    let active = f.scheduler.run(&first.id).await.unwrap();
    let queued = f.scheduler.run(&second.id).await.unwrap();
    assert_eq!(f.api("POST", &format!("/api/conversations/{}/cancel", queued.conversation_id), Some(json!({}))).await.0, 200);
    assert_result(&f.persisted().await, &queued, RoutineResultStatus::Cancelled);
    assert_eq!(f.api("POST", &format!("/api/conversations/{blocker}/cancel"), Some(json!({}))).await.0, 200);
    f.provider.release(0);
    f.terminal(&blocker, RunStatus::Cancelled).await;
    // Stopping the blocker must not let ordinary chat steal a still-reserved queued conversation.
    assert_eq!(
        f.api("POST", &format!("/api/conversations/{}/messages", active.conversation_id), Some(json!({"text": "foreign ordinary message"}))).await.0,
        409
    );
    f.scheduler.tick().await.unwrap();
    f.provider.wait_for(2).await;
    assert_eq!(f.provider.prompts(), ["cancel blocker", "cancel active routine"]);
    assert_eq!(f.api("POST", &format!("/api/conversations/{}/cancel", active.conversation_id), Some(json!({}))).await.0, 200);
    f.provider.release(1);
    f.terminal(&active.conversation_id, RunStatus::Cancelled).await;
    f.scheduler.tick().await.unwrap();
    assert_eq!(f.provider.requests().len(), 2);
    let disk = f.persisted().await;
    assert_eq!(disk.routine_runs.len(), 2);
    assert_result(&disk, &active, RoutineResultStatus::Cancelled);
    assert_result(&disk, &queued, RoutineResultStatus::Cancelled);
    assert_provider_claims(&f.provider);
    f.close().await;
}

/// Close with a provider request, durable queued work, a waiting tick, and a waiting manual claim.
async fn shutdown_preserves_queue_and_rejects_waiting_commands(root: &Path) {
    let mut f = Fixture::new(root, "shutdown", false, false).await;
    let active_routine = f.routine("shutdown in flight", once()).await;
    let queued_routine = f.routine("recover durable queue", once()).await;
    let rejected_routine = f.routine("must not claim at shutdown", once()).await;
    let active = f.scheduler.run(&active_routine.id).await.unwrap();
    f.provider.wait_for(1).await;
    let queued = f.scheduler.run(&queued_routine.id).await.unwrap();
    assert_queued(&f.persisted().await, &queued);
    {
        let queue = f.queue.clone();
        let scheduler = f.scheduler.clone();
        let release = Notify::new();
        let holding = queue.run(release.notified());
        let tick = scheduler.tick();
        let manual = scheduler.run(&rejected_routine.id);
        let closing = scheduler.close();
        tokio::pin!(holding, tick, manual, closing);
        assert!(futures::poll!(holding.as_mut()).is_pending());
        assert!(futures::poll!(tick.as_mut()).is_pending());
        assert!(futures::poll!(manual.as_mut()).is_pending());
        assert!(futures::poll!(closing.as_mut()).is_pending(), "close must wait for the in-flight tick");
        f.stop.take().unwrap().send(()).unwrap();
        release.notify_one();
        holding.await;
        tick.await.unwrap();
        assert_eq!(manual.await.unwrap_err().status, 503);
        closing.await;
    }
    f.stop_backend().await;
    let disk = f.persisted().await;
    assert_eq!(f.provider.prompts(), ["shutdown in flight"], "shutdown cancels active inference without dispatching the queue");
    assert_eq!(disk.routine_runs.len(), 2, "waiting manual claim was never admitted");
    assert_result(&disk, &active, RoutineResultStatus::Interrupted);
    assert_queued(&disk, &queued);
    f.set_time("2026-01-02T00:00:00Z");
    f.scheduler.start();
    f.scheduler.tick().await.unwrap();
    assert_eq!(f.scheduler.run(&rejected_routine.id).await.unwrap_err().status, 503);
    assert_eq!(f.persisted().await.routine_runs.len(), 2, "closed scheduler cannot claim future deadlines");
    f.close().await;

    let restarted = Fixture::new(root, "shutdown", false, false).await;
    assert_queued(&restarted.persisted().await, &queued);
    restarted.scheduler.tick().await.unwrap();
    restarted.provider.wait_for(1).await;
    assert_eq!(restarted.provider.prompts(), ["recover durable queue"], "started work is not replayed after restart");
    restarted.provider.release(0);
    restarted.terminal(&queued.conversation_id, RunStatus::Completed).await;
    restarted.scheduler.tick().await.unwrap();
    let disk = restarted.persisted().await;
    assert_eq!(disk.routine_runs.len(), 2);
    assert_result(&disk, &active, RoutineResultStatus::Interrupted);
    assert_result(&disk, &queued, RoutineResultStatus::Completed);
    assert_provider_claims(&restarted.provider);
    restarted.close().await;
}

/// routines-api.test.ts: listening starts polling; repeated start calls do not duplicate dispatch.
async fn automatic_dispatch_and_repeated_start(root: &Path) {
    let f = Fixture::new(root, "automatic", false, true).await;
    let first = f.routine("automatic first", once()).await;
    let second = f.routine("automatic second", once()).await;
    f.set_time(DUE);
    let (a, b) = (f.scheduler.clone(), f.scheduler.clone());
    let (a, b) = tokio::join!(tokio::spawn(async move { a.start() }), tokio::spawn(async move { b.start() }));
    a.unwrap();
    b.unwrap();
    f.provider.wait_for(1).await;
    assert_eq!(f.provider.prompts(), [first.prompt]);
    let disk = f.persisted().await;
    assert_eq!(disk.routine_runs.len(), 2);
    let first_run = disk.routine_runs.iter().find(|run| run.routine_id == first.id).unwrap().clone();
    let second_run = disk.routine_runs.iter().find(|run| run.routine_id == second.id).unwrap().clone();
    assert_queued(&disk, &second_run);
    f.provider.release(0);
    f.terminal(&first_run.conversation_id, RunStatus::Completed).await;
    // No explicit tick: automatic dispatch must advance after a real runtime turn settles.
    f.provider.wait_for(2).await;
    assert_eq!(f.provider.prompts(), ["automatic first", "automatic second"]);
    f.provider.release(1);
    f.terminal(&second_run.conversation_id, RunStatus::Completed).await;
    f.scheduler.close().await;
    let disk = f.persisted().await;
    assert_result(&disk, &first_run, RoutineResultStatus::Completed);
    assert_result(&disk, &second_run, RoutineResultStatus::Completed);
    assert_provider_claims(&f.provider);
    f.close().await;
}
