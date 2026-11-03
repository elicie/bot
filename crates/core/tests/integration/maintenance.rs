//! Update preparation is an atomic reply-admission gate, not a racy public-state idle check.

use crate::common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::Request;
use grok_bot_core::app::{Backend, BackendOptions};
use grok_bot_core::http::{HttpContext, default_allowed_hosts, router};
use grok_bot_core::scheduler::SchedulerOptions;
use grok_bot_model::{RoutineInput, RoutineSchedule, RunState, RunStatus};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Fixture {
    context: Arc<HttpContext>,
    app: Router,
}

impl Fixture {
    async fn new(root: &Path, engine: &Path) -> Self {
        let backend = Backend::new(BackendOptions {
            data_dir: root.join("data"),
            engine_root: engine.to_path_buf(),
            web_root: None,
            embedded_web: None,
            allowed_hosts: Vec::new(),
            now: None,
            scheduler: Some(SchedulerOptions { auto_start: false, ..SchedulerOptions::default() }),
            runtime: None,
        })
        .await
        .unwrap();
        backend.runtime.set_origin("http://127.0.0.1:3333".into());
        let context = Arc::new(HttpContext::new(backend.store, backend.runtime, backend.scheduler, backend.queue, default_allowed_hosts(&[]), None));
        context.port.store(3333, Ordering::SeqCst);
        Self { app: router(context.clone()), context }
    }

    async fn request(&self, remote: &str, method: &str, route: &str, body: Option<Value>, extra: &[(&str, &str)]) -> (u16, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(route)
            .header("Host", "127.0.0.1:3333")
            .header("Origin", "http://127.0.0.1:3333")
            .header("X-Grok-Bot", "1")
            .header("Content-Type", "application/json")
            .extension(ConnectInfo(remote.parse::<std::net::SocketAddr>().unwrap()))
            .body(Body::from(body.map(|body| body.to_string()).unwrap_or_default()))
            .unwrap();
        for (name, value) in extra {
            let name = axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap();
            if value.is_empty() {
                request.headers_mut().remove(name);
            } else {
                request.headers_mut().insert(name, value.parse().unwrap());
            }
        }
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn api(&self, method: &str, route: &str, body: Option<Value>) -> (u16, Value) {
        self.request("127.0.0.1:40000", method, route, body, &[]).await
    }

    async fn control(&self, method: &str) -> (u16, Value) {
        self.api(method, "/api/maintenance", Some(json!({}))).await
    }

    async fn conversation(&self) -> String {
        let bot = self.context.store.snapshot().bots[0].id.clone();
        self.context.store.mutate(|s| s.add_conversation(&bot)).await.unwrap().id
    }

    async fn close(self) {
        self.context.scheduler.close().await;
        self.context.runtime.shutdown().await;
    }
}

#[test]
fn update_maintenance() {
    common::run("maintenance", async |root| {
        let engine = root.join("engine");
        active_replies_refuse_preparation(&root.join("busy"), &engine).await;
        controls_and_mutation_guard(&root.join("controls"), &engine).await;
        queued_work_and_late_admission(&root.join("queue"), &engine).await;
    });
}

async fn active_replies_refuse_preparation(root: &Path, engine: &Path) {
    let fixture = Fixture::new(root, engine).await;
    let conversation = fixture.conversation().await;
    for status in [RunStatus::Starting, RunStatus::Running, RunStatus::AwaitingPermission] {
        fixture
            .context
            .store
            .mutate(|s| {
                s.conversation_mut(&conversation)?.run = RunState { id: "busy-run".into(), status, ..RunState::idle() };
                Ok(())
            })
            .await
            .unwrap();
        let before = fixture.context.store.snapshot();
        assert_eq!(fixture.control("POST").await.0, 409, "{status:?}");
        assert!(!fixture.context.runtime.is_paused());
        assert_eq!(fixture.context.store.snapshot(), before, "busy preparation changed state");
        assert_eq!(fixture.api("GET", "/api/state", None).await.0, 200);
    }
    fixture.close().await;
}

async fn controls_and_mutation_guard(root: &Path, engine: &Path) {
    let fixture = Fixture::new(root, engine).await;
    for method in ["POST", "DELETE"] {
        for extra in [vec![("Origin", "")], vec![("X-Grok-Bot", "")], vec![("Content-Type", "text/plain")], vec![("Origin", "http://evil.invalid")]] {
            assert_eq!(fixture.request("127.0.0.1:40000", method, "/api/maintenance", Some(json!({})), &extra).await.0, 403);
        }
        let forwarded = [("X-Forwarded-For", "127.0.0.1"), ("Forwarded", "for=127.0.0.1")];
        assert_eq!(fixture.request("192.0.2.10:40000", method, "/api/maintenance", Some(json!({})), &forwarded).await.0, 403);
        assert!(!fixture.context.runtime.is_paused());
    }
    assert_eq!(fixture.api("POST", "/api/maintenance", Some(json!({"unexpected": true}))).await.0, 400);
    assert_eq!(fixture.control("POST").await, (200, json!({"maintenance": true, "dataDir": fixture.context.store.data_dir()})));
    assert_eq!(fixture.control("POST").await, (200, json!({"maintenance": true, "dataDir": fixture.context.store.data_dir()})));
    assert_eq!(fixture.api("GET", "/api/state", None).await.0, 200);
    assert_eq!(fixture.api("POST", "/api/bots", Some(json!({"name": "Blocked"}))).await.0, 503);
    assert_eq!(fixture.api("POST", "/hooks/routines/unknown", None).await.0, 503);
    assert_eq!(fixture.api("POST", "/internal/mcp/unknown", Some(json!({}))).await.0, 503);
    assert_eq!(fixture.control("DELETE").await, (200, json!({"maintenance": false})));
    assert_eq!(fixture.api("POST", "/api/bots", Some(json!({"name": "Resumed"}))).await.0, 201);
    assert_eq!(fixture.control("POST").await.0, 200);
    fixture.context.closing.store(true, Ordering::SeqCst);
    assert_eq!(fixture.control("DELETE").await.0, 503);
    assert!(fixture.context.runtime.is_paused(), "shutdown was resumed");
    fixture.context.runtime.shutdown().await;
    assert_eq!(fixture.context.runtime.resume().unwrap_err().status, 503);
    fixture.close().await;
}

async fn queued_work_and_late_admission(root: &Path, engine: &Path) {
    let fixture = Fixture::new(root, engine).await;
    let bot = fixture.context.store.snapshot().bots[0].id.clone();
    let routine = fixture
        .context
        .store
        .mutate(|s| {
            s.add_routine(RoutineInput {
                bot_id: bot,
                name: "Queued routine".into(),
                prompt: "A synthetic test prompt".into(),
                schedule: RoutineSchedule::Interval { minutes: 60 },
                enabled: false,
            })
        })
        .await
        .unwrap();
    let queued = fixture.context.store.mutate(|s| s.run_routine(&routine.id, s.now())).await.unwrap();
    assert_eq!(fixture.control("POST").await.0, 200, "durable queues may be preserved");
    let before = fixture.context.store.snapshot();
    fixture.context.scheduler.tick().await.unwrap();
    assert_eq!(fixture.context.store.snapshot(), before, "paused tick changed or dispatched queued work");
    assert_eq!(fixture.context.scheduler.run(&routine.id).await.unwrap_err().status, 503);
    assert_eq!(fixture.context.scheduler.run_webhook(&routine.id, "unused-test-key", None).await.unwrap_err().status, 503);
    assert!(fixture.context.scheduler.error().is_none());

    // This command passed HTTP middleware before preparation but reaches turn admission afterwards.
    // Poll it until it is waiting behind a queue holder, then latch maintenance before releasing it.
    assert_eq!(fixture.control("DELETE").await.0, 200);
    let conversation = fixture.conversation().await;
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let queue = fixture.context.queue.clone();
    let holder = tokio::spawn(async move {
        queue
            .run(async {
                entered.send(()).unwrap();
                release_rx.await.unwrap();
            })
            .await;
    });
    entered_rx.await.unwrap();
    let route = format!("/api/conversations/{conversation}/messages");
    let mut late = Box::pin(fixture.api("POST", &route, Some(json!({"text": "Must not start"}))));
    assert!(futures::poll!(late.as_mut()).is_pending());
    fixture.context.runtime.pause_if_idle().unwrap();
    release.send(()).unwrap();
    holder.await.unwrap();
    assert_eq!(late.await.0, 503);
    assert!(fixture.context.store.read(|s| s.conversation(&conversation).unwrap().messages.is_empty()));
    assert_eq!(fixture.context.runtime.start(&conversation, "Direct admission is blocked too").await.unwrap_err().status, 503);
    assert_eq!(fixture.control("DELETE").await.0, 200);
    fixture.context.scheduler.tick().await.unwrap();
    let after = fixture.context.store.snapshot();
    let resumed = after.routine_runs.iter().find(|run| run.id == queued.id).unwrap();
    assert!(resumed.result.is_some(), "resumed dispatcher did not consume the queued run");
    assert_eq!(after.routine_runs.len(), 1, "queued run was duplicated");
    // No provider is configured: dispatch fails locally instead of making a model call.
    assert_eq!(resumed.result.as_ref().unwrap().status, grok_bot_model::RoutineResultStatus::Error);
    fixture.close().await;
}
