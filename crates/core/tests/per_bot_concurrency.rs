//! Issue #20: one durable FIFO lane per speaking bot, with a configurable global run cap.
//!
//! These scenarios drive a listening Backend, its live scheduler and the real in-process engine.
//! Provider one-shot gates, store subscriptions and explicit scheduler fences establish ordering;
//! no assertion depends on sleeping long enough for another task to run. The engine environment is
//! process-global, so all scenarios share this binary's single #[test].

mod common;
#[path = "scheduler/provider.rs"]
mod provider;
#[path = "per_bot_concurrency/scripted_provider.rs"]
mod scripted_provider;

use std::collections::BTreeSet;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use grok_bot_core::app::{Backend, BackendOptions};
use grok_bot_core::runtime::RuntimeOptions;
use grok_bot_core::scheduler::{RoutineScheduler, SchedulerOptions};
use grok_bot_model::{AppState, Clock, Conversation, DeliveryStatus, DiskState, ReportStatus, RoutineResultStatus, RunStatus, SettingsInput};
use grok_bot_store::Store;
use provider::Provider;
use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};

const TIMEOUT: Duration = Duration::from_secs(45);
const NOW: &str = "2026-01-01T00:00:00Z";

fn clock() -> Clock {
    // Deliberately equal timestamps: FIFO is admission order, not conversation creation order.
    Arc::new(|| DateTime::parse_from_rfc3339(NOW).unwrap().with_timezone(&Utc))
}

struct Fixture {
    data: PathBuf,
    base_url: String,
    workspace: PathBuf,
    store: Arc<Store>,
    scheduler: Arc<RoutineScheduler>,
    /// Recovered state before the live scheduler is allowed to dispatch anything.
    initial: DiskState,
    port: u16,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl Fixture {
    async fn new(root: &Path, name: &str, base_url: String, capacity: Option<usize>) -> Self {
        let data = root.join(name).join("data");
        let workspace = root.join(name).join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let now = clock();
        let backend = Backend::new(BackendOptions {
            data_dir: data.clone(),
            engine_root: root.join("engine"),
            web_root: None,
            embedded_web: None,
            allowed_hosts: vec![],
            now: Some(now.clone()),
            scheduler: Some(SchedulerOptions { now, poll_interval: Duration::from_millis(20), auto_start: true }),
            runtime: Some(RuntimeOptions {
                startup_timeout: Duration::from_secs(30),
                prompt_idle_timeout: Duration::from_secs(120),
                cancel_deadline: Duration::from_millis(100),
                ..RuntimeOptions::new(root.join("engine"))
            }),
        })
        .await
        .unwrap();
        // Bootstrap before serve(): recovered queued work must not race a stale provider URL.
        // Settings changes exercised by the scenarios themselves go through the real HTTP route.
        let input: SettingsInput = serde_json::from_value(settings(&base_url, &workspace, capacity)).unwrap();
        backend.store.mutate(|state| state.update_settings(input)).await.unwrap();
        let initial = backend.store.read(|state| state.disk().clone());
        let (store, scheduler) = (backend.store.clone(), backend.scheduler.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop, stopped) = oneshot::channel();
        let server = tokio::spawn(backend.serve(listener, async {
            let _ = stopped.await;
        }));
        let fixture = Self { data, base_url, workspace, store, scheduler, initial, port, stop: Some(stop), server: Some(server) };
        // serve() connects the Runtime's real local MCP broker before accepting HTTP commands.
        assert_eq!(fixture.api("GET", "/api/state", None).await.0, 200);
        fixture
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
        (status, serde_json::from_str(&text).unwrap_or_else(|error| panic!("{method} {route}: {error}: {text}")))
    }

    async fn state(&self) -> Value {
        let (status, state) = self.api("GET", "/api/state", None).await;
        assert_eq!(status, 200, "{state}");
        state
    }

    async fn queue(&self) -> Vec<Value> {
        let state = self.state().await;
        let queue = state["workQueue"].as_array().expect("public state always exposes a workQueue list").clone();
        assert_queue_shape(&queue);
        queue
    }

    async fn bot(&self, name: &str) -> String {
        let (status, state) = self.api("POST", "/api/bots", Some(json!({"name": name}))).await;
        assert_eq!(status, 201, "{state}");
        state["bots"].as_array().unwrap().iter().find(|bot| bot["name"] == name).unwrap()["id"].as_str().unwrap().to_owned()
    }

    fn bot_id(&self) -> String {
        self.store.snapshot().bots[0].id.clone()
    }

    async fn conversation(&self, bot: &str) -> String {
        let (status, body) = self.api("POST", "/api/conversations", Some(json!({"botId": bot}))).await;
        assert_eq!(status, 201, "{body}");
        body["conversation"]["id"].as_str().unwrap().to_owned()
    }

    async fn send(&self, conversation: &str, text: &str) {
        let (status, body) = self.api("POST", &format!("/api/conversations/{conversation}/messages"), Some(json!({"text": text}))).await;
        assert_eq!(status, 202, "ordinary messages enqueue even when their bot or conversation is live: {body}");
    }

    async fn send_file(&self, conversation: &str, text: &str, name: &str, content: &str) -> Value {
        let (status, body) = self
            .api(
                "POST",
                &format!("/api/conversations/{conversation}/attachment-messages"),
                Some(json!({"text": text, "attachments": [{
                    "name": name, "mimeType": "text/plain", "data": grok_bot_store::attachments::base64_encode(content.as_bytes()),
                }]})),
            )
            .await;
        assert_eq!(status, 202, "an attachment request queues while its bot/capacity is occupied: {body}");
        self.queue()
            .await
            .into_iter()
            .find(|item| item["conversationId"] == conversation && item["text"] == text)
            .expect("attachment request is still queued behind its held predecessor")
    }

    fn attachment_blob(&self, conversation: &str, item: &Value) -> PathBuf {
        self.data.join("attachments").join(conversation).join(item["attachments"][0]["id"].as_str().unwrap())
    }

    async fn cancel(&self, conversation: &str) {
        let (status, body) = self.api("POST", &format!("/api/conversations/{conversation}/cancel"), Some(json!({}))).await;
        assert_eq!(status, 200, "{body}");
    }

    fn get(&self, id: &str) -> Conversation {
        self.store.read(|state| state.conversation(id).unwrap().clone())
    }

    async fn wait(&self, what: &str, predicate: impl Fn(&AppState) -> bool) -> AppState {
        let mut changes = self.store.subscribe();
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let state = self.store.snapshot();
                if predicate(&state) {
                    return state;
                }
                match changes.recv().await {
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => panic!("store closed while waiting for {what}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("waiting for {what}: {}", serde_json::to_string(&self.store.snapshot()).unwrap()))
    }

    async fn finished(&self, id: &str, messages: usize, status: RunStatus) -> Conversation {
        let state = self
            .wait("a terminal conversation with the expected messages", |state| {
                state
                    .conversations
                    .iter()
                    .any(|conversation| conversation.id == id && conversation.messages.len() == messages && conversation.run.status.is_terminal())
            })
            .await;
        let conversation = state.conversations.into_iter().find(|conversation| conversation.id == id).unwrap();
        assert_eq!(conversation.run.status, status, "{:?}", conversation.run);
        conversation
    }

    async fn approve(&self, id: &str) {
        let state = self
            .wait("a real send_message permission", |state| {
                state
                    .conversations
                    .iter()
                    .any(|conversation| conversation.id == id && (conversation.run.permission.is_some() || conversation.run.status.is_terminal()))
            })
            .await;
        let conversation = state.conversations.iter().find(|conversation| conversation.id == id).unwrap();
        let permission = conversation.run.permission.as_ref().unwrap_or_else(|| panic!("send_message ended without its approval: {:?}", conversation.run));
        let option = &permission.options.iter().find(|option| option.kind == "allow_once").unwrap().id;
        let (status, body) = self.api("POST", &format!("/api/conversations/{id}/permissions/{}", permission.id), Some(json!({"optionId": option}))).await;
        assert_eq!(status, 200, "{body}");
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
            tokio::time::timeout(TIMEOUT, server).await.expect("shutdown must not wait for provider gates").unwrap().unwrap();
        }
    }

    async fn close(mut self) {
        self.stop_backend().await;
    }
}

fn settings(base_url: &str, workspace: &Path, capacity: Option<usize>) -> Value {
    let mut settings = json!({
        "baseUrl": base_url, "model": "scheduler-test-model", "workspace": workspace.to_string_lossy(), "clearApiKey": true,
    });
    if let Some(capacity) = capacity {
        settings["maxConcurrentRuns"] = json!(capacity);
    }
    settings
}

struct HeldFixture {
    app: Fixture,
    provider: Provider,
    provider_server: tokio::task::JoinHandle<()>,
}

impl Deref for HeldFixture {
    type Target = Fixture;

    fn deref(&self) -> &Self::Target {
        &self.app
    }
}

impl HeldFixture {
    async fn new(root: &Path, name: &str, capacity: Option<usize>) -> Self {
        let provider = Provider::new(root.join(name).join("data/state.json"), false);
        let (base_url, provider_server) = provider.serve().await;
        let app = Fixture::new(root, name, base_url, capacity).await;
        Self { app, provider, provider_server }
    }

    async fn close(self) {
        self.app.close().await;
        self.provider.release_all();
        self.provider_server.abort();
        assert!(self.provider_server.await.unwrap_err().is_cancelled());
    }
}

fn assert_queue_shape(queue: &[Value]) {
    let expected: BTreeSet<_> = ["id", "botId", "conversationId", "createdAt", "kind"].into_iter().collect();
    let mut ids = BTreeSet::new();
    for item in queue {
        let keys: BTreeSet<_> = item.as_object().unwrap().keys().map(String::as_str).collect();
        assert!(expected.is_subset(&keys), "public queued work exposes its stable identity, owner, conversation, admission time and kind: {item}");
        assert!(ids.insert(item["id"].as_str().unwrap()), "each queued item has its own stable ID");
        for field in ["id", "botId", "conversationId"] {
            uuid::Uuid::parse_str(item[field].as_str().unwrap()).unwrap();
        }
        DateTime::parse_from_rfc3339(item["createdAt"].as_str().unwrap()).unwrap();
        assert!(matches!(item["kind"].as_str().unwrap(), "user" | "routine" | "delivery" | "group" | "report"), "{item}");
    }
}

fn assert_queue(queue: &[Value], expected: &[(&str, &str, &str)]) {
    assert_queue_shape(queue);
    assert_eq!(queue.len(), expected.len(), "{queue:?}");
    for (item, (bot, conversation, kind)) in queue.iter().zip(expected) {
        assert_eq!(item["botId"], *bot, "{item}");
        assert_eq!(item["conversationId"], *conversation, "{item}");
        assert_eq!(item["kind"], *kind, "{item}");
    }
}

fn queue_ids(disk: &DiskState) -> Vec<String> {
    serde_json::to_value(disk).unwrap()["workQueue"]
        .as_array()
        .expect("queued turns are durable")
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .collect()
}

fn assert_active_claims(disk: &DiskState, capacity: usize) {
    let active: Vec<_> = disk.conversations.iter().filter(|conversation| conversation.run.status.is_active()).collect();
    assert!(active.len() <= capacity, "global cap {capacity}: {active:?}");
    let mut owners = BTreeSet::new();
    for conversation in active {
        let run = serde_json::to_value(&conversation.run).unwrap();
        let owner = run["botId"].as_str().unwrap_or(&conversation.bot_id);
        assert!(owners.insert(owner.to_owned()), "one engine execution per speaking bot, even across conversations: {disk:?}");
        if conversation.group_id.is_some() {
            assert_ne!(owner, conversation.bot_id, "a group run is owned by its actual speaker, not the group ID");
        }
    }
}

fn assert_provider_claims(provider: &Provider, capacity: usize) {
    for request in provider.requests() {
        assert_active_claims(&request.persisted, capacity);
        assert!(
            request.persisted.conversations.iter().any(|conversation| conversation.run.status.is_active()),
            "claim was durable before {} reached inference",
            request.prompt
        );
    }
}

#[test]
fn per_bot_concurrency_regressions() {
    common::run("per-bot-concurrency", async |root| {
        tokio::time::timeout(Duration::from_secs(90), independent_lanes_and_same_conversation_fifo(&root)).await.expect("independent lanes and FIFO");
        tokio::time::timeout(Duration::from_secs(90), global_capacity_and_configuration(&root)).await.expect("global capacity");
        tokio::time::timeout(Duration::from_secs(90), stop_is_conversation_scoped(&root)).await.expect("conversation-scoped Stop");
        tokio::time::timeout(Duration::from_secs(90), ordinary_and_routine_share_one_fifo(&root)).await.expect("ordinary/routine FIFO");
        tokio::time::timeout(Duration::from_secs(90), group_speakers_share_their_normal_lane(&root)).await.expect("group speaker lanes");
        tokio::time::timeout(Duration::from_secs(90), restart_retains_queue_and_does_not_replay_claims(&root)).await.expect("durable restart claims");
        tokio::time::timeout(Duration::from_secs(90), cyclic_send_message_and_reports_release_capacity(&root)).await.expect("real A to B to A cycle");
    });
}

async fn independent_lanes_and_same_conversation_fifo(root: &Path) {
    let f = HeldFixture::new(root, "independent", None).await;
    let (alpha, beta) = (f.bot_id(), f.bot("Beta").await);
    // Creation order is deliberately the reverse of later enqueue order.
    let later = f.conversation(&alpha).await;
    let first = f.conversation(&alpha).await;
    let other = f.conversation(&beta).await;
    let original_instructions = f.store.snapshot().bots.iter().find(|bot| bot.id == alpha).unwrap().instructions.clone();
    f.send(&first, "A first held turn").await;
    f.provider.wait_for(1).await;
    let first_run = f.get(&first).run.id;

    // Bot-local settings belong to that bot's lane, not the unrelated globally active run.
    let (status, body) = f.api("PATCH", &format!("/api/bots/{beta}"), Some(json!({"instructions": "Beta may change while Alpha works."}))).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = f.api("PATCH", &format!("/api/bots/{alpha}"), Some(json!({"instructions": "must not replace live instructions"}))).await;
    assert_eq!(status, 409, "a bot cannot change its own live execution settings: {body}");
    assert_eq!(f.store.snapshot().bots.iter().find(|bot| bot.id == alpha).unwrap().instructions, original_instructions);
    assert_eq!(f.store.snapshot().bots.iter().find(|bot| bot.id == beta).unwrap().instructions, "Beta may change while Alpha works.");

    f.send(&first, "A same-conversation second").await;
    f.send(&later, "A different-conversation third").await;
    let queued = f.queue().await;
    assert_queue(&queued, &[(&alpha, &first, "user"), (&alpha, &later, "user")]);
    assert_eq!(f.get(&first).messages.len(), 2, "queued input is not appended to a live session");
    assert!(f.get(&later).messages.is_empty());
    f.scheduler.tick().await.unwrap();
    assert_eq!(f.provider.prompts(), ["A first held turn"]);
    // A's blocked FIFO head is already present when B arrives. Dispatch must skip the occupied
    // lane instead of making a different bot wait behind it or returning global RUN_BUSY.
    f.send(&other, "B independent held turn").await;
    f.provider.wait_for(2).await;
    let other_run = f.get(&other).run.id;
    assert_eq!(f.store.snapshot().conversations.iter().filter(|conversation| conversation.run.status.is_active()).count(), 2);
    assert_eq!(f.queue().await, queued, "B cannot reorder or consume A's queued turns");
    assert_eq!(f.provider.prompts(), ["A first held turn", "B independent held turn"]);
    assert_eq!(queue_ids(&f.persisted().await), queued.iter().map(|item| item["id"].as_str().unwrap().to_owned()).collect::<Vec<_>>());

    f.provider.release(0);
    f.provider.wait_for(3).await;
    assert_eq!(f.provider.prompts()[2], "A same-conversation second");
    assert_ne!(f.get(&first).run.id, first_run);
    assert_eq!(f.get(&first).messages.len(), 4);
    assert_eq!((f.get(&other).run.id, f.get(&other).run.status), (other_run.clone(), RunStatus::Running));
    assert_queue(&f.queue().await, &[(&alpha, &later, "user")]);
    f.provider.release(2);
    f.provider.wait_for(4).await;
    assert_eq!(f.provider.prompts()[3], "A different-conversation third");
    assert_eq!((f.get(&other).run.id, f.get(&other).run.status), (other_run, RunStatus::Running));
    f.provider.release(3);
    f.provider.release(1);
    let first = f.finished(&first, 4, RunStatus::Completed).await;
    assert_eq!(first.messages[2].text, "A same-conversation second");
    f.finished(&later, 2, RunStatus::Completed).await;
    f.finished(&other, 2, RunStatus::Completed).await;
    assert!(f.queue().await.is_empty());
    assert_provider_claims(&f.provider, 3);
    f.close().await;
}

async fn global_capacity_and_configuration(root: &Path) {
    for configured in [None, Some(2)] {
        let capacity = configured.unwrap_or(3);
        let f = HeldFixture::new(root, &format!("capacity-{capacity}"), None).await;
        assert_eq!(f.state().await["settings"]["maxConcurrentRuns"], 3, "default global cap");
        if configured.is_some() {
            let (status, body) = f.api("PUT", "/api/settings", Some(settings(&f.base_url, &f.workspace, configured))).await;
            assert_eq!(status, 200, "{body}");
        }
        assert_eq!(f.state().await["settings"]["maxConcurrentRuns"], capacity);
        // Exercise the HTTP boundary, not just SettingsInput/store deserialization.
        for invalid in [0, 17] {
            let (status, body) = f.api("PUT", "/api/settings", Some(settings(&f.base_url, &f.workspace, Some(invalid)))).await;
            assert_eq!(status, 400, "maxConcurrentRuns must be in 1..=16: {body}");
        }
        assert_eq!(f.state().await["settings"]["maxConcurrentRuns"], capacity);
        let mut conversations = Vec::new();
        for index in 0..=capacity {
            let bot = if index == 0 { f.bot_id() } else { f.bot(&format!("Capacity bot {index}")).await };
            conversations.push((bot.clone(), f.conversation(&bot).await));
        }
        for (index, (_, conversation)) in conversations[..capacity].iter().enumerate() {
            f.send(conversation, &format!("capacity held {index}")).await;
            f.provider.wait_for(index + 1).await;
        }
        let running_ids: Vec<_> = conversations[..capacity].iter().map(|(_, conversation)| f.get(conversation).run.id).collect();
        let (waiting_bot, waiting) = &conversations[capacity];
        f.send(waiting, "capacity next queued").await;
        assert_queue(&f.queue().await, &[(waiting_bot, waiting, "user")]);
        assert!(f.get(waiting).messages.is_empty());
        f.scheduler.tick().await.unwrap();
        assert_eq!(f.provider.requests().len(), capacity);
        assert_eq!(f.store.snapshot().conversations.iter().filter(|conversation| conversation.run.status.is_active()).count(), capacity);
        f.provider.release(0);
        f.provider.wait_for(capacity + 1).await;
        assert_eq!(f.provider.prompts()[capacity], "capacity next queued", "a released slot dispatches without another message or manual tick");
        for (index, (_, conversation)) in conversations[1..capacity].iter().enumerate() {
            assert_eq!((f.get(conversation).run.id, f.get(conversation).run.status), (running_ids[index + 1].clone(), RunStatus::Running));
        }
        for index in 1..=capacity {
            f.provider.release(index);
        }
        for (_, conversation) in &conversations {
            f.finished(conversation, 2, RunStatus::Completed).await;
        }
        assert!(f.queue().await.is_empty());
        assert_provider_claims(&f.provider, capacity);
        f.close().await;
    }
}

async fn stop_is_conversation_scoped(root: &Path) {
    let f = HeldFixture::new(root, "stop", None).await;
    let (alpha, beta) = (f.bot_id(), f.bot("Stop-independent Beta").await);
    let active = f.conversation(&alpha).await;
    let queued_only = f.conversation(&alpha).await;
    let sibling = f.conversation(&alpha).await;
    let other = f.conversation(&beta).await;
    f.send(&active, "A stop active").await;
    f.provider.wait_for(1).await;
    f.send(&other, "B must remain live").await;
    f.provider.wait_for(2).await;
    let other_run = f.get(&other).run.id;
    let selected = f.send_file(&queued_only, "CANCELLED_QUEUE_CANARY one", "selected.txt", "CANCELLED_SELECTED_FILE_BYTES").await;
    let retained = f.send_file(&queued_only, "CANCELLED_QUEUE_CANARY two", "retained.txt", "CANCELLED_RETAINED_FILE_BYTES").await;
    let selected_blob = f.attachment_blob(&queued_only, &selected);
    let retained_blob = f.attachment_blob(&queued_only, &retained);
    assert_eq!(std::fs::read(&selected_blob).unwrap(), b"CANCELLED_SELECTED_FILE_BYTES");
    assert_eq!(std::fs::read(&retained_blob).unwrap(), b"CANCELLED_RETAINED_FILE_BYTES");
    f.send(&active, "CANCELLED_ACTIVE_QUEUE_CANARY").await;
    f.send(&sibling, "A other conversation survives Stop").await;
    let before = f.queue().await;
    assert_queue(&before, &[(&alpha, &queued_only, "user"), (&alpha, &queued_only, "user"), (&alpha, &active, "user"), (&alpha, &sibling, "user")]);

    let cancel_one = format!("/api/conversations/{queued_only}/queued-turns/{}/cancel", selected["id"].as_str().unwrap());
    let (status, body) = f.api("POST", &cancel_one, Some(json!({}))).await;
    assert_eq!(status, 200, "{body}");
    assert!(!selected_blob.exists(), "cancelling one unstarted attachment request deletes its uncommitted private blob");
    assert_eq!(std::fs::read(&retained_blob).unwrap(), b"CANCELLED_RETAINED_FILE_BYTES", "its later sibling still owns its own blob");
    assert_eq!(f.queue().await, before[1..], "individual cancellation removes exactly its selected ID, not all work for that conversation or bot");
    assert_eq!(f.api("POST", &cancel_one, Some(json!({}))).await.0, 404, "a stale selected ID cannot cancel a newer request");
    assert!(f.get(&queued_only).messages.is_empty());
    assert_eq!(f.get(&active).run.status, RunStatus::Running);
    assert_eq!((f.get(&other).run.id, f.get(&other).run.status), (other_run.clone(), RunStatus::Running));
    f.cancel(&queued_only).await;
    assert!(f.get(&queued_only).messages.is_empty(), "a queued-only Stop allocates no engine turn or messages");
    assert!(!retained_blob.exists(), "Stop also disposes of an unstarted attachment request's private blob");
    assert_queue(&f.queue().await, &[(&alpha, &active, "user"), (&alpha, &sibling, "user")]);
    assert_eq!(f.get(&active).run.status, RunStatus::Running, "stopping queued work must not cancel its bot's different live conversation");
    assert_eq!((f.get(&other).run.id, f.get(&other).run.status), (other_run.clone(), RunStatus::Running));
    f.cancel(&active).await;
    f.provider.release(0); // Also dispose of the provider-side handler if HTTP cancellation won.
    f.finished(&active, 2, RunStatus::Cancelled).await;
    f.provider.wait_for(3).await;
    assert_eq!(f.provider.prompts()[2], "A other conversation survives Stop");
    assert_eq!((f.get(&other).run.id, f.get(&other).run.status), (other_run.clone(), RunStatus::Running));
    f.provider.release(2);
    f.provider.release(1);
    f.finished(&sibling, 2, RunStatus::Completed).await;
    let other = f.finished(&other, 2, RunStatus::Completed).await;
    assert_eq!(other.run.id, other_run);
    f.scheduler.tick().await.unwrap();
    assert!(f.queue().await.is_empty());
    assert_eq!(
        f.provider.prompts(),
        ["A stop active", "B must remain live", "A other conversation survives Stop"],
        "neither cancelled queued input ever reaches inference"
    );
    assert!(f.get(&queued_only).messages.is_empty());
    assert_provider_claims(&f.provider, 3);
    f.close().await;
}

async fn ordinary_and_routine_share_one_fifo(root: &Path) {
    let f = HeldFixture::new(root, "routine-fifo", None).await;
    let (alpha, beta) = (f.bot_id(), f.bot("Routine-independent Beta").await);
    let last = f.conversation(&alpha).await;
    let second = f.conversation(&alpha).await;
    let active = f.conversation(&alpha).await;
    let other = f.conversation(&beta).await;
    let (status, body) = f
        .api(
            "POST",
            "/api/routines",
            Some(json!({
                "botId": alpha, "name": "FIFO routine", "prompt": "A routine between ordinary turns",
                "schedule": {"kind": "interval", "minutes": 60}, "enabled": true,
            })),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let routine = body["routines"].as_array().unwrap().iter().find(|routine| routine["name"] == "FIFO routine").unwrap()["id"].as_str().unwrap().to_owned();
    f.send(&active, "A routine blocker").await;
    f.provider.wait_for(1).await;
    f.send(&other, "B during routine FIFO").await;
    f.provider.wait_for(2).await;
    f.send(&second, "A ordinary before routine").await;
    let (status, claimed) = f.api("POST", &format!("/api/routines/{routine}/run"), Some(json!({}))).await;
    assert_eq!(status, 202, "{claimed}");
    let run_id = claimed["run"]["id"].as_str().unwrap().to_owned();
    let routine_conversation = claimed["run"]["conversationId"].as_str().unwrap().to_owned();
    f.send(&last, "A ordinary after routine").await;
    assert_queue(&f.queue().await, &[(&alpha, &second, "user"), (&alpha, &routine_conversation, "routine"), (&alpha, &last, "user")]);
    assert!(f.get(&routine_conversation).messages.is_empty());
    f.provider.release(0);
    f.provider.wait_for(3).await;
    assert_eq!(f.provider.prompts()[2], "A ordinary before routine");
    f.provider.release(2);
    f.provider.wait_for(4).await;
    assert_eq!(f.provider.prompts()[3], "A routine between ordinary turns");
    let durable = f.provider.requests()[3].persisted.clone();
    let claim = durable.routine_runs.iter().find(|run| run.id == run_id).unwrap();
    assert_eq!(claim.execution_run_id.as_deref(), Some(f.get(&routine_conversation).run.id.as_str()), "routine claim is durable before inference");
    assert!(claim.result.is_none());
    f.provider.release(3);
    f.provider.wait_for(5).await;
    assert_eq!(f.provider.prompts()[4], "A ordinary after routine");
    assert_eq!(f.get(&other).run.status, RunStatus::Running);
    f.provider.release(4);
    f.provider.release(1);
    for conversation in [&active, &second, &routine_conversation, &last, &other] {
        f.finished(conversation, 2, RunStatus::Completed).await;
    }
    let disk = f.persisted().await;
    assert_eq!(disk.routine_runs.iter().find(|run| run.id == run_id).unwrap().result.as_ref().unwrap().status, RoutineResultStatus::Completed);
    assert!(f.queue().await.is_empty());
    assert_provider_claims(&f.provider, 3);
    f.close().await;
}

async fn group_speakers_share_their_normal_lane(root: &Path) {
    let f = HeldFixture::new(root, "group-lanes", None).await;
    let alpha = f.bot_id();
    let (status, body) = f.api("PATCH", &format!("/api/bots/{alpha}"), Some(json!({"name": "Alpha"}))).await;
    assert_eq!(status, 200, "{body}");
    let beta = f.bot("Beta").await;
    let normal_alpha = f.conversation(&alpha).await;
    let normal_beta = f.conversation(&beta).await;
    let later_alpha = f.conversation(&alpha).await;
    let (status, body) = f.api("POST", "/api/groups", Some(json!({"name": "Contending team", "memberBotIds": [alpha, beta]}))).await;
    assert_eq!(status, 201, "{body}");
    let group = body["group"]["id"].as_str().unwrap();
    let (status, body) = f.api("POST", &format!("/api/groups/{group}/conversations"), Some(json!({}))).await;
    assert_eq!(status, 201, "{body}");
    let conversation = body["conversation"]["id"].as_str().unwrap().to_owned();
    f.send(&normal_alpha, "Alpha normal held").await;
    f.provider.wait_for(1).await;
    f.send(&normal_beta, "Beta normal held").await;
    f.provider.wait_for(2).await;
    f.send(&conversation, "Group round contends with normal chats").await;
    f.send(&later_alpha, "Alpha normal after group speaker").await;
    assert_queue(&f.queue().await, &[(&alpha, &conversation, "user"), (&alpha, &later_alpha, "user")]);
    f.scheduler.tick().await.unwrap();
    assert_eq!(f.provider.requests().len(), 2, "a group cannot give its first speaker a second engine slot");
    f.provider.release(0);
    f.provider.wait_for(3).await;
    let alpha_group = &f.provider.prompts()[2];
    assert!(alpha_group.contains("You are Alpha.") && alpha_group.contains("Latest user message:\nGroup round contends with normal chats"), "{alpha_group}");
    assert_eq!(serde_json::to_value(f.get(&conversation).run).unwrap()["botId"], alpha, "the group execution belongs to its speaker");
    assert_eq!(f.get(&normal_beta).run.status, RunStatus::Running);
    f.provider.release(2);
    f.provider.wait_for(4).await;
    assert_eq!(
        f.provider.prompts()[3],
        "Alpha normal after group speaker",
        "a blocked next group member must not reserve the previous speaker's free bot lane"
    );
    assert_queue(&f.queue().await, &[(&beta, &conversation, "group")]);
    assert_eq!(f.get(&conversation).group_round.as_ref().unwrap().pending_bot_ids, [beta.clone()]);
    f.provider.release(1);
    f.provider.wait_for(5).await;
    let beta_group = &f.provider.prompts()[4];
    assert!(beta_group.contains("You are Beta."), "{beta_group}");
    assert_eq!(serde_json::to_value(f.get(&conversation).run).unwrap()["botId"], beta);
    assert_eq!(f.get(&later_alpha).run.status, RunStatus::Running);
    f.provider.release(4);
    f.provider.release(3);
    let completed = f.finished(&conversation, 3, RunStatus::Completed).await;
    assert!(completed.group_round.is_none());
    assert_eq!(completed.messages.iter().map(|message| message.speaker_bot_id.clone()).collect::<Vec<_>>(), [None, Some(alpha), Some(beta)]);
    for normal in [&normal_alpha, &normal_beta, &later_alpha] {
        f.finished(normal, 2, RunStatus::Completed).await;
    }
    assert!(f.queue().await.is_empty());
    assert_provider_claims(&f.provider, 3);
    f.close().await;
}

async fn restart_retains_queue_and_does_not_replay_claims(root: &Path) {
    let mut f = HeldFixture::new(root, "restart", Some(2)).await;
    let (alpha, beta, gamma) = (f.bot_id(), f.bot("Restart Beta").await, f.bot("Restart Gamma").await);
    let active_a = f.conversation(&alpha).await;
    let active_b = f.conversation(&beta).await;
    let waiting_c = f.conversation(&gamma).await;
    f.send(&active_a, "restart active A never replay").await;
    f.provider.wait_for(1).await;
    f.send(&active_b, "restart active B never replay").await;
    f.provider.wait_for(2).await;
    f.send(&active_a, "restart durable same-conversation A").await;
    let attachment = f.send_file(&waiting_c, "restart durable capacity-waiting C", "durable.txt", "QUEUED_ATTACHMENT_SURVIVES_RESTART").await;
    let blob = f.attachment_blob(&waiting_c, &attachment);
    assert_eq!(std::fs::read(&blob).unwrap(), b"QUEUED_ATTACHMENT_SURVIVES_RESTART", "attachment bytes are durable before the turn is accepted into the queue");
    assert!(f.get(&waiting_c).messages.is_empty(), "a queued attachment is not an early conversation turn");
    let queued = f.queue().await;
    assert_queue(&queued, &[(&alpha, &active_a, "user"), (&gamma, &waiting_c, "user")]);
    assert!(
        !serde_json::to_string(&queued).unwrap().contains("QUEUED_ATTACHMENT_SURVIVES_RESTART"),
        "public queued work contains metadata, never private attachment bytes"
    );
    let persisted = f.persisted().await;
    assert!(
        !serde_json::to_string(&persisted).unwrap().contains("QUEUED_ATTACHMENT_SURVIVES_RESTART"),
        "attachment payloads remain in private blobs, not duplicated into state.json"
    );
    let ids = queue_ids(&persisted);
    let live_a_run = f.get(&active_a).run.id;
    let live_b_run = f.get(&active_b).run.id;

    // Crash recovery must interrupt every live claim, not just the first globally active run.
    // Reopen an isolated checkpoint, never the live writer's directory.
    let checkpoint = root.join("restart-checkpoint");
    std::fs::create_dir_all(&checkpoint).unwrap();
    std::fs::copy(f.data.join("state.json"), checkpoint.join("state.json")).unwrap();
    let checkpoint_blobs = checkpoint.join("attachments").join(&waiting_c);
    std::fs::create_dir_all(&checkpoint_blobs).unwrap();
    let checkpoint_blob = checkpoint_blobs.join(attachment["attachments"][0]["id"].as_str().unwrap());
    std::fs::copy(&blob, &checkpoint_blob).unwrap();
    let recovered = Store::open(&checkpoint, clock()).await.unwrap();
    recovered.read(|state| {
        assert_eq!(state.conversation(&active_a).unwrap().run.status, RunStatus::Interrupted);
        assert_eq!(state.conversation(&active_b).unwrap().run.status, RunStatus::Interrupted);
        assert_eq!(state.conversation(&active_a).unwrap().run.id, live_a_run);
        assert_eq!(state.conversation(&active_b).unwrap().run.id, live_b_run);
        assert_eq!(queue_ids(state.disk()), ids, "recovery retains exact queued IDs and FIFO order");
    });
    assert_eq!(std::fs::read(&checkpoint_blob).unwrap(), b"QUEUED_ATTACHMENT_SURVIVES_RESTART", "crash recovery retains the queued attachment's durable bytes");
    recovered.flush().await.unwrap();
    drop(recovered);

    f.app.stop_backend().await;
    assert_eq!(queue_ids(&f.persisted().await), ids, "shutdown must preserve queued work, including the live conversation's later turn");
    assert_eq!(std::fs::read(&blob).unwrap(), b"QUEUED_ATTACHMENT_SURVIVES_RESTART", "shutdown must not discard accepted pending attachments");
    assert_eq!(f.provider.requests().len(), 2, "shutdown cannot dispatch new queued claims");
    f.close().await;
    let restarted = HeldFixture::new(root, "restart", Some(2)).await;
    assert_eq!(queue_ids(&restarted.initial), ids);
    assert!(restarted.initial.conversations.iter().all(|conversation| !conversation.run.status.is_active()));
    restarted.provider.wait_for(2).await;
    // A cancelled run may require a fresh engine session. Its visible user input is legitimate
    // continuation history, not a replayed inference; compare the actual current turn separately.
    let raw_prompts = restarted.provider.prompts();
    let attachment_prompt = raw_prompts.iter().find(|prompt| prompt.contains("restart durable capacity-waiting C")).unwrap();
    assert!(
        attachment_prompt.contains("<user_attachments>\n")
            && attachment_prompt.contains(r#""name":"durable.txt""#)
            && attachment_prompt.contains(r#""content":"QUEUED_ATTACHMENT_SURVIVES_RESTART""#),
        "the actual engine must receive the recovered queued attachment: {attachment_prompt}"
    );
    let mut prompts: Vec<_> = raw_prompts
        .into_iter()
        .map(|prompt| prompt.rsplit("\n\nCurrent user message:\n").next().unwrap().split("\n\n<user_attachments>\n").next().unwrap().to_owned())
        .collect();
    prompts.sort();
    assert_eq!(prompts, ["restart durable capacity-waiting C", "restart durable same-conversation A"]);
    assert_ne!(restarted.get(&active_a).run.id, live_a_run);
    assert_eq!(restarted.get(&active_b).run.id, live_b_run);
    restarted.provider.release(0);
    restarted.provider.release(1);
    let completed = restarted.finished(&active_a, 4, RunStatus::Completed).await;
    assert_eq!(completed.messages[2].text, "restart durable same-conversation A");
    let with_attachment = restarted.finished(&waiting_c, 2, RunStatus::Completed).await;
    let delivered = with_attachment.messages[0].attachments.as_ref().unwrap();
    assert_eq!(serde_json::to_value(delivered).unwrap(), attachment["attachments"], "dispatch moves the exact queued metadata into one actual user message");
    assert_eq!(
        std::fs::read(&blob).unwrap(),
        b"QUEUED_ATTACHMENT_SURVIVES_RESTART",
        "a dispatched attachment remains available in the conversation transcript"
    );
    restarted.scheduler.tick().await.unwrap();
    assert!(restarted.queue().await.is_empty());
    assert_eq!(restarted.provider.requests().len(), 2, "no active claim is replayed, and queued claims run only once");
    assert_provider_claims(&restarted.provider, 2);
    restarted.close().await;
}

async fn cyclic_send_message_and_reports_release_capacity(root: &Path) {
    use scripted_provider::{Reply, ScriptedProvider};

    let provider = ScriptedProvider::new(root.join("cycle/data/state.json"));
    let (base_url, provider_server) = provider.serve().await;
    // A cap of one makes every WaitingChildren boundary release real global capacity as well as
    // its bot lane. A -> B -> A cannot make progress if a waiting logical parent reserves either.
    let f = Fixture::new(root, "cycle", base_url, Some(1)).await;
    let (alpha, beta) = (f.bot_id(), f.bot("Cycle Beta").await);
    let source = f.conversation(&alpha).await;
    provider.plan(vec![
        Reply::send(&beta, "CYCLE_REQUEST_B"),
        Reply::text("A delegated to B."),
        Reply::send(&alpha, "CYCLE_REQUEST_A"),
        Reply::text("B delegated back to A."),
        Reply::Hold("CYCLE_LEAF_ANSWER".into()),
        Reply::text("CYCLE_B_SUMMARY: A answered the nested request."),
        Reply::text("CYCLE_A_SUMMARY: B and A finished the cycle."),
    ]);
    f.send(&source, "Start the real A to B to A message cycle.").await;
    f.approve(&source).await;
    f.finished(&source, 2, RunStatus::Completed).await;
    let first = f.store.snapshot().bot_deliveries.into_iter().find(|delivery| delivery.source_conversation_id == source).unwrap();
    f.approve(&first.recipient_conversation_id).await;
    provider.wait_for(5).await;
    let state = f
        .wait("a waiting B parent and running A leaf", |state| {
            state.bot_deliveries.iter().any(|delivery| delivery.id == first.id && delivery.status == DeliveryStatus::WaitingChildren)
                && state.bot_deliveries.iter().any(|delivery| delivery.hop == 2 && delivery.status == DeliveryStatus::Running)
        })
        .await;
    let first = state.bot_deliveries.iter().find(|delivery| delivery.id == first.id).unwrap();
    let leaf = state.bot_deliveries.iter().find(|delivery| delivery.hop == 2).unwrap();
    assert_eq!(state.bot_deliveries.len(), 2, "both edges must come from actual approved send_message calls");
    assert_eq!((&first.from_bot_id, &first.to_bot_id, first.hop), (&alpha, &beta, 1));
    assert_eq!((&leaf.from_bot_id, &leaf.to_bot_id, leaf.hop), (&beta, &alpha, 2));
    assert_eq!(first.chain_id, leaf.chain_id);
    assert!(first.report.is_none(), "B's provisional delegation is not the answer to A");
    assert!(!f.get(&source).run.status.is_active());
    assert!(!f.get(&first.recipient_conversation_id).run.status.is_active(), "WaitingChildren is logical work, not an active engine execution");
    assert_eq!(f.get(&leaf.recipient_conversation_id).run.status, RunStatus::Running);
    assert_eq!(f.store.snapshot().conversations.iter().filter(|conversation| conversation.run.status.is_active()).count(), 1);
    assert_active_claims(&f.persisted().await, 1);
    let leaf_run = f.get(&leaf.recipient_conversation_id).run.id;
    // A waiting logical parent has no live local turn to stop. In particular, Stop on B must
    // not reach across its conversation boundary and cancel A's actual running descendant.
    f.cancel(&first.recipient_conversation_id).await;
    assert_eq!((f.get(&leaf.recipient_conversation_id).run.id, f.get(&leaf.recipient_conversation_id).run.status), (leaf_run, RunStatus::Running));
    assert!(f.store.snapshot().bot_deliveries.iter().all(|delivery| delivery.chain_termination.is_none()));
    provider.release();
    f.wait("both real automatic reports to finish", |state| {
        state.bot_deliveries.len() == 2
            && state.bot_deliveries.iter().all(|delivery| {
                delivery.status == DeliveryStatus::Completed && delivery.report.as_ref().is_some_and(|report| report.status == ReportStatus::Completed)
            })
    })
    .await;
    let root_answer = f.finished(&source, 4, RunStatus::Completed).await;
    let b_answer = f.finished(&first.recipient_conversation_id, 4, RunStatus::Completed).await;
    let leaf_answer = f.finished(&leaf.recipient_conversation_id, 2, RunStatus::Completed).await;
    assert_eq!(leaf_answer.messages[1].text, "CYCLE_LEAF_ANSWER");
    assert_eq!(b_answer.messages[2].report_delivery_ids.as_deref(), Some(std::slice::from_ref(&leaf.id)));
    assert!(b_answer.messages[2].text.contains("CYCLE_LEAF_ANSWER"));
    assert_eq!(root_answer.messages[2].report_delivery_ids.as_deref(), Some(std::slice::from_ref(&first.id)));
    assert!(root_answer.messages[2].text.contains("CYCLE_B_SUMMARY"));
    assert!(!root_answer.messages[2].text.contains("B delegated back to A."));
    assert_eq!(root_answer.messages[3].text, "CYCLE_A_SUMMARY: B and A finished the cycle.");
    f.scheduler.tick().await.unwrap();
    assert!(f.queue().await.is_empty());
    assert_eq!(provider.requests().len(), 7, "no duplicated delivery or report inference");
    for request in provider.requests() {
        assert_active_claims(&request.persisted, 1);
    }
    provider.assert_drained();
    f.close().await;
    provider_server.abort();
    assert!(provider_server.await.unwrap_err().is_cancelled());
}
