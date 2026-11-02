//! Cancellation uses real store persistence and finish/project paths, without starting the engine.
//! Explicit barriers keep queue, broker, dispatch, and failed-writer boundaries deterministic.

use super::*;
use crate::computer::{ComputerObservation, ComputerScreenshot, ComputerService, ComputerState, ComputerStatus, VIEWPORT};
use futures::future::BoxFuture;
use grok_bot_model::{Conversation, WorkerBatch, system_clock};
use tokio::task::JoinHandle;
use tower::ServiceExt;

struct Fixture {
    _directory: tempfile::TempDir,
    store: Arc<Store>,
    runtime: Arc<Runtime>,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path(), system_clock()).await.unwrap();
        let runtime = Runtime::new(Arc::clone(&store), CommandQueue::new(), RuntimeOptions::new(directory.path().join("engine")));
        store
            .mutate(|state| {
                state.set_runtime(RuntimeInfo { available: true, version: None, error: None });
                Ok(())
            })
            .await
            .unwrap();
        Self { _directory: directory, store, runtime }
    }

    fn router(&self) -> axum::Router {
        let scheduler = crate::scheduler::RoutineScheduler::new(
            Arc::clone(&self.store),
            Arc::clone(&self.runtime),
            self.runtime.queue.clone(),
            crate::scheduler::SchedulerOptions { now: system_clock(), poll_interval: Duration::from_secs(3600), auto_start: false },
        );
        crate::http::router(Arc::new(crate::http::HttpContext {
            store: Arc::clone(&self.store),
            runtime: Arc::clone(&self.runtime),
            scheduler,
            queue: self.runtime.queue.clone(),
            allowed_hosts: crate::http::default_allowed_hosts(&[]),
            port: std::sync::atomic::AtomicU16::new(3000),
            closing: AtomicBool::new(false),
            web_root: None,
            mcp_test_stopping: watch::Sender::new(false),
            stopping: watch::Sender::new(false),
        }))
    }

    async fn run(&self, child: Option<(&str, &str)>) -> Arc<Run> {
        let bot = self.store.read(|state| state.disk().bots[0].clone());
        let conversation_id = match child {
            Some((id, _)) => id.to_owned(),
            None => self.store.mutate(|state| state.add_conversation(&bot.id)).await.unwrap().id,
        };
        let (run_id, message_id) = (new_id(), new_id());
        let now = iso(self.store.now());
        self.store
            .mutate(|state| {
                let task = state.worker_for_conversation(&conversation_id).map(|(_, worker)| worker.task.clone()).unwrap_or_else(|| "A user task".into());
                let conversation = state.conversation_mut(&conversation_id)?;
                let message = |id, role, text| Message {
                    id,
                    role,
                    text,
                    created_at: now.clone(),
                    tools: Vec::new(),
                    bot_exchange_ids: None,
                    source_bot_id: None,
                    speaker_bot_id: None,
                    attachments: None,
                    report_delivery_ids: None,
                };
                conversation.messages = vec![message(new_id(), Role::User, task), message(message_id.clone(), Role::Assistant, String::new())];
                conversation.run = RunState { id: run_id.clone(), status: RunStatus::Running, started_at: Some(now.clone()), ..RunState::idle() };
                if let Some((_, worker_id)) = child {
                    state.bind_worker_execution(worker_id, &run_id)?;
                }
                Ok(())
            })
            .await
            .unwrap();
        let run = Arc::new(Run {
            conversation_id,
            run_id,
            message_id,
            settings: self.store.read(|state| state.settings().clone()),
            bot,
            token: new_id(),
            fingerprint: String::new(),
            history: Vec::new(),
            attachments: Vec::new(),
            speaker: None,
            fresh_prompt: None,
            loaded_prompt: None,
            extra_rules: None,
            report_deliveries: Vec::new(),
            worker_id: child.map(|(_, id)| id.to_owned()),
            worker_deadline: None,
            worker_report: None,
            keys: Vec::new(),
            mcp: custom_mcp::RunMcp::new(Vec::new(), Vec::new()),
            flags: Mutex::new(RunFlags { session_id: Some(new_id()), prompting: true, accepting_updates: true, ..RunFlags::default() }),
            activity: tokio::sync::Notify::new(),
            stop: watch::Sender::new(false),
            done: watch::Sender::new(false),
            broker: tokio::sync::Mutex::new(()),
        });
        self.runtime.runs().push(Arc::clone(&run));
        run
    }

    async fn batch(&self, parent: &Run) -> WorkerBatch {
        for task in ["Active worker", "Queued sibling"] {
            self.store.mutate(|state| state.spawn_worker(&parent.conversation_id, &parent.message_id, &parent.run_id, task)).await.unwrap();
        }
        self.store.read(|state| state.disk().worker_batches[0].clone())
    }

    fn get(&self, run: &Run) -> Conversation {
        self.store.read(|state| state.conversation(&run.conversation_id).unwrap().clone())
    }

    fn finish_when_stopped(&self, run: &Arc<Run>) -> JoinHandle<()> {
        let (runtime, run) = (Arc::clone(&self.runtime), Arc::clone(run));
        tokio::spawn(async move {
            run.stopped().await;
            runtime.finish(&run, Outcome::default()).await;
        })
    }
}

/// Fail atomic replacement without chmod assumptions (also works when tests run as root). Restore
/// the last successfully saved file on drop so recovery can distinguish durable state from memory.
struct BlockedWriter {
    path: PathBuf,
    saved: PathBuf,
}

impl BlockedWriter {
    fn new(store: &Store) -> Self {
        let path = store.data_dir().join("state.json");
        let saved = store.data_dir().join("state.cancellation-saved.json");
        assert!(path.is_absolute() && path.is_file() && !saved.exists());
        std::fs::rename(&path, &saved).unwrap();
        std::fs::create_dir(&path).unwrap();
        Self { path, saved }
    }
}

impl Drop for BlockedWriter {
    fn drop(&mut self) {
        assert!(self.path.is_absolute() && self.path.is_dir() && self.saved.is_file());
        std::fs::remove_dir(&self.path).unwrap();
        std::fs::rename(&self.saved, &self.path).unwrap();
    }
}

fn update(run: &Run, text: &str) -> acp::SessionNotification {
    serde_json::from_value(json!({
        "sessionId": run.flags().session_id,
        "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}
    }))
    .unwrap()
}

async fn stopped(run: &Run) {
    tokio::time::timeout(Duration::from_secs(2), run.stopped()).await.expect("Stop must be signaled without queue admission or a writable store");
    assert!(!run.flags().accepting_updates);
    assert!(run.flags().approvals.is_empty());
}

async fn settled(task: JoinHandle<()>) {
    tokio::time::timeout(Duration::from_secs(2), task).await.expect("stopped run must settle without a queue/broker deadlock").unwrap();
}

struct Browser {
    epoch: String,
    stopped: AtomicBool,
    statuses: std::sync::atomic::AtomicUsize,
    cancel_on_second_status: Mutex<Option<(Weak<Runtime>, Weak<Run>)>>,
}

impl Browser {
    fn new() -> Self {
        Self { epoch: new_id(), stopped: AtomicBool::new(false), statuses: std::sync::atomic::AtomicUsize::new(0), cancel_on_second_status: Mutex::new(None) }
    }
}

impl ComputerService for Browser {
    fn status(&self) -> ComputerStatus {
        if self.statuses.fetch_add(1, Ordering::SeqCst) == 1 {
            let hook = self.cancel_on_second_status.lock().unwrap().take();
            if let Some((runtime, run)) = hook {
                runtime.upgrade().unwrap().request_stop(&run.upgrade().unwrap());
            }
        }
        ComputerStatus {
            state: if self.stopped.load(Ordering::SeqCst) { ComputerState::Stopped } else { ComputerState::Running },
            epoch: self.epoch.clone(),
            frame_id: None,
            viewport: VIEWPORT,
            origins: Vec::new(),
            url: None,
            busy: false,
            sandboxed: true,
            error: None,
        }
    }
    fn start(&self, _: Value) -> BoxFuture<'_, HttpResult<ComputerStatus>> {
        unreachable!()
    }
    fn stop(&self) -> BoxFuture<'_, ComputerStatus> {
        self.stopped.store(true, Ordering::SeqCst);
        Box::pin(async { self.status() })
    }
    fn perform(&self, _: Value) -> BoxFuture<'_, HttpResult<ComputerObservation>> {
        unreachable!()
    }
    fn screenshot(&self, _: Value) -> BoxFuture<'_, HttpResult<ComputerScreenshot>> {
        unreachable!()
    }
}

#[tokio::test]
async fn stop_between_browser_approval_and_epoch_publication_prevents_the_operation() {
    let f = Fixture::new().await;
    let run = f.run(None).await;
    let browser = Arc::new(Browser::new());
    *browser.cancel_on_second_status.lock().unwrap() = Some((Arc::downgrade(&f.runtime), Arc::downgrade(&run)));
    f.runtime.set_computer(Arc::new(crate::computer::ApplicationComputer::with_service(Arc::clone(&f.store), browser)));
    let authorization = format!("Bearer {}", run.token);
    let call = f.runtime.mcp(
        &run.run_id,
        Some(&authorization),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "computer_observe", "arguments": {}}}),
    );
    tokio::pin!(call);
    assert!(futures::poll!(&mut call).is_pending());
    let permission = run.flags().approvals.front().unwrap().permission.clone();
    let allow = &permission.options.iter().find(|option| option.kind == "allow_once").unwrap().id;
    f.runtime.resolve(&run.conversation_id, &permission.id, allow, true).await.unwrap();
    // The fake's second status read signals Stop after the initial inactive check but before epoch
    // publication. Its perform() panics if called: no browser operation may cross that boundary.
    let response = tokio::time::timeout(Duration::from_secs(2), &mut call).await.unwrap().unwrap().unwrap();
    assert_eq!(response["result"]["isError"], true);
    assert!(run.flags().computer_epoch.is_none(), "a stopped turn must not acquire a browser epoch");
    stopped(&run).await;
    f.runtime.finish(&run, Outcome::default()).await;
}

#[tokio::test]
async fn stop_releases_approvals_and_computer_before_queue_admission_even_when_store_already_failed() {
    let f = Fixture::new().await;
    let run = f.run(None).await;
    let browser = Arc::new(Browser::new());
    f.runtime.set_computer(Arc::new(crate::computer::ApplicationComputer::with_service(Arc::clone(&f.store), browser.clone())));
    run.flags().computer_epoch = Some(browser.epoch.clone());
    let (resolve, decision) = oneshot::channel();
    run.flags().approvals.push_back(Approval {
        permission: PendingPermission {
            id: new_id(),
            title: "Pending approval".into(),
            kind: "other".into(),
            description: String::new(),
            options: Vec::new(),
            paths: Vec::new(),
            local_only: None,
        },
        resolve,
    });
    let blocked = BlockedWriter::new(&f.store);
    assert_eq!(
        f.store
            .mutate(|state| {
                state.disk_mut();
                Ok(())
            })
            .await
            .unwrap_err()
            .status,
        500
    );
    let (entered, queued) = oneshot::channel();
    let (release, wait) = oneshot::channel();
    let queue = f.runtime.queue.clone();
    let holder = tokio::spawn(async move {
        queue
            .run(async {
                entered.send(()).unwrap();
                wait.await.unwrap();
            })
            .await
    });
    queued.await.unwrap();
    let finish = f.finish_when_stopped(&run);
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/conversations/{}/cancel", run.conversation_id))
        .header("host", "127.0.0.1:3000")
        .header("origin", "http://127.0.0.1:3000")
        .header("content-type", "application/json")
        .header("x-grok-bot", "1")
        .extension(axum::extract::ConnectInfo("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap()))
        .body(axum::body::Body::from("{}"))
        .unwrap();
    let router = f.router();
    let cancel = tokio::spawn(async move { router.oneshot(request).await.unwrap() });
    stopped(&run).await;
    assert_eq!(decision.await.unwrap(), None);
    // The cancellation task stops this epoch before it blocks on the held command queue.
    tokio::time::timeout(Duration::from_secs(2), async {
        while !browser.stopped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!cancel.is_finished());
    release.send(()).unwrap();
    settled(holder).await;
    let response = tokio::time::timeout(Duration::from_secs(2), cancel).await.unwrap().unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
    settled(finish).await;
    assert!(*run.done.borrow() && f.runtime.runs().is_empty());
    assert!(!f.store.snapshot().runtime.available);
    drop(blocked);
}

#[tokio::test]
async fn failed_cascade_still_stops_active_child_and_waits_out_a_queued_broker_without_losing_first_error() {
    let f = Fixture::new().await;
    let parent = f.run(None).await;
    let batch = f.batch(&parent).await;
    f.runtime.finish(&parent, Outcome::default()).await;
    let child = f.run(Some((&batch.workers[0].conversation_id, &batch.workers[0].id))).await;
    let blocked = BlockedWriter::new(&f.store);
    let (entered, held) = oneshot::channel();
    let (runtime, run) = (Arc::clone(&f.runtime), Arc::clone(&child));
    let broker = tokio::spawn(async move {
        let _call = run.broker.lock().await;
        entered.send(()).unwrap();
        run.stopped().await;
        runtime
            .queue
            .run(async {
                assert!(run.inactive(), "a tool queued before Stop cannot mutate afterwards");
            })
            .await;
    });
    held.await.unwrap();
    let finish = f.finish_when_stopped(&child);
    let error = tokio::time::timeout(Duration::from_secs(2), f.runtime.cancel(&parent.conversation_id)).await.unwrap().unwrap_err();
    assert_eq!(error.status, 500, "preserve the failed cascade's first-write error, not a cleanup 503");
    assert!(*child.done.borrow(), "even an error response must wait for live-run cleanup");
    stopped(&child).await;
    settled(broker).await;
    settled(finish).await;
    assert!(*child.done.borrow() && f.runtime.runs().is_empty());
    let batch = f.store.read(|state| state.worker_batch(&batch.id).unwrap().clone());
    assert!(batch.cancelled && batch.workers[1].status == DeliveryStatus::Cancelled && batch.report.is_none());
    let before = (f.get(&child).messages, f.store.snapshot().revision);
    f.runtime.project(update(&child, "Late engine callback must be ignored")).await;
    assert_eq!((f.get(&child).messages, f.store.snapshot().revision), before);
    drop(blocked);
    let recovered = Store::open(f.store.data_dir(), system_clock()).await.unwrap();
    let recovered = recovered.read(|state| state.worker_batch(&batch.id).unwrap().clone());
    assert!(!recovered.cancelled, "the returned error must not imply an unsaved Stop survives restart");
    assert_eq!(recovered.workers[0].status, DeliveryStatus::Interrupted, "started work is never replayed");
    assert_eq!(recovered.workers[1].status, DeliveryStatus::Queued, "unsaved cancellation cannot discard durable queued work");
}

#[tokio::test]
async fn failed_project_immediately_signals_parent_or_worker_and_rejects_late_callbacks() {
    for worker in [false, true] {
        let f = Fixture::new().await;
        let parent = f.run(None).await;
        let batch = f.batch(&parent).await;
        let run = if worker {
            f.runtime.finish(&parent, Outcome::default()).await;
            f.run(Some((&batch.workers[0].conversation_id, &batch.workers[0].id))).await
        } else {
            Arc::clone(&parent)
        };
        let blocked = BlockedWriter::new(&f.store);
        f.runtime.project(update(&run, "The update that cannot be saved")).await;
        assert!(*run.stop.borrow(), "project must send the stop watch, not merely set a flag");
        stopped(&run).await;
        let before = (f.get(&run).messages, f.store.snapshot().revision);
        f.runtime.project(update(&run, "No updates after the persistence failure")).await;
        assert_eq!((f.get(&run).messages, f.store.snapshot().revision), before);
        let finish = f.finish_when_stopped(&run);
        let error = tokio::time::timeout(Duration::from_secs(2), f.runtime.cancel(&parent.conversation_id)).await.unwrap().unwrap_err();
        assert_eq!(error.status, 503);
        settled(finish).await;
        assert!(*run.done.borrow() && f.runtime.runs().is_empty());
        assert!(!f.store.snapshot().runtime.available);
        drop(blocked);
    }
}

#[tokio::test]
async fn stop_resolves_a_child_admitted_under_the_queue_and_durable_cancellation_is_not_replayed() {
    let f = Fixture::new().await;
    let parent = f.run(None).await;
    let batch = f.batch(&parent).await;
    f.runtime.finish(&parent, Outcome::default()).await;
    let (entered, held) = oneshot::channel();
    let (release, wait) = oneshot::channel();
    let queue = f.runtime.queue.clone();
    let holder = tokio::spawn(async move {
        queue
            .run(async {
                entered.send(()).unwrap();
                wait.await.unwrap();
            })
            .await
    });
    held.await.unwrap();
    let cancel = f.runtime.cancel(&parent.conversation_id);
    tokio::pin!(cancel);
    assert!(futures::poll!(&mut cancel).is_pending(), "Stop is waiting for dispatch's queue turn");
    // Model a dispatcher which already owns the queue and admits a child after Stop's first read.
    let child = f.run(Some((&batch.workers[0].conversation_id, &batch.workers[0].id))).await;
    let finish = f.finish_when_stopped(&child);
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), &mut cancel).await.unwrap().unwrap();
    settled(holder).await;
    settled(finish).await;
    stopped(&child).await;
    let recovered = Store::open(f.store.data_dir(), system_clock()).await.unwrap();
    let batch = recovered.read(|state| state.worker_batch(&batch.id).unwrap().clone());
    assert!(batch.cancelled && batch.workers.iter().all(|worker| worker.status == DeliveryStatus::Cancelled));
    assert!(batch.report.is_none());
    assert!(recovered.read(|state| state.active().is_none()));
}

#[tokio::test]
async fn cancellation_does_not_report_success_when_only_the_final_run_save_fails() {
    let f = Fixture::new().await;
    let run = f.run(None).await;
    let blocked = BlockedWriter::new(&f.store);
    let finish = f.finish_when_stopped(&run);
    // With no queued work/batches, the cascade is a no-op. The failed write comes from finish().
    let error = tokio::time::timeout(Duration::from_secs(2), f.runtime.cancel(&run.conversation_id)).await.unwrap().unwrap_err();
    assert_eq!(error.status, 503);
    settled(finish).await;
    assert!(*run.done.borrow());
    assert_eq!(f.runtime.cancel(&new_id()).await.unwrap_err().status, 503, "preserve failed-store semantics");
    drop(blocked);
}
