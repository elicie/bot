//! Workers and their grouped reports use the same durable per-Bot FIFO as ordinary messages.
//! Four gated scenarios share one isolated executable invocation; all admission/approval/Stop
//! commands use a listening Backend and its live scheduler, never forged active conversations.

use crate::{common, scripted_provider};

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use grok_bot_core::app::{Backend, BackendOptions};
use grok_bot_core::runtime::{Runtime, RuntimeOptions};
use grok_bot_core::scheduler::{RoutineScheduler, SchedulerOptions};
use grok_bot_model::{Conversation, DeliveryStatus, DiskState, ReportStatus, RunStatus, SettingsInput, WorkKind, WorkerBatch, system_clock};
use grok_bot_store::Store;
use scripted_provider::{Reply, ScriptedProvider};
use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};

const TIMEOUT: Duration = Duration::from_secs(45);
const WORKER_REPORT: &str = "Automatic worker follow-up from the app, not typed by the user:";
const BOT_REPORT: &str = "Automatic follow-up from the app, not typed by the user:";

struct Fixture {
    store: Arc<Store>,
    runtime: Arc<Runtime>,
    scheduler: Arc<RoutineScheduler>,
    provider: ScriptedProvider,
    provider_server: tokio::task::JoinHandle<()>,
    port: u16,
    stop: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Fixture {
    async fn new(root: &Path, name: &str, capacity: u32) -> Self {
        let data = root.join(name).join("data");
        let workspace = root.join(name).join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("notes.txt"), "FIFO_READ_ONLY_NOTES\n").unwrap();
        let provider = ScriptedProvider::new(data.join("state.json"));
        let (base_url, provider_server) = provider.serve().await;
        let backend = Backend::new(BackendOptions {
            data_dir: data,
            engine_root: root.join("engine"),
            web_root: None,
            embedded_web: None,
            allowed_hosts: vec![],
            now: None,
            scheduler: Some(SchedulerOptions { now: system_clock(), poll_interval: Duration::from_millis(20), auto_start: true }),
            runtime: Some(RuntimeOptions {
                startup_timeout: Duration::from_secs(30),
                prompt_idle_timeout: Duration::from_secs(120),
                cancel_deadline: Duration::from_millis(100),
                ..RuntimeOptions::new(root.join("engine"))
            }),
        })
        .await
        .unwrap();
        // Configure before starting the listener/scheduler, so no accepted claim can see stale
        // settings. Actual scenario admission and permission decisions go through HTTP below.
        backend
            .store
            .mutate(|state| {
                state.update_settings(SettingsInput {
                    base_url,
                    model: "scheduler-test-model".into(),
                    workspace: workspace.to_string_lossy().into_owned(),
                    api_key: None,
                    clear_api_key: Some(true),
                    max_concurrent_runs: Some(capacity),
                })
            })
            .await
            .unwrap();
        let (store, runtime, scheduler) = (backend.store.clone(), backend.runtime.clone(), backend.scheduler.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop, stopped) = oneshot::channel();
        let server = tokio::spawn(backend.serve(listener, async {
            let _ = stopped.await;
        }));
        let fixture = Self { store, runtime, scheduler, provider, provider_server, port, stop, server };
        assert_eq!(fixture.api("GET", "/api/state", None).await.0, 200);
        fixture
    }

    async fn api(&self, method: &str, route: &str, body: Option<Value>) -> (u16, Value) {
        let headers = vec![
            ("Host".into(), format!("127.0.0.1:{}", self.port)),
            ("Origin".into(), format!("http://127.0.0.1:{}", self.port)),
            ("X-Grok-Bot".into(), "1".into()),
            ("Content-Type".into(), "application/json".into()),
        ];
        let body = body.map(|body| body.to_string());
        let (status, text) = tokio::time::timeout(TIMEOUT, common::raw(self.port, method, route, &headers, body.as_deref())).await.expect("HTTP response");
        (status, serde_json::from_str(&text).unwrap_or_else(|error| panic!("{method} {route}: {error}: {text}")))
    }

    fn bot_id(&self) -> String {
        self.store.snapshot().bots[0].id.clone()
    }

    async fn bot(&self, name: &str) -> String {
        let (status, body) = self.api("POST", "/api/bots", Some(json!({"name": name}))).await;
        assert_eq!(status, 201, "{body}");
        body["bots"].as_array().unwrap().iter().find(|bot| bot["name"] == name).unwrap()["id"].as_str().unwrap().to_owned()
    }

    async fn conversation(&self, bot: &str) -> String {
        let (status, body) = self.api("POST", "/api/conversations", Some(json!({"botId": bot}))).await;
        assert_eq!(status, 201, "{body}");
        body["conversation"]["id"].as_str().unwrap().to_owned()
    }

    async fn send(&self, conversation: &str, text: &str) {
        let (status, body) = self.api("POST", &format!("/api/conversations/{conversation}/messages"), Some(json!({"text": text}))).await;
        assert_eq!(status, 202, "ordinary turns must enter the FIFO even while their Bot has worker work: {body}");
    }

    async fn approve(&self, conversation: &str) {
        let disk = self
            .wait("an actual broker permission", |disk| {
                let conversation = disk.conversations.iter().find(|item| item.id == conversation).unwrap();
                conversation.run.permission.is_some() || conversation.run.status.is_terminal()
            })
            .await;
        let conversation = disk.conversations.iter().find(|item| item.id == conversation).unwrap();
        let permission = conversation.run.permission.as_ref().unwrap_or_else(|| panic!("no approval: {:?}", conversation.run));
        let option = &permission.options.iter().find(|option| option.kind == "allow_once").unwrap().id;
        let (status, body) =
            self.api("POST", &format!("/api/conversations/{}/permissions/{}", conversation.id, permission.id), Some(json!({"optionId": option}))).await;
        assert_eq!(status, 200, "{body}");
    }

    fn disk(&self) -> DiskState {
        self.store.read(|state| state.disk().clone())
    }

    fn get(&self, id: &str) -> Conversation {
        self.store.read(|state| state.conversation(id).unwrap().clone())
    }

    fn batch(&self, parent: &str) -> WorkerBatch {
        self.store.read(|state| state.disk().worker_batches.iter().find(|batch| batch.parent_conversation_id == parent).unwrap().clone())
    }

    async fn wait(&self, what: &str, predicate: impl Fn(&DiskState) -> bool) -> DiskState {
        let mut changes = self.store.subscribe();
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let disk = self.disk();
                if predicate(&disk) {
                    return disk;
                }
                match changes.recv().await {
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => panic!("store closed waiting for {what}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("waiting for {what}: {:?}", self.disk()))
    }

    async fn finished(&self, id: &str, messages: usize, status: RunStatus) -> Conversation {
        let disk = self
            .wait("the expected terminal conversation", |disk| {
                disk.conversations
                    .iter()
                    .any(|conversation| conversation.id == id && conversation.messages.len() == messages && conversation.run.status.is_terminal())
            })
            .await;
        let conversation = disk.conversations.into_iter().find(|conversation| conversation.id == id).unwrap();
        assert_eq!(conversation.run.status, status, "{:?}", conversation.run);
        conversation
    }

    async fn reported(&self, parent: &str, messages: usize) -> WorkerBatch {
        self.wait("the single completed worker report", |disk| {
            disk.worker_batches
                .iter()
                .any(|batch| batch.parent_conversation_id == parent && batch.report.as_ref().is_some_and(|report| report.status == ReportStatus::Completed))
        })
        .await;
        self.finished(parent, messages, RunStatus::Completed).await;
        self.batch(parent)
    }

    fn assert_queue(&self, expected: &[(&str, &str, &str)]) {
        let disk = self.disk();
        let actual: Vec<_> = disk.work_queue.iter().map(|work| (work.bot_id.as_str(), work.conversation_id.as_str(), kind(&work.kind))).collect();
        assert_eq!(actual, expected, "{:?}", disk.work_queue);
    }

    fn assert_claims(&self, capacity: usize) {
        for request in self.provider.requests() {
            let active: Vec<_> = request.persisted.conversations.iter().filter(|conversation| conversation.run.status.is_active()).collect();
            assert!(!active.is_empty() && active.len() <= capacity, "durable claim before inference: {active:?}");
            let mut bots = BTreeSet::new();
            for conversation in active {
                assert_eq!(conversation.run.bot_id.as_deref(), Some(conversation.bot_id.as_str()), "worker and report claims retain their actual Bot owner");
                assert!(bots.insert(conversation.bot_id.clone()), "a Bot never receives two simultaneous execution slots");
            }
        }
    }

    async fn close(self) {
        self.scheduler.tick().await.unwrap();
        assert!(self.disk().work_queue.is_empty(), "no accepted work is stranded after settlement");
        self.provider.assert_drained();
        self.stop.send(()).unwrap();
        tokio::time::timeout(TIMEOUT, self.server).await.expect("backend shutdown").unwrap().unwrap();
        self.provider_server.abort();
        assert!(self.provider_server.await.unwrap_err().is_cancelled());
    }
}

fn kind(work: &WorkKind) -> &'static str {
    match work {
        WorkKind::User { .. } => "user",
        WorkKind::Worker { .. } => "worker",
        WorkKind::WorkerReport { .. } => "worker_report",
        WorkKind::Delivery { .. } => "delivery",
        WorkKind::Report { .. } => "report",
        _ => "other",
    }
}

fn spawn(task: &str) -> Reply {
    Reply::tool("spawn_worker", json!({"task": task}))
}

fn current_prompt(request: &Value) -> String {
    let query = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .filter(|message| message["role"] == "user")
        .map(|message| match &message["content"] {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts.iter().filter_map(|part| part["text"].as_str()).collect::<Vec<_>>().join("\n"),
            _ => String::new(),
        })
        .find(|text| text.contains("<user_query>"))
        .unwrap();
    let start = query.find("<user_query>\n").unwrap() + "<user_query>\n".len();
    let end = query.rfind("\n</user_query>").unwrap();
    query[start..end].rsplit("\n\nCurrent user message:\n").next().unwrap().to_owned()
}

#[test]
fn workers_and_reports_share_per_bot_fifo_and_conversation_stop() {
    common::run("worker-fifo", async |root| {
        tokio::time::timeout(Duration::from_secs(90), different_bot_workers_overlap(&root)).await.expect("two Bot workers overlap under cap two");
        eprintln!("[worker-fifo] two real Bot workers overlap under cap 2: passed");
        tokio::time::timeout(Duration::from_secs(90), user_worker_report_fifo(&root)).await.expect("ordinary/worker/report FIFO");
        eprintln!("[worker-fifo] User/worker/report FIFO and original sourceRequest: passed");
        tokio::time::timeout(Duration::from_secs(90), capacity_one_releases_parent(&root)).await.expect("cap one worker/report progress");
        eprintln!("[worker-fifo] cap 1 parent/worker/grouped-report progress: passed");
        tokio::time::timeout(Duration::from_secs(90), parent_stop_preserves_independent_work(&root)).await.expect("parent Stop owns only internal workers");
        eprintln!("[worker-fifo] parent Stop preserves unrelated users/Bot-message chain: passed");
    });
}

async fn different_bot_workers_overlap(root: &Path) {
    let f = Fixture::new(root, "worker-overlap", 2).await;
    let (alpha, beta) = (f.bot_id(), f.bot("Worker Beta").await);
    let (parent_a, parent_b) = (f.conversation(&alpha).await, f.conversation(&beta).await);
    f.provider.plan_contains("OVERLAP_PARENT_A", vec![spawn("OVERLAP_TASK_A"), Reply::text("A delegated.")]);
    f.provider.plan_contains("OVERLAP_PARENT_B", vec![spawn("OVERLAP_TASK_B"), Reply::text("B delegated.")]);
    f.provider.plan_contains("OVERLAP_TASK_A", vec![Reply::hold("worker-a", "A_WORKER_ANSWER")]);
    f.provider.plan_contains("OVERLAP_TASK_B", vec![Reply::hold("worker-b", "B_WORKER_ANSWER")]);
    f.provider.plan_prefix(WORKER_REPORT, vec![Reply::text("A_GROUPED_ANSWER"), Reply::text("B_GROUPED_ANSWER")]);
    f.send(&parent_a, "OVERLAP_PARENT_A").await;
    f.approve(&parent_a).await;
    f.provider.wait_for_gate("worker-a").await;
    f.send(&parent_b, "OVERLAP_PARENT_B").await;
    f.approve(&parent_b).await;
    f.provider.wait_for_gate("worker-b").await;
    let (a, b) = (f.batch(&parent_a), f.batch(&parent_b));
    let disk = f.disk();
    let active: BTreeSet<_> =
        disk.conversations.iter().filter(|conversation| conversation.run.status.is_active()).map(|conversation| conversation.id.as_str()).collect();
    assert_eq!(active, [&a.workers[0].conversation_id[..], &b.workers[0].conversation_id[..]].into_iter().collect());
    assert_eq!(a.workers[0].status, DeliveryStatus::Running);
    assert_eq!(b.workers[0].status, DeliveryStatus::Running);
    assert_ne!(a.bot_id, b.bot_id);
    assert!(f.store.snapshot().conversations.iter().all(|conversation| !active.contains(conversation.id.as_str())), "active workers remain private");
    let b_run = f.get(&b.workers[0].conversation_id).run.id;
    f.provider.release_for("worker-a");
    f.reported(&parent_a, 4).await;
    assert_eq!((f.get(&b.workers[0].conversation_id).run.id, f.get(&b.workers[0].conversation_id).run.status), (b_run, RunStatus::Running));
    f.provider.release_for("worker-b");
    f.reported(&parent_b, 4).await;
    assert_eq!(f.get(&parent_a).messages[3].text, "A_GROUPED_ANSWER");
    assert_eq!(f.get(&parent_b).messages[3].text, "B_GROUPED_ANSWER");
    assert_eq!(f.provider.requests().len(), 8, "each Bot runs its actual spawn, worker and one report exactly once");
    f.assert_claims(2);
    f.close().await;
}

async fn user_worker_report_fifo(root: &Path) {
    let f = Fixture::new(root, "worker-user-fifo", 2).await;
    let bot = f.bot_id();
    let parent = f.conversation(&bot).await;
    let before = f.conversation(&bot).await;
    let last = f.conversation(&bot).await;
    f.provider.plan_contains("FIFO_PARENT", vec![spawn("FIFO_TASK"), Reply::hold("parent", "Queued the read-only task.")]);
    f.provider.plan_contains("FIFO_USER_BEFORE", vec![Reply::hold("before", "Earlier ordinary answer.")]);
    f.provider.plan_contains("FIFO_TASK", vec![Reply::hold("worker", "FIFO_WORKER_ANSWER")]);
    f.provider.plan_prefix(WORKER_REPORT, vec![Reply::hold("report", "FIFO_GROUPED_ANSWER")]);
    f.provider.plan_contains("FIFO_USER_AFTER", vec![Reply::hold("after", "Later ordinary answer.")]);
    f.provider.plan_contains("FIFO_USER_LAST", vec![Reply::text("Final ordinary answer.")]);
    f.send(&parent, "FIFO_PARENT").await;
    f.approve(&parent).await;
    f.provider.wait_for_gate("parent").await;
    let batch = f.batch(&parent);
    let child = &batch.workers[0].conversation_id;
    f.send(&before, "FIFO_USER_BEFORE").await;
    f.assert_queue(&[(&bot, &before, "user")]);
    assert!(f.get(child).messages.is_empty(), "unreleased workers cannot reserve a FIFO position before an accepted User");
    f.provider.release_for("parent");
    f.provider.wait_for_gate("before").await;
    f.assert_queue(&[(&bot, child, "worker")]);
    assert_eq!(f.runtime.start_worker(child).await.unwrap_err().status, 409, "the compatibility wrapper cannot bypass the earlier User's Bot claim");
    f.send(&parent, "FIFO_USER_AFTER").await;
    f.assert_queue(&[(&bot, child, "worker"), (&bot, &parent, "user")]);
    assert!(matches!(&f.disk().work_queue[0].kind, WorkKind::Worker { worker_id } if worker_id == &batch.workers[0].id));
    f.provider.release_for("before");
    f.provider.wait_for_gate("worker").await;
    assert_eq!(f.get(&parent).messages.len(), 2, "queued User input never appends to a live worker's parent early");
    f.assert_queue(&[(&bot, &parent, "user")]);
    f.provider.release_for("worker");
    f.provider.wait_for_gate("after").await;
    f.assert_queue(&[(&bot, &parent, "worker_report")]);
    assert!(matches!(&f.disk().work_queue[0].kind, WorkKind::WorkerReport { batch_id } if batch_id == &batch.id));
    assert_eq!(f.runtime.start_worker_report(&batch.id).await.unwrap_err().status, 409, "a report cannot bypass the preceding ordinary turn");
    assert_eq!(f.batch(&parent).report.unwrap().status, ReportStatus::Queued);
    f.provider.release_for("after");
    f.provider.wait_for_gate("report").await;
    f.send(&last, "FIFO_USER_LAST").await;
    f.assert_queue(&[(&bot, &last, "user")]);
    assert_eq!(f.batch(&parent).report.unwrap().status, ReportStatus::Running);
    f.provider.release_for("report");
    let done = f.reported(&parent, 6).await;
    f.finished(&before, 2, RunStatus::Completed).await;
    f.finished(&last, 2, RunStatus::Completed).await;
    assert_eq!(f.get(&parent).messages[5].text, "FIFO_GROUPED_ANSWER");
    assert_eq!(done.report_input_id.as_deref(), Some(f.get(&parent).messages[4].id.as_str()));
    let prompts: Vec<_> = f.provider.requests().iter().map(|request| current_prompt(&request.payload)).collect();
    assert_eq!(&prompts[..5], ["FIFO_PARENT", "FIFO_PARENT", "FIFO_USER_BEFORE", "FIFO_TASK", "FIFO_USER_AFTER"]);
    assert!(prompts[5].starts_with(WORKER_REPORT) && prompts[5].contains("FIFO_WORKER_ANSWER"));
    let source: Value = serde_json::from_str(prompts[5].lines().last().unwrap()).unwrap();
    assert_eq!(source["sourceRequest"]["text"], "FIFO_PARENT", "the batch answers its immutable original question, never the intervening FIFO_USER_AFTER");
    assert_eq!(source["sourceRequest"]["sourceUserId"], f.get(&parent).messages[0].id);
    assert_eq!(prompts[6], "FIFO_USER_LAST");
    assert_eq!(prompts.len(), 7);
    f.assert_claims(2);
    f.close().await;
}

async fn capacity_one_releases_parent(root: &Path) {
    let f = Fixture::new(root, "worker-capacity-one", 1).await;
    let parent = f.conversation(&f.bot_id()).await;
    f.provider.plan_contains("SINGLE_PARENT", vec![spawn("SINGLE_TASK"), Reply::text("Parent releases its slot.")]);
    f.provider.plan_contains("SINGLE_TASK", vec![Reply::tool("read_file", json!({"path": "notes.txt"})), Reply::hold("single-worker", "SINGLE_READ_ANSWER")]);
    f.provider.plan_prefix(WORKER_REPORT, vec![Reply::text("SINGLE_GROUPED_ANSWER")]);
    f.send(&parent, "SINGLE_PARENT").await;
    f.approve(&parent).await;
    f.provider.wait_for_gate("single-worker").await;
    let batch = f.batch(&parent);
    assert_eq!(f.get(&parent).run.status, RunStatus::Completed);
    assert_eq!(f.get(&batch.workers[0].conversation_id).run.status, RunStatus::Running);
    assert_eq!(f.disk().conversations.iter().filter(|conversation| conversation.run.status.is_active()).count(), 1);
    assert!(f.provider.tool_outcomes().iter().any(|outcome| outcome.contains("FIFO_READ_ONLY_NOTES")), "the worker actually read the approved local file");
    f.provider.release_for("single-worker");
    let done = f.reported(&parent, 4).await;
    assert_eq!(done.workers[0].status, DeliveryStatus::Completed);
    assert_eq!(f.get(&parent).messages[3].text, "SINGLE_GROUPED_ANSWER");
    assert_eq!(f.provider.requests().len(), 5, "cap one admits parent, child and one grouped report without a waiting-parent slot deadlock");
    f.assert_claims(1);
    f.close().await;
}

async fn parent_stop_preserves_independent_work(root: &Path) {
    let f = Fixture::new(root, "worker-stop-scope", 2).await;
    let (alpha, beta) = (f.bot_id(), f.bot("Stop Beta").await);
    let parent = f.conversation(&alpha).await;
    let sibling = f.conversation(&alpha).await;
    let other = f.conversation(&beta).await;
    f.provider.plan_contains(
        "STOP_PARENT",
        vec![
            spawn("STOP_TASK_RUNNING"),
            spawn("STOP_TASK_NEVER"),
            Reply::send(&beta, "STOP_CHAIN_LEAF"),
            Reply::text("Worker tasks and normal bot message queued."),
        ],
    );
    f.provider.plan_contains("STOP_TASK_RUNNING", vec![Reply::hold("stopped-worker", "CANCELLED_WORKER_RESULT_MUST_NOT_BE_REPORTED")]);
    f.provider.plan_contains("STOP_CHAIN_LEAF", vec![Reply::hold("chain-leaf", "STOP_CHAIN_LEAF_ANSWER")]);
    f.provider.plan_prefix(BOT_REPORT, vec![Reply::text("NORMAL_CHAIN_FINISHED")]);
    f.provider.plan_contains("STOP_SURVIVING_USER", vec![Reply::hold("surviving-user", "USER_SURVIVES_ANSWER")]);
    f.provider.plan_contains("STOP_B_USER", vec![Reply::text("B_USER_SURVIVES")]);
    f.send(&parent, "STOP_PARENT").await;
    for _ in 0..3 {
        f.approve(&parent).await;
    }
    f.provider.wait_for_gate("stopped-worker").await;
    f.provider.wait_for_gate("chain-leaf").await;
    let batch = f.batch(&parent);
    assert_eq!(batch.workers.len(), 2);
    assert_eq!(batch.workers[0].status, DeliveryStatus::Running);
    assert_eq!(batch.workers[1].status, DeliveryStatus::Queued);
    let delivery = f.disk().bot_deliveries.into_iter().find(|delivery| delivery.source_conversation_id == parent).unwrap();
    let leaf_run = f.get(&delivery.recipient_conversation_id).run.id;
    f.send(&sibling, "STOP_SURVIVING_USER").await;
    f.send(&other, "STOP_B_USER").await;
    let queued_b = f.disk().work_queue.into_iter().find(|work| work.conversation_id == other).unwrap();
    let (status, body) = f.api("POST", &format!("/api/conversations/{parent}/cancel"), Some(json!({}))).await;
    assert_eq!(status, 200, "{body}");
    f.provider.wait_for_gate("surviving-user").await;
    let cancelled = f.batch(&parent);
    assert!(cancelled.cancelled && cancelled.workers.iter().all(|worker| worker.status == DeliveryStatus::Cancelled));
    assert!(cancelled.report.is_none(), "cancelled internal work cannot synthesize later");
    assert_eq!(f.get(&batch.workers[0].conversation_id).run.status, RunStatus::Cancelled);
    assert!(f.get(&batch.workers[1].conversation_id).messages.is_empty(), "queued internal sibling never executes");
    assert_eq!(
        (f.get(&delivery.recipient_conversation_id).run.id, f.get(&delivery.recipient_conversation_id).run.status),
        (leaf_run.clone(), RunStatus::Running)
    );
    let disk = f.disk();
    assert!(disk.bot_deliveries.iter().all(|delivery| delivery.chain_termination.is_none()), "normal Bot-message descendants are not internal worker children");
    assert!(disk.work_queue.contains(&queued_b), "another Bot's accepted User keeps its exact FIFO ID");
    assert_eq!(f.get(&sibling).run.status, RunStatus::Running, "a different user conversation of the same Bot survives parent Stop");
    f.provider.discard_cancelled_gate("stopped-worker").await;
    f.provider.release_for("surviving-user");
    f.finished(&sibling, 2, RunStatus::Completed).await;
    assert_eq!((f.get(&delivery.recipient_conversation_id).run.id, f.get(&delivery.recipient_conversation_id).run.status), (leaf_run, RunStatus::Running));
    f.provider.release_for("chain-leaf");
    f.wait("the independent normal chain report", |disk| {
        disk.bot_deliveries.iter().any(|item| {
            item.id == delivery.id
                && item.status == DeliveryStatus::Completed
                && item.report.as_ref().is_some_and(|report| report.status == ReportStatus::Completed)
        })
    })
    .await;
    f.finished(&other, 2, RunStatus::Completed).await;
    let reported = f.finished(&parent, 4, RunStatus::Completed).await;
    assert_eq!(reported.messages[3].text, "NORMAL_CHAIN_FINISHED");
    assert_eq!(reported.messages[2].report_delivery_ids.as_deref(), Some(std::slice::from_ref(&delivery.id)));
    assert!(reported.messages.iter().all(|message| !message.text.contains("CANCELLED_WORKER_RESULT_MUST_NOT_BE_REPORTED")));
    assert_eq!(f.provider.requests().len(), 9, "no cancelled worker/report is re-admitted, and each unrelated turn runs once");
    f.assert_claims(2);
    f.close().await;
}
