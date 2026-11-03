//! Real pinned-engine MCP Apps path: definition metadata -> ui resource -> persisted activity,
//! then a separate explicitly approved session call with no model turn or workspace broker.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use grok_bot_model::{BotInput, PermissionAction, PermissionCategory, PermissionRuleInput, SecretInput, WorkKind};
use serde_json::{Value, json};

use super::{Fixture, Reply, discover, serve, text};

const URI: &str = "ui://fixture/counter";
const SECRET: &str = "app-secret-quoted-\"-\\-end";

#[derive(Default)]
struct Recorded {
    calls: Vec<Value>,
    reads: Vec<String>,
    slow_reads: bool,
    call_gate: Option<Arc<CallGate>>,
}

#[derive(Default)]
struct CallGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

async fn mcp(State(recorded): State<Arc<Mutex<Recorded>>>, Json(request): Json<Value>) -> Response {
    let Some(id) = request.get("id").cloned() else { return StatusCode::ACCEPTED.into_response() };
    let result = match request["method"].as_str().unwrap_or_default() {
        "initialize" => {
            json!({"protocolVersion": request["params"]["protocolVersion"], "capabilities": {"tools": {}, "resources": {}}, "serverInfo": {"name": "apps-fixture", "version": "1.0.0"}})
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": [
            {"name": "render", "description": "Show the counter app.", "inputSchema": {"type": "object", "properties": {"delta": {"type": "integer"}, "quoted": {"type": "string"}}}, "_meta": {"ui": {"resourceUri": URI, "visibility": ["model", "app"]}}},
            {"name": "refresh", "description": "Read the app's fresh counter.", "inputSchema": {"type": "object", "properties": {}}, "_meta": {"ui": {"visibility": ["model", "app"]}}},
            {"name": "model_only", "description": "Never callable from apps.", "inputSchema": {"type": "object", "properties": {}}, "_meta": {"ui": {"visibility": ["model"]}}}
        ]}),
        "resources/read" => {
            let slow = {
                let mut recorded = recorded.lock().unwrap();
                recorded.reads.push(request["params"]["uri"].as_str().unwrap_or_default().to_owned());
                recorded.slow_reads
            };
            if slow {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
            json!({"contents": [{"uri": URI, "mimeType": "text/html;profile=mcp-app", "text": format!("<!doctype html><h1>Counter app</h1><p>{SECRET}</p>")} ]})
        }
        "tools/call" => {
            let gate = {
                let mut recorded = recorded.lock().unwrap();
                recorded.calls.push(request["params"].clone());
                recorded.call_gate.clone()
            };
            if let Some(gate) = gate {
                gate.entered.notify_one();
                tokio::time::timeout(Duration::from_secs(10), gate.release.notified()).await.expect("release the isolated app call");
            }
            json!({"content": [{"type": "text", "text": format!("counter: 1 {SECRET}")}], "structuredContent": {"mustNotBeSynthesized": true}, "_meta": {"privateResultMeta": true}})
        }
        _ => return Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}})).into_response(),
    };
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

pub(super) async fn mcp_apps(f: &Fixture) {
    f.configure("", false).await;
    let bot = f.bot_id();
    let recorded = Arc::new(Mutex::new(Recorded::default()));
    let origin = serve(Router::new().route("/mcp", post(mcp).get(|| async { StatusCode::METHOD_NOT_ALLOWED })).with_state(recorded.clone())).await;
    f.store.mutate(|s| s.add_secret(SecretInput { name: "APPS_TOKEN".into(), value: SECRET.into(), bot_id: Some(bot.clone()) }, true)).await.unwrap();
    f.store
        .mutate(|s| {
            s.set_mcp_servers(
                &bot,
                &json!({"apps": {"url": format!("{origin}/mcp"), "headers": {"Authorization": "Bearer ${secret:APPS_TOKEN}"}}}).to_string(),
                true,
            )
        })
        .await
        .unwrap();
    f.store.mutate(|s| s.set_mcp_server_enabled(&bot, "apps", true, true)).await.unwrap();
    let id = f.conversation(&bot).await;
    f.provider.plan(vec![
        discover("apps render counter"),
        Reply::Tool("use_tool", json!({"tool_name": "apps__render", "tool_input": {"delta": 0, "quoted": SECRET}})),
        text("The app is ready."),
    ]);
    f.runtime.start(&id, "Show my counter app").await.unwrap();
    f.decide(&id, "allow_once").await;
    let completed = f.completed(&id).await;
    let tool = completed.messages.iter().flat_map(|message| &message.tools).find(|tool| tool.mcp_app.is_some()).unwrap_or_else(|| {
        panic!(
            "MCP app captured on its tool activity: {:?}; reads: {:?}",
            completed.messages.iter().flat_map(|message| &message.tools).collect::<Vec<_>>(),
            recorded.lock().unwrap().reads
        )
    });
    let app = tool.mcp_app.as_ref().unwrap();
    assert_eq!(app.bot_id, bot);
    assert_eq!(app.server, "apps");
    assert_eq!(app.tool, "render");
    assert_eq!(app.uri, URI);
    assert!(app.html.contains("Counter app") && app.html.contains("[redacted]"));
    assert_eq!(serde_json::from_str::<Value>(&app.input).unwrap(), json!({"delta": 0, "quoted": "[redacted]"}));
    assert!(app.output.contains("counter: 1") && !app.output.contains(SECRET));
    assert!(!app.output.contains("privateResultMeta"));
    assert_eq!(recorded.lock().unwrap().reads, [URI]);
    assert_eq!(recorded.lock().unwrap().calls.len(), 1);
    let run = completed.run.clone();
    let model_calls = f.provider.inference_len();

    // Auto-allow rules are deliberately irrelevant to app-originated tool proposals.
    f.store
        .mutate(|s| {
            s.add_permission_rule(
                PermissionRuleInput {
                    category: PermissionCategory::McpTool,
                    action: PermissionAction::Allow,
                    bot_id: Some(bot.clone()),
                    server: Some("apps".into()),
                },
                true,
            )
        })
        .await
        .unwrap();
    let denied = f.runtime.request_mcp_app_call(&id, &tool.id, "render", json!({"delta": 1})).unwrap();
    assert_eq!(recorded.lock().unwrap().calls.len(), 1, "proposing starts no server call");
    let result = f.runtime.decide_mcp_app_call(&id, &tool.id, &denied, false).await.unwrap();
    assert_eq!(result["isError"], true);
    assert!(f.runtime.decide_mcp_app_call(&id, &tool.id, &denied, true).await.is_err());
    assert_eq!(recorded.lock().unwrap().calls.len(), 1, "deny/replay never executes");

    let approved = f.runtime.request_mcp_app_call(&id, &tool.id, "refresh", json!({})).unwrap();
    let result = f.runtime.decide_mcp_app_call(&id, &tool.id, &approved, true).await.unwrap();
    assert_eq!(result["content"][0]["text"], "counter: 1 [redacted]");
    assert!(result.get("structuredContent").is_none() && result.get("_meta").is_none());
    assert_eq!(recorded.lock().unwrap().calls.len(), 2);
    assert_eq!(recorded.lock().unwrap().calls.last().unwrap()["name"], "refresh");
    assert_eq!(f.provider.inference_len(), model_calls, "app calls never start an LLM prompt");
    assert_eq!(f.get(&id).run, run, "app calls do not replace Conversation.run");

    for target in ["unknown", "model_only"] {
        let request = f.runtime.request_mcp_app_call(&id, &tool.id, target, json!({})).unwrap();
        assert!(f.runtime.decide_mcp_app_call(&id, &tool.id, &request, true).await.is_err());
    }
    assert_eq!(recorded.lock().unwrap().calls.len(), 2, "catalog and visibility admission fail closed");
    assert!(f.runtime.request_mcp_app_call(&id, &tool.id, "other__tool", json!({})).is_err());
    assert!(f.runtime.request_mcp_app_call(&id, &tool.id, "render", json!({"large": "한".repeat(12_000)})).is_err());

    maintenance_admission(f, &recorded, &id, &tool.id).await;

    // Two slow resources used to stall the single event router past its five-second drain and
    // lose the final assistant text. Captures are concurrent tracked work, not router awaits.
    // Issue #20 protects the active Bot's settings; another idle Bot can still rotate its secrets.
    let idle_bot = f
        .store
        .mutate(|state| state.add_bot(BotInput { name: "Idle secret owner".into(), instructions: None, color: None, shape: None, pinned: None }))
        .await
        .unwrap();
    recorded.lock().unwrap().slow_reads = true;
    f.provider.plan(vec![
        Reply::Tool("use_tool", json!({"tool_name": "apps__render", "tool_input": {"delta": 0, "quoted": "late-vault-token-123456"}})),
        Reply::Tool("use_tool", json!({"tool_name": "apps__render", "tool_input": {"delta": 0, "quoted": "late-vault-token-123456"}})),
        text("Both slow apps finished; this final answer must not disappear."),
    ]);
    f.runtime.start(&id, "Show two slow apps").await.unwrap();
    let streamed = f
        .wait(&id, "the final text before HTML capture finishes", |conversation| {
            conversation.messages.last().is_some_and(|message| message.text.contains("Both slow apps finished"))
        })
        .await;
    assert!(!streamed.run.status.is_terminal(), "normal text projects before slow resource work completes");
    let busy_secret = f
        .store
        .mutate(|state| state.add_secret(SecretInput { name: "LATE_TOKEN".into(), value: "late-vault-token-123456".into(), bot_id: Some(bot.clone()) }, true))
        .await
        .unwrap_err();
    assert_eq!(busy_secret.status, 409, "slow capture still owns the active Bot's configuration slot");
    let late_secret = f
        .store
        .mutate(|state| {
            state.add_secret(SecretInput { name: "LATE_TOKEN".into(), value: "late-vault-token-123456".into(), bot_id: Some(idle_bot.id.clone()) }, true)
        })
        .await
        .unwrap();
    let slow = f.completed(&id).await;
    assert!(slow.messages.last().unwrap().text.contains("Both slow apps finished"));
    assert_eq!(slow.messages.last().unwrap().tools.iter().filter(|tool| tool.mcp_app.is_some()).count(), 2);
    f.store.mutate(|state| state.delete_secret(&late_secret.id, true)).await.unwrap();
    for app in f.get(&id).messages.last().unwrap().tools.iter().filter_map(|tool| tool.mcp_app.as_ref()) {
        assert_eq!(serde_json::from_str::<Value>(&app.input).unwrap()["quoted"], "[redacted]", "input is redacted durably with keys added during resource IO");
    }
    recorded.lock().unwrap().slow_reads = false;
    let before_rejections = recorded.lock().unwrap().calls.len();

    // A request's private resolution snapshot detects secret rotation, without any hash/value in
    // the durable app. A later fresh request may use the freshly configured credentials.
    let rotation = f.runtime.request_mcp_app_call(&id, &tool.id, "render", json!({})).unwrap();
    let secret = f.store.read(|s| s.secrets().iter().find(|secret| secret.name == "APPS_TOKEN").unwrap().id.clone());
    f.store.mutate(|s| s.replace_secret(&secret, "new-app-token-after-rotation", true)).await.unwrap();
    assert!(f.runtime.decide_mcp_app_call(&id, &tool.id, &rotation, true).await.is_err());
    assert_eq!(recorded.lock().unwrap().calls.len(), before_rejections);

    let stale = f.runtime.request_mcp_app_call(&id, &tool.id, "render", json!({})).unwrap();
    f.store.mutate(|s| s.set_mcp_server_enabled(&bot, "apps", false, true)).await.unwrap();
    assert!(f.runtime.decide_mcp_app_call(&id, &tool.id, &stale, true).await.is_err());
    assert_eq!(recorded.lock().unwrap().calls.len(), before_rejections);
    assert!(f.get(&id).messages.iter().flat_map(|message| &message.tools).any(|tool| tool.mcp_app.is_some()), "tightening preserves rendered history");
}

async fn maintenance_admission(f: &Fixture, recorded: &Arc<Mutex<Recorded>>, conversation: &str, activity: &str) {
    let calls = recorded.lock().unwrap().calls.len();
    let model_calls = f.provider.inference_len();
    let late = f.runtime.request_mcp_app_call(conversation, activity, "refresh", json!({})).unwrap();
    let before = f.store.snapshot();
    f.runtime.pause_if_idle().unwrap();
    assert_eq!(f.runtime.request_mcp_app_call(conversation, activity, "refresh", json!({})).unwrap_err().status, 503);
    assert_eq!(f.runtime.decide_mcp_app_call(conversation, activity, &late, true).await.unwrap_err().status, 503);
    assert_eq!(
        f.runtime.decide_mcp_app_call(conversation, activity, &late, true).await.unwrap_err().status,
        409,
        "a paused approval is consumed, not replayed"
    );
    assert_eq!(recorded.lock().unwrap().calls.len(), calls, "late admission made no MCP call during maintenance");
    assert_eq!(f.store.snapshot(), before, "paused app admission changed durable state");
    f.runtime.resume().unwrap();

    // An isolated app session leaves Conversation.run terminal. Its private busy owner still
    // blocks maintenance and live reply dispatch, but issue #20 accepts replies into the durable queue.
    let gate = Arc::new(CallGate::default());
    recorded.lock().unwrap().call_gate = Some(gate.clone());
    let approved = f.runtime.request_mcp_app_call(conversation, activity, "refresh", json!({})).unwrap();
    let (runtime, id, tool) = (f.runtime.clone(), conversation.to_owned(), activity.to_owned());
    let running = tokio::spawn(async move { runtime.decide_mcp_app_call(&id, &tool, &approved, true).await });
    tokio::time::timeout(Duration::from_secs(10), gate.entered.notified()).await.expect("isolated app call started");
    assert!(f.get(conversation).run.status.is_terminal());
    assert_eq!(f.runtime.pause_if_idle().unwrap_err().status, 409);
    assert!(!f.runtime.is_paused(), "busy maintenance did not latch");
    assert_eq!(f.store.snapshot(), before, "busy maintenance changed durable state");
    f.runtime.start(conversation, "Must not overlap an MCP app session").await.unwrap();
    let queued = f.public().work_queue.into_iter().find(|work| work.conversation_id == conversation).expect("reply waits for the isolated app session");
    assert!(matches!(&queued.kind, WorkKind::User { text, attachments } if text == "Must not overlap an MCP app session" && attachments.is_empty()));
    assert_eq!(f.store.snapshot().conversations, before.conversations, "queue acceptance does not replace any conversation run or history");
    assert_eq!(f.store.read(|state| state.active_count()), 0, "the queued reply cannot overlap the isolated app call");
    assert_eq!(f.provider.inference_len(), model_calls, "durable acceptance does not prompt the model while the app call is live");
    // Cancel only this accepted request before releasing the app so the remaining app scenarios stay isolated.
    f.runtime.cancel_queued_turn(conversation, &queued.id).await.unwrap();
    let after_cancel = f.store.snapshot();
    let mut unchanged = after_cancel.clone();
    unchanged.revision = before.revision;
    assert_eq!(unchanged, before, "queue acceptance/removal changes no unrelated durable state");
    gate.release.notify_one();
    let result = running.await.unwrap().unwrap();
    recorded.lock().unwrap().call_gate = None;
    assert_eq!(result["content"][0]["text"], "counter: 1 [redacted]");
    assert_eq!(recorded.lock().unwrap().calls.len(), calls + 1);
    assert_eq!(f.provider.inference_len(), model_calls, "maintenance checks and isolated app calls never prompt the model");
    assert_eq!(f.store.snapshot(), after_cancel, "isolated app call replaced persisted conversation state");
    f.runtime.pause_if_idle().unwrap();
    f.runtime.resume().unwrap();
}
