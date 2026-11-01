//! P0 spike: the Grok Build agent runs inside this process over in-memory ACP.
//! Covers one-turn chat, MCP tool calls with permission round trips, concurrent sessions with
//! per-session models, cancellation, and session load after an engine restart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_client_protocol as acp;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use grok_bot_engine::host::{EngineEvent, EngineHost, EngineSettings, ModelSpec, prepare_process_environment};
use serde_json::{Value, json};

#[derive(Default)]
struct Recorded {
    /// (model, latest user text) per chat-completions request.
    completions: Vec<(String, String)>,
    /// Tool names offered on main agent turns.
    offered_tools: std::collections::BTreeSet<String>,
    /// Tool names offered on auxiliary requests (e.g. session titles).
    auxiliary_tools: std::collections::BTreeSet<String>,
    mcp_calls: Vec<Value>,
}
type Shared = Arc<Mutex<Recorded>>;

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn sse(model: &str, delta: Value, finish: &str) -> Response {
    let chunk = |delta: Value, reason: Value| json!({"id": "chatcmpl-p0", "object": "chat.completion.chunk", "created": 1, "model": model, "choices": [{"index": 0, "delta": delta, "finish_reason": reason}]});
    let mut body = String::new();
    for value in [
        chunk(json!({"role": "assistant"}), Value::Null),
        chunk(delta, Value::Null),
        {
            let mut last = chunk(json!({}), json!(finish));
            last["usage"] = json!({"prompt_tokens": 100, "completion_tokens": 12, "total_tokens": 112});
            last
        },
    ] {
        body.push_str(&format!("data: {value}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

fn text(model: &str, text: &str) -> Response {
    sse(model, json!({"content": text}), "stop")
}

fn tool(model: &str, id: usize, name: &str, arguments: Value) -> Response {
    sse(model, json!({"tool_calls": [{"index": 0, "id": format!("call_p0_{id}"), "type": "function", "function": {"name": name, "arguments": arguments.to_string()}}]}), "tool_calls")
}

async fn completions(State(state): State<Shared>, Json(request): Json<Value>) -> Response {
    let model = request["model"].as_str().unwrap_or_default().to_owned();
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let names: Vec<String> = request["tools"].as_array().into_iter().flatten()
        .filter_map(|t| t["function"]["name"].as_str().map(str::to_owned)).collect();
    let latest = messages.iter().rposition(|m| m["role"] == "user");
    let user = latest.map(|i| content_text(&messages[i]["content"])).unwrap_or_default();
    let count = {
        let mut recorded = state.lock().unwrap();
        if names.iter().any(|n| n == "search_tool") {
            recorded.offered_tools.extend(names.iter().cloned());
        } else {
            recorded.auxiliary_tools.extend(names.iter().cloned());
        }
        recorded.completions.push((model.clone(), user.clone()));
        recorded.completions.len()
    };
    if !names.iter().any(|n| n == "search_tool") {
        return text(&model, "P0 title");
    }
    let after: Vec<String> = latest.map(|i| &messages[i + 1..]).unwrap_or(&[]).iter()
        .flat_map(|m| m["tool_calls"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["function"]["name"].as_str().map(str::to_owned)).collect();
    if user.contains("SLOW") {
        tokio::time::sleep(Duration::from_secs(60)).await;
        return text(&model, "slow finished");
    }
    if user.contains("DELAY") {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        return text(&model, &format!("delayed:{model}"));
    }
    if user.contains("TOOL") {
        if !after.iter().any(|n| n == "search_tool") {
            return tool(&model, count, "search_tool", json!({"query": "workspace echo", "limit": 3}));
        }
        if !after.iter().any(|n| n == "use_tool") {
            return tool(&model, count, "use_tool", json!({"tool_name": "workspace__echo", "tool_input": {"text": "ping"}}));
        }
        return text(&model, "tool-done");
    }
    if user.contains("RECALL") {
        let all = messages.iter().map(|m| content_text(&m["content"])).collect::<Vec<_>>().join("\n");
        return text(&model, if all.contains("FIRST-TURN-SECRET") { "recall-ok" } else { "recall-missing" });
    }
    text(&model, &format!("echo:{model}"))
}

async fn models() -> Json<Value> {
    Json(json!({"object": "list", "data": [{"id": "model-a", "object": "model"}, {"id": "model-b", "object": "model"}]}))
}

async fn mcp(State(state): State<Shared>, Json(request): Json<Value>) -> Response {
    let Some(id) = request.get("id").cloned() else { return StatusCode::ACCEPTED.into_response() };
    let result = match request["method"].as_str().unwrap_or_default() {
        "initialize" => json!({"protocolVersion": request["params"]["protocolVersion"], "capabilities": {"tools": {"listChanged": false}}, "serverInfo": {"name": "p0-workspace", "version": "1.0.0"}}),
        "ping" => json!({}),
        "tools/list" => json!({"tools": [{"name": "echo", "description": "Echo text back.", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"], "additionalProperties": false}}]}),
        "tools/call" => {
            state.lock().unwrap().mcp_calls.push(request["params"].clone());
            let said = request["params"]["arguments"]["text"].as_str().unwrap_or_default();
            json!({"content": [{"type": "text", "text": format!("echo:{said}")}]})
        }
        _ => return Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}})).into_response(),
    };
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

async fn start_fixture(state: Shared) -> String {
    let app = Router::new()
        .route("/v1/chat/completions", post(completions))
        .route("/v1/models", get(models))
        .route("/mcp", post(mcp).get(|| async { StatusCode::METHOD_NOT_ALLOWED }))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    origin
}

#[derive(Default)]
struct Observed {
    text: HashMap<String, String>,
    permissions: usize,
}

fn pump(mut events: tokio::sync::mpsc::UnboundedReceiver<EngineEvent>, observed: Arc<Mutex<Observed>>) {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                EngineEvent::Update(notification) => {
                    if let acp::SessionUpdate::AgentMessageChunk(chunk) = notification.update
                        && let acp::ContentBlock::Text(text) = chunk.content
                    {
                        observed.lock().unwrap().text.entry(notification.session_id.0.to_string()).or_default().push_str(&text.text);
                    }
                }
                EngineEvent::Permission { request, respond } => {
                    observed.lock().unwrap().permissions += 1;
                    let allow = request.options.iter().find(|o| o.kind == acp::PermissionOptionKind::AllowOnce);
                    let outcome = match allow {
                        Some(option) => acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(option.option_id.clone())),
                        None => acp::RequestPermissionOutcome::Cancelled,
                    };
                    let _ = respond.send(acp::RequestPermissionResponse::new(outcome));
                }
            }
        }
    });
}

fn take_text(observed: &Arc<Mutex<Observed>>, session: &acp::SessionId) -> String {
    observed.lock().unwrap().text.remove(&session.0.to_string()).unwrap_or_default()
}

fn workspace(root: &Path) -> PathBuf {
    let cwd = root.join("workspace");
    for dir in [".git/objects", ".git/refs/heads"] {
        std::fs::create_dir_all(cwd.join(dir)).unwrap();
    }
    std::fs::write(cwd.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(cwd.join(".git/config"), "[core]\nrepositoryformatversion = 0\nbare = false\n").unwrap();
    cwd
}

fn session_request(cwd: &Path, origin: &str) -> acp::NewSessionRequest {
    let meta = json!({
        "yoloMode": false, "autoMode": false,
        "rules": "You are a P0 test assistant. Use only the workspace MCP server.",
        "agentProfile": {
            "name": "grok-bot-scoped", "description": "Workspace broker only", "promptMode": "extend",
            "injectDefaultTools": false, "agentsMd": false,
            "toolConfig": {"tools": [{"id": "GrokBuild:search_tool"}, {"id": "GrokBuild:use_tool"}]},
            "tools": ["search_tool", "use_tool"],
            "disallowedTools": ["run_terminal_cmd", "task", "workflow", "read_file", "list_dir", "grep", "write", "search_replace", "delete_file", "Agent"],
            "discoverSkills": false, "inheritSkills": false, "permissionMode": "default"
        }
    });
    acp::NewSessionRequest::new(cwd.to_path_buf())
        .mcp_servers(vec![acp::McpServer::Http(
            acp::McpServerHttp::new("workspace", format!("{origin}/mcp"))
                .headers(vec![acp::HttpHeader::new("Authorization", "Bearer p0-token")]),
        )])
        .meta(meta.as_object().cloned())
}

fn prompt(session: &acp::SessionId, text: &str) -> acp::PromptRequest {
    acp::PromptRequest::new(session.clone(), vec![acp::ContentBlock::Text(acp::TextContent::new(text))])
}

fn settings(root: &Path, origin: &str) -> EngineSettings {
    let model = |id: &str, name: &str| ModelSpec { id: id.into(), model: name.into(), base_url: format!("{origin}/v1"), api_key: Some("p0-secret-key".into()) };
    EngineSettings {
        grok_home: root.join("engine/home/.grok"),
        models: vec![model("a", "model-a"), model("b", "model-b")],
        default_model: "a".into(),
        allow: vec![],
        inference_idle_timeout_secs: 120,
    }
}

#[test]
fn in_process_engine_p0() {
    let root = std::env::temp_dir().join(format!("grok-bot-p0-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // SAFETY: set before the test runtime and engine threads start.
    unsafe { prepare_process_environment(&root.join("engine")) };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let recorded: Shared = Arc::default();
        let origin = start_fixture(recorded.clone()).await;
        let cwd = workspace(&root);
        let observed: Arc<Mutex<Observed>> = Arc::default();

        let started = Instant::now();
        let (host, events) = EngineHost::start(settings(&root, &origin)).await.expect("engine starts");
        eprintln!("[p0] engine start {:?}, loadSession={}", started.elapsed(), host.initialize.agent_capabilities.load_session);
        assert!(host.initialize.agent_capabilities.load_session);
        pump(events, observed.clone());

        // A. one turn
        let first = host.new_session(session_request(&cwd, &origin)).await.expect("new session").session_id;
        let t = Instant::now();
        let response = host.prompt(prompt(&first, "hello FIRST-TURN-SECRET")).await.expect("prompt");
        assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
        let said = take_text(&observed, &first);
        eprintln!("[p0] A one turn {:?}: {said:?}", t.elapsed());
        assert!(said.contains("echo:model-a"), "{said}");

        // B. MCP tool through search_tool/use_tool, with permission round trip
        let response = host.prompt(prompt(&first, "please TOOL now")).await.expect("tool prompt");
        assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
        let said = take_text(&observed, &first);
        let calls = recorded.lock().unwrap().mcp_calls.clone();
        let permissions = observed.lock().unwrap().permissions;
        eprintln!("[p0] B tool: text={said:?} mcp_calls={calls:?} permissions={permissions}");
        assert!(said.contains("tool-done"), "{said}");
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0]["name"], "echo");
        let (offered, auxiliary) = { let r = recorded.lock().unwrap(); (r.offered_tools.clone(), r.auxiliary_tools.clone()) };
        eprintln!("[p0] B offered tools: main={offered:?} auxiliary={auxiliary:?}");
        assert_eq!(offered.into_iter().collect::<Vec<_>>(), ["search_tool", "use_tool"]);

        // C. two concurrent sessions, second on model b
        let second = host.new_session(session_request(&cwd, &origin)).await.expect("second session").session_id;
        host.set_session_model(acp::SetSessionModelRequest::new(second.clone(), "b")).await.expect("set model");
        let t = Instant::now();
        let (one, two) = tokio::join!(host.prompt(prompt(&first, "DELAY one")), host.prompt(prompt(&second, "DELAY two")));
        let elapsed = t.elapsed();
        assert_eq!(one.unwrap().stop_reason, acp::StopReason::EndTurn);
        assert_eq!(two.unwrap().stop_reason, acp::StopReason::EndTurn);
        let (said_one, said_two) = (take_text(&observed, &first), take_text(&observed, &second));
        eprintln!("[p0] C concurrent {elapsed:?}: one={said_one:?} two={said_two:?}");
        assert!(said_one.contains("delayed:model-a"), "{said_one}");
        assert!(said_two.contains("delayed:model-b"), "{said_two}");
        assert!(elapsed < Duration::from_millis(2900), "sessions ran serially: {elapsed:?}");

        // D. cancellation
        let t = Instant::now();
        let slow = host.prompt(prompt(&second, "SLOW please"));
        let cancel = async {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            host.cancel(second.clone()).await.expect("cancel");
        };
        let (slow, ()) = tokio::join!(slow, cancel);
        eprintln!("[p0] D cancel {:?}: {:?}", t.elapsed(), slow.as_ref().map(|r| r.stop_reason));
        assert_eq!(slow.expect("cancelled prompt").stop_reason, acp::StopReason::Cancelled);
        assert!(t.elapsed() < Duration::from_secs(6));
        take_text(&observed, &second);

        // E. engine restart, then load and continue the first session
        let t = Instant::now();
        host.shutdown();
        let (host, events) = EngineHost::start(settings(&root, &origin)).await.expect("engine restarts");
        pump(events, observed.clone());
        host.load_session(acp::LoadSessionRequest::new(first.clone(), cwd.clone()).mcp_servers(session_request(&cwd, &origin).mcp_servers).meta(session_request(&cwd, &origin).meta))
            .await.expect("load session");
        take_text(&observed, &first); // replayed history
        let response = host.prompt(prompt(&first, "RECALL it")).await.expect("recall");
        assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
        let said = take_text(&observed, &first);
        eprintln!("[p0] E restart+load {:?}: {said:?}", t.elapsed());
        assert!(said.contains("recall-ok"), "{said}");

        let models: Vec<String> = recorded.lock().unwrap().completions.iter().map(|(m, _)| m.clone()).collect();
        eprintln!("[p0] provider models seen: {models:?}");
        host.shutdown();
    });
    let _ = std::fs::remove_dir_all(&root);
}
