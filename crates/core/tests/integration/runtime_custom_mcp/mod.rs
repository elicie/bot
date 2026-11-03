//! Runtime scenario for vault secrets and custom MCP servers, included by `runtime.rs` (the engine
//! environment is process-global, so it runs in that binary's sequential scenario list).
//!
//! Fixtures: an HTTP MCP server (records request headers) and a stdio MCP server (a `sh` script
//! that records its environment to a file outside the data directory). Both receive the vault
//! secret through `${secret:NAME}`; the model, the snapshots, and the data directory apart from
//! `state.json` must never contain it.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use grok_bot_model::{PermissionAction, PermissionCategory, PermissionRuleInput, Role, RunStatus, SecretInput, ToolStatus};
use serde_json::{Value, json};

use super::{Fixture, Reply, content_text, discover, files_containing, messages, serve, text, tool_outcomes};

const SECRET: &str = "fixture-vault-secret-9876";

#[derive(Default)]
struct HttpFixture {
    /// `Authorization` header of every request.
    authorization: Vec<String>,
    calls: Vec<Value>,
}

async fn http_mcp(State(state): State<Arc<Mutex<HttpFixture>>>, headers: HeaderMap, Json(request): Json<Value>) -> Response {
    let authorization = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
    state.lock().unwrap().authorization.push(authorization);
    let Some(id) = request.get("id").cloned() else { return StatusCode::ACCEPTED.into_response() };
    let result = match request["method"].as_str().unwrap_or_default() {
        "initialize" => {
            json!({"protocolVersion": request["params"]["protocolVersion"], "capabilities": {"tools": {"listChanged": false}}, "serverInfo": {"name": "http-fixture", "version": "1.0.0"}})
        }
        "ping" => json!({}),
        "tools/list" => {
            json!({"tools": [{"name": "lookup", "description": "Look up a fixture record.", "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"], "additionalProperties": false}}]})
        }
        "tools/call" => {
            state.lock().unwrap().calls.push(request["params"].clone());
            let query = request["params"]["arguments"]["query"].as_str().unwrap_or_default();
            json!({"content": [{"type": "text", "text": format!("http-fixture-ok:{query}")}]})
        }
        _ => return Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}})).into_response(),
    };
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

/// A line-oriented JSON-RPC MCP server in POSIX `sh`. `tools/call` writes `$FIXTURE_TOKEN` to
/// `$FIXTURE_OUT` and answers without echoing it.
const STDIO_SERVER: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/^{[^}]*"id":\([0-9][0-9]*\).*/\1/p')
  method=$(printf '%s\n' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  [ -z "$id" ] && continue
  case "$method" in
    initialize)
      version=$(printf '%s\n' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"%s","capabilities":{"tools":{}},"serverInfo":{"name":"stdio-fixture","version":"1.0.0"}}}\n' "$id" "$version" ;;
    ping) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
    tools/list)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"whoami","description":"Report the stdio fixture status.","inputSchema":{"type":"object","properties":{}}}]}}\n' "$id" ;;
    tools/call)
      printf '%s' "$FIXTURE_TOKEN" > "$FIXTURE_OUT"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"stdio-fixture-ok"}]}}\n' "$id" ;;
    *) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"Method not found"}}\n' "$id" ;;
  esac
done
"#;

fn call(tool: &str, input: Value) -> Reply {
    Reply::Tool("use_tool", json!({"tool_name": tool, "tool_input": input}))
}

fn system_rules(requests: &[Value]) -> String {
    requests.iter().flat_map(messages).filter(|m| m["role"] == "system").map(|m| content_text(&m["content"])).collect()
}

async fn set_servers(f: &Fixture, bot: &str, document: Value) {
    let (bot, json) = (bot.to_owned(), document.to_string());
    f.store.mutate(move |s| s.set_mcp_servers(&bot, &json, true)).await.unwrap();
}

async fn enable(f: &Fixture, bot: &str, names: &[&'static str]) {
    for name in names {
        let bot = bot.to_owned();
        f.store.mutate(move |s| s.set_mcp_server_enabled(&bot, name, true, true)).await.unwrap();
    }
}

pub(super) async fn custom_mcp_servers(f: &Fixture) {
    f.configure("", false).await;
    let bot = f.bot_id();
    let fixture = Arc::new(Mutex::new(HttpFixture::default()));
    let origin = serve(Router::new().route("/mcp", post(http_mcp).get(|| async { StatusCode::METHOD_NOT_ALLOWED })).with_state(Arc::clone(&fixture))).await;
    let script = f.root.join("stdio-fixture.sh");
    std::fs::write(&script, STDIO_SERVER).unwrap();
    let out = f.root.join("stdio-token.txt");
    f.store.mutate(|s| s.add_secret(SecretInput { name: "FIXTURE_TOKEN".into(), value: SECRET.into(), bot_id: None }, true)).await.unwrap();
    set_servers(
        f,
        &bot,
        json!({"mcpServers": {
            "web": {"url": format!("{origin}/mcp"), "headers": {"Authorization": "Bearer ${secret:FIXTURE_TOKEN}"}},
            "local": {"command": "/bin/sh", "args": [script.to_string_lossy()], "env": {"FIXTURE_TOKEN": "${secret:FIXTURE_TOKEN}", "FIXTURE_OUT": out.to_string_lossy()}},
            "missing": {"url": format!("{origin}/mcp"), "headers": {"Authorization": "Bearer ${secret:NOT_IN_VAULT}"}},
            "down": {"command": "/nonexistent/grok-bot-fixture-server"},
            "offline": {"url": "http://127.0.0.1:9/mcp"},
        }}),
    )
    .await;
    let id = f.conversation(&bot).await;

    // Disabled by default: no custom servers reach the session.
    f.provider.clear();
    f.provider.plan(vec![text("Nothing custom yet.")]);
    f.runtime.start(&id, "Hello before enabling").await.unwrap();
    f.completed(&id).await;
    assert!(!system_rules(&f.provider.inference()).contains("custom MCP servers"));
    assert!(fixture.lock().unwrap().authorization.is_empty(), "a disabled server is never contacted");
    let before = f.get(&id).session_fingerprint.clone();

    // Enabled: tools are reachable through search_tool/use_tool, every call shows a card, and the
    // secret reaches the servers only.
    enable(f, &bot, &["web", "local", "missing", "down", "offline"]).await;
    f.provider.clear();
    f.provider.plan(vec![
        discover("web lookup local whoami"),
        call("web__lookup", json!({"query": "alpha"})),
        call("local__whoami", json!({})),
        text("Used both custom servers."),
    ]);
    f.runtime.start(&id, "Use my custom servers").await.unwrap();
    let card = f.decide(&id, "allow_once").await;
    assert_eq!(card.title, "MCP 서버 web: lookup");
    assert_eq!(card.options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), ["Run once", "Deny"]);
    assert!(card.description.contains("alpha"));
    let card = f.decide(&id, "allow_once").await;
    assert_eq!(card.title, "MCP 서버 local: whoami");
    let used = f.completed(&id).await;
    assert_ne!(used.session_fingerprint, before, "enabling servers replaces the session");
    let requests = f.provider.inference();
    let outcomes = tool_outcomes(&requests);
    assert!(outcomes.iter().any(|o| o.contains("web__lookup")) && outcomes.iter().any(|o| o.contains("local__whoami")), "{outcomes:?}");
    assert!(outcomes.iter().any(|o| o.contains("http-fixture-ok:alpha")), "{outcomes:?}");
    assert!(outcomes.iter().any(|o| o.contains("stdio-fixture-ok")), "{outcomes:?}");
    assert!(fixture.lock().unwrap().authorization.iter().all(|value| value == &format!("Bearer {SECRET}")));
    assert_eq!(fixture.lock().unwrap().calls.len(), 1);
    assert_eq!(std::fs::read_to_string(&out).unwrap(), SECRET, "the stdio server received the secret in its environment");
    let rules = system_rules(&requests);
    assert!(rules.contains("custom MCP servers for this bot: local (stdio), offline (http), web (http)."), "missing secrets and commands are left out");
    // Startup problems are activities, not a failed turn.
    let tools = &used.messages.last().unwrap().tools;
    let failed: Vec<&str> = tools.iter().filter(|t| t.status == ToolStatus::Failed).map(|t| t.title.as_str()).collect();
    for name in ["missing", "down", "offline"] {
        assert!(failed.contains(&format!("MCP 서버 {name} 시작 실패").as_str()), "{name}: {tools:?}");
    }
    assert!(tools.iter().any(|t| t.title == "MCP 서버 missing 시작 실패" && t.output.as_deref().is_some_and(|o| o.contains("NOT_IN_VAULT"))));
    assert_eq!(used.run.status, RunStatus::Completed);

    // A rule allows one server without asking; the other server still asks.
    let rule = {
        let input = PermissionRuleInput {
            category: PermissionCategory::McpTool,
            action: PermissionAction::Allow,
            bot_id: Some(bot.clone()),
            server: Some("web".into()),
        };
        f.store.mutate(move |s| s.add_permission_rule(input, true)).await.unwrap()
    };
    f.provider.plan(vec![call("web__lookup", json!({"query": "beta"})), call("local__whoami", json!({})), text("Rule applied.")]);
    f.runtime.start(&id, "Again with my rule").await.unwrap();
    let card = f.decide(&id, "reject_once").await;
    assert_eq!(card.title, "MCP 서버 local: whoami");
    let ruled = f.completed(&id).await;
    assert_eq!(ruled.messages.last().unwrap().text, "Rule applied.", "declining a custom tool continues the turn");
    assert!(tool_outcomes(&f.provider.inference()).iter().any(|o| o.contains("avoid running")), "the model learns the call was declined");
    let tools = &ruled.messages.last().unwrap().tools;
    let lookup = tools.iter().find(|t| t.approved_by_rule.is_some()).unwrap_or_else(|| panic!("auto-approved activity: {tools:?}"));
    assert_eq!(lookup.approved_by_rule.as_deref(), Some(rule.id.as_str()));
    assert_eq!(lookup.status, ToolStatus::Completed);
    assert_eq!(fixture.lock().unwrap().calls.len(), 2);

    // Secrets stay out of the model, snapshots and engine files. The primary private state
    // and its required create-once private rollback backup intentionally retain stored values.
    let everything = serde_json::to_string(&f.provider.inference()).unwrap();
    assert!(!everything.contains(SECRET));
    assert!(!serde_json::to_string(&f.public()).unwrap().contains(SECRET));
    let data_dir = f.root.join("data");
    // The terminal run is visible in memory before its atomic write finishes. Scan only after
    // that write drains, otherwise its private state.json.<uuid>.tmp can appear mid-replacement.
    f.store.flush().await.unwrap();
    let backup = data_dir.join(grok_bot_store::PRE_WORK_QUEUE_BACKUP);
    let mut stored = files_containing(&data_dir, SECRET.as_bytes());
    stored.sort();
    assert_eq!(stored, [data_dir.join("state.json"), backup.clone()]);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(backup).unwrap().permissions().mode() & 0o777, 0o600);
    let engine_home = std::env::var("GROK_HOME").map(std::path::PathBuf::from).unwrap();
    let leaked = files_containing(engine_home.parent().unwrap(), SECRET.as_bytes());
    assert!(leaked.is_empty(), "engine files with the secret: {leaked:?}");

    // A model answer quoting the secret is redacted in storage and in the snapshot.
    f.provider.plan(vec![Reply::Chunks(vec![format!("The token is {}", &SECRET[..10]), format!("{} done", &SECRET[10..])])]);
    f.runtime.start(&id, "Repeat it").await.unwrap();
    let quoted = f.completed(&id).await;
    assert_eq!(quoted.messages.last().unwrap().text, "The token is [redacted] done");

    recipients_get_no_custom_servers(f, &bot, &origin, &fixture).await;
}

/// Bot-message recipient conversations never get custom servers, even when the recipient bot has
/// enabled ones.
async fn recipients_get_no_custom_servers(f: &Fixture, sender: &str, origin: &str, fixture: &Arc<Mutex<HttpFixture>>) {
    let recipient = f
        .store
        .mutate(|s| s.add_bot(grok_bot_model::BotInput { name: "Recipient".into(), color: None, instructions: None, shape: None, pinned: None }))
        .await
        .unwrap()
        .id;
    set_servers(f, &recipient, json!({"web": {"url": format!("{origin}/mcp"), "headers": {"Authorization": "Bearer ${secret:FIXTURE_TOKEN}"}}})).await;
    enable(f, &recipient, &["web"]).await;
    let source = f.conversation(sender).await;
    f.provider.plan(vec![
        Reply::Tool("use_tool", json!({"tool_name": "workspace__send_message", "tool_input": {"toBotId": recipient, "text": "Please look something up."}})),
        text("Sent."),
    ]);
    f.runtime.start(&source, "Message the recipient").await.unwrap();
    f.decide(&source, "allow_once").await;
    f.completed(&source).await;
    let delivery = f.store.read(|s| s.disk().bot_deliveries.last().cloned()).expect("delivery");
    let contacted = fixture.lock().unwrap().authorization.len();
    f.provider.clear();
    f.provider.plan(vec![text("Recipient answer.")]);
    f.runtime.start(&delivery.recipient_conversation_id, &delivery.text).await.unwrap();
    let answered = f.completed(&delivery.recipient_conversation_id).await;
    assert_eq!(answered.messages.last().unwrap().role, Role::Assistant);
    assert!(!system_rules(&f.provider.inference()).contains("custom MCP servers"));
    assert_eq!(fixture.lock().unwrap().authorization.len(), contacted, "the recipient session never contacted the server");
}
