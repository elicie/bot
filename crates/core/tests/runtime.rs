//! Runtime integration tests against the real in-process Grok Build engine and a mock
//! OpenAI-compatible provider. Ports the runtime-level cases of `tests/backend-runtime.test.ts`,
//! `tests/backend-grok.test.ts`, `tests/runtime.integration.test.ts`, and the runtime parts of
//! `tests/skills.test.ts` and `tests/bot-messages.test.ts` (managed-computer cases excluded).
//!
//! Engine configuration and `GROK_HOME` are process-global, so this binary holds one `#[test]` that
//! prepares the environment first and then runs every scenario sequentially (like the P0 test).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use grok_bot_core::queue::CommandQueue;
use grok_bot_core::runtime::{Runtime, RuntimeOptions};
use grok_bot_model::{
    AppState, BotInput, Conversation, DeliveryStatus, Role, RunStatus, SettingsInput, SkillInput,
    SkillPatch, system_clock,
};
use grok_bot_store::Store;
use serde_json::{Value, json};

#[path = "runtime_bot_data/mod.rs"]
mod bot_data;
use bot_data::bot_memory_and_knowledge;

// ---------------------------------------------------------------------------------------------
// Mock OpenAI-compatible provider (port of tests/mock-provider.ts and scriptedFileReply)

#[derive(Clone, Debug)]
enum Reply {
    Text(String),
    Tool(&'static str, Value),
    /// Fail the request with this HTTP status.
    Status(u16, String),
    /// Stream these text deltas in one response.
    Chunks(Vec<String>),
    /// Text that ends with `finish_reason: "length"`.
    Truncated(String),
    /// Hold the request open until released (or 120 s), then answer with text.
    Hold,
}

fn text(value: &str) -> Reply {
    Reply::Text(value.to_owned())
}

fn discover(query: &str) -> Reply {
    Reply::Tool("search_tool", json!({"query": query, "limit": 5}))
}

fn use_tool(name: &str, input: Value) -> Reply {
    Reply::Tool(
        "use_tool",
        json!({"tool_name": format!("workspace__{name}"), "tool_input": input}),
    )
}

#[derive(Default)]
struct ProviderState {
    plan: VecDeque<Reply>,
    /// Reply when the plan is empty.
    fallback: Option<Reply>,
    /// `scriptedFileReply` from tests/runtime.integration.test.ts.
    scripted_file: bool,
    /// Main agent requests (those offering `search_tool`).
    inference: Vec<Value>,
    models: Vec<String>,
}

#[derive(Clone, Default)]
struct Provider {
    state: Arc<Mutex<ProviderState>>,
    release: Arc<tokio::sync::Notify>,
}

impl Provider {
    fn plan(&self, replies: Vec<Reply>) {
        let mut state = self.state.lock().unwrap();
        state.plan = replies.into();
    }
    fn inference(&self) -> Vec<Value> {
        self.state.lock().unwrap().inference.clone()
    }
    fn inference_len(&self) -> usize {
        self.state.lock().unwrap().inference.len()
    }
    fn clear(&self) {
        self.state.lock().unwrap().inference.clear();
    }
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn messages(request: &Value) -> Vec<Value> {
    request["messages"].as_array().cloned().unwrap_or_default()
}

fn tool_names(request: &Value) -> Vec<String> {
    let mut names: Vec<String> = request["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["function"]["name"].as_str().map(str::to_owned))
        .collect();
    names.sort();
    names
}

/// Tool result texts the engine returned to the model in `requests`.
fn tool_outcomes(requests: &[Value]) -> Vec<String> {
    requests
        .iter()
        .flat_map(messages)
        .filter(|m| m["role"] == "tool")
        .map(|m| content_text(&m["content"]))
        .collect()
}

fn user_texts(requests: &[Value]) -> Vec<String> {
    requests
        .iter()
        .flat_map(messages)
        .filter(|m| m["role"] == "user")
        .map(|m| content_text(&m["content"]))
        .collect()
}

/// The latest prompt the engine sent for this turn (it wraps user text in `<user_query>` and adds
/// its own context and reminder messages around it).
fn turn_prompt(request: &Value) -> String {
    let query = messages(request)
        .iter()
        .rev()
        .filter(|m| m["role"] == "user")
        .map(|m| content_text(&m["content"]))
        .find(|text| text.contains("<user_query>"))
        .unwrap_or_default();
    let start = query
        .find("<user_query>\n")
        .map_or(0, |index| index + "<user_query>\n".len());
    let end = query.rfind("\n</user_query>").unwrap_or(query.len());
    query[start..end.max(start)].to_owned()
}

fn scripted_file_reply(request: &Value) -> Reply {
    let messages = messages(request);
    let latest = messages
        .iter()
        .rposition(|m| m["role"] == "user" && m["content"].to_string().contains("fixture"));
    let Some(latest) = latest else {
        return text("로컬 OpenAI 호환 서버 응답");
    };
    let user = messages[latest]["content"].to_string();
    let calls: Vec<String> = messages[latest + 1..]
        .iter()
        .flat_map(|m| m["tool_calls"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["function"]["name"].as_str().map(str::to_owned))
        .collect();
    if !calls.iter().any(|n| n == "search_tool") {
        return discover("workspace write_file");
    }
    if !calls.iter().any(|n| n == "use_tool") {
        let content = if user.contains("denied") {
            "denied edit must never reach disk\n"
        } else {
            "AI approved local file content\n"
        };
        return use_tool(
            "write_file",
            json!({"path": "notes.txt", "content": content}),
        );
    }
    text("로컬 파일 작업 요청 처리가 끝났습니다.")
}

fn sse(model: &str, delta: Value, finish: &str) -> Response {
    sse_deltas(model, vec![delta], finish)
}

fn sse_deltas(model: &str, deltas: Vec<Value>, finish: &str) -> Response {
    let chunk = |delta: Value, reason: Value| json!({"id": "chatcmpl-local", "object": "chat.completion.chunk", "created": 1, "model": model, "choices": [{"index": 0, "delta": delta, "finish_reason": reason}]});
    let mut last = chunk(json!({}), json!(finish));
    last["usage"] = json!({"prompt_tokens": 100, "completion_tokens": 12, "total_tokens": 112});
    let mut body = String::new();
    let values = std::iter::once(chunk(json!({"role": "assistant"}), Value::Null))
        .chain(deltas.into_iter().map(|delta| chunk(delta, Value::Null)))
        .chain(std::iter::once(last));
    for value in values {
        body.push_str(&format!("data: {value}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

async fn completions(State(provider): State<Provider>, Json(request): Json<Value>) -> Response {
    let model = request["model"].as_str().unwrap_or_default().to_owned();
    let main = tool_names(&request).iter().any(|n| n == "search_tool");
    let reply = {
        let mut state = provider.state.lock().unwrap();
        state.models.push(model.clone());
        if !main {
            Reply::Text("Fixture title".into())
        } else {
            state.inference.push(request.clone());
            if state.scripted_file {
                scripted_file_reply(&request)
            } else {
                state
                    .plan
                    .pop_front()
                    .or_else(|| state.fallback.clone())
                    .unwrap_or_else(|| text("Local turn complete."))
            }
        }
    };
    let reply = match reply {
        Reply::Hold => {
            let _ =
                tokio::time::timeout(Duration::from_secs(120), provider.release.notified()).await;
            text("Released.")
        }
        other => other,
    };
    match reply {
        Reply::Text(content) => sse(&model, json!({"content": content}), "stop"),
        Reply::Tool(name, arguments) => {
            let id = format!("call_local_{}", provider.inference_len());
            sse(
                &model,
                json!({"tool_calls": [{"index": 0, "id": id, "type": "function", "function": {"name": name, "arguments": arguments.to_string()}}]}),
                "tool_calls",
            )
        }
        Reply::Status(status, message) => (
            StatusCode::from_u16(status).unwrap(),
            Json(json!({"error": {"message": message, "type": "fixture_error"}})),
        )
            .into_response(),
        Reply::Chunks(parts) => sse_deltas(
            &model,
            parts
                .into_iter()
                .map(|part| json!({"content": part}))
                .collect(),
            "stop",
        ),
        Reply::Truncated(content) => sse(&model, json!({"content": content}), "length"),
        Reply::Hold => unreachable!(),
    }
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    origin
}

async fn start_provider(provider: Provider) -> String {
    let router = Router::new()
        .route("/v1/chat/completions", post(completions))
        .route("/v1/models", axum::routing::get(|| async { Json(json!({"object": "list", "data": [{"id": "local-test-model", "object": "model"}]})) }))
        .with_state(provider);
    format!("{}/v1", serve(router).await)
}

// ---------------------------------------------------------------------------------------------
// Test-only broker route: `POST /internal/mcp/:runId` forwards to `Runtime::mcp`.

async fn broker(
    State(runtime): State<Arc<Runtime>>,
    UrlPath(run_id): UrlPath<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    match runtime.mcp(&run_id, authorization, body).await {
        Ok(Some(result)) => Json(result).into_response(),
        Ok(None) => StatusCode::ACCEPTED.into_response(),
        Err(error) => (
            StatusCode::from_u16(error.status).unwrap(),
            Json(json!({"error": error.message})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------------------------
// Fixture

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    store: Arc<Store>,
    runtime: Arc<Runtime>,
    provider: Provider,
    base_url: String,
}

struct Options {
    startup: Duration,
    idle: Duration,
    cancel: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            startup: Duration::from_secs(30),
            idle: Duration::from_secs(30),
            cancel: Duration::from_millis(800),
        }
    }
}

impl Fixture {
    async fn new(base: &Path, engine_root: &Path, name: &str, options: Options) -> Fixture {
        let root = base.join(name);
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("notes.txt"), "original\n").unwrap();
        let provider = Provider::default();
        let base_url = start_provider(provider.clone()).await;
        let store = Store::open(&root.join("data"), system_clock())
            .await
            .unwrap();
        let runtime = Runtime::new(
            Arc::clone(&store),
            CommandQueue::new(),
            RuntimeOptions {
                engine_root: engine_root.to_path_buf(),
                startup_timeout: options.startup,
                prompt_idle_timeout: options.idle,
                cancel_deadline: options.cancel,
            },
        );
        runtime.initialize().await;
        let router = Router::new()
            .route("/internal/mcp/{run_id}", post(broker))
            .with_state(Arc::clone(&runtime));
        runtime.set_origin(serve(router).await);
        let fixture = Fixture {
            root,
            workspace,
            store,
            runtime,
            provider,
            base_url,
        };
        fixture.configure("", true).await;
        fixture
    }

    async fn configure(&self, api_key: &str, files: bool) {
        let input = SettingsInput {
            base_url: self.base_url.clone(),
            model: "local-test-model".into(),
            workspace: if files {
                self.workspace.to_string_lossy().into_owned()
            } else {
                String::new()
            },
            api_key: (!api_key.is_empty()).then(|| api_key.to_owned()),
            clear_api_key: api_key.is_empty().then_some(true),
        };
        self.store
            .mutate(move |state| state.update_settings(input))
            .await
            .unwrap();
    }

    fn bot_id(&self) -> String {
        self.store.read(|state| state.disk().bots[0].id.clone())
    }

    async fn conversation(&self, bot_id: &str) -> String {
        let bot_id = bot_id.to_owned();
        self.store
            .mutate(move |state| state.add_conversation(&bot_id))
            .await
            .unwrap()
            .id
    }

    fn get(&self, id: &str) -> Conversation {
        self.store
            .read(|state| state.conversation(id).unwrap().clone())
    }

    fn public(&self) -> AppState {
        self.store.snapshot()
    }

    async fn wait(
        &self,
        id: &str,
        what: &str,
        predicate: impl Fn(&Conversation) -> bool,
    ) -> Conversation {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let current = self.get(id);
            if predicate(&current) {
                return current;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {}",
                serde_json::to_string(&current.run).unwrap()
            );
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }

    async fn settled(&self, id: &str) -> Conversation {
        self.wait(id, "a terminal run", |c| c.run.status.is_terminal())
            .await
    }

    async fn completed(&self, id: &str) -> Conversation {
        let conversation = self.settled(id).await;
        assert_eq!(
            conversation.run.status,
            RunStatus::Completed,
            "{:?}",
            conversation.run
        );
        conversation
    }

    async fn pending(&self, id: &str) -> Conversation {
        let conversation = self
            .wait(id, "an approval", |c| {
                c.run.status == RunStatus::AwaitingPermission || c.run.status.is_terminal()
            })
            .await;
        assert_eq!(
            conversation.run.status,
            RunStatus::AwaitingPermission,
            "{:?}",
            conversation.run
        );
        conversation
    }

    async fn decide(&self, id: &str, kind: &str) -> grok_bot_model::PendingPermission {
        let conversation = self.pending(id).await;
        let permission = conversation.run.permission.clone().unwrap();
        let option = permission
            .options
            .iter()
            .find(|o| o.kind == kind)
            .unwrap()
            .id
            .clone();
        self.runtime
            .resolve(id, &permission.id, &option, false)
            .await
            .unwrap();
        permission
    }

    fn notes(&self) -> String {
        std::fs::read_to_string(self.workspace.join("notes.txt")).unwrap()
    }

    async fn close(self) {
        self.runtime.shutdown().await;
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn last_text(conversation: &Conversation) -> String {
    conversation.messages.last().unwrap().text.clone()
}

fn files_containing(directory: &Path, needle: &[u8]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            found.extend(files_containing(&path, needle));
        } else if kind.is_file()
            && std::fs::read(&path)
                .unwrap()
                .windows(needle.len())
                .any(|w| w == needle)
        {
            found.push(path);
        }
    }
    found
}

// ---------------------------------------------------------------------------------------------
// Scenarios

/// backend-grok.test.ts + runtime.integration.test.ts: broker-only tools, exact approved writes,
/// denial, cancellation, continuation, confinement, redaction, and literal prompts.
async fn file_approvals_and_sessions(f: &Fixture) {
    let snapshots = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut updates = f.store.subscribe();
    let collected = Arc::clone(&snapshots);
    tokio::spawn(async move {
        while let Ok(state) = updates.recv().await {
            collected
                .lock()
                .unwrap()
                .push(serde_json::to_string(&*state).unwrap());
        }
    });
    assert_eq!(
        f.public().runtime.version.as_deref(),
        Some("grok-bot-engine 0.1.0 (2bdd1d6a6369)")
    );
    assert!(f.public().runtime.available);
    let id = f.conversation(&f.bot_id()).await;

    f.provider.plan(vec![
        discover("workspace write_file"),
        use_tool(
            "write_file",
            json!({"path": "notes.txt", "content": "approved by browser\n"}),
        ),
        text("The approved write succeeded."),
    ]);
    f.runtime
        .start(&id, "Write notes.txt using the workspace broker.")
        .await
        .unwrap();
    let busy = f
        .runtime
        .start(&id, "second simultaneous run")
        .await
        .unwrap_err();
    assert_eq!(
        (busy.status, busy.message.as_str()),
        (
            409,
            "Stop the active run before changing settings, bots, or files."
        )
    );
    let pending = f.pending(&id).await;
    assert_eq!(f.notes(), "original\n");
    let permission = pending.run.permission.clone().unwrap();
    assert_eq!(permission.paths, ["notes.txt"]);
    assert_eq!(
        permission
            .options
            .iter()
            .map(|o| o.kind.as_str())
            .collect::<Vec<_>>(),
        ["allow_once", "reject_once"]
    );
    assert!(
        permission
            .description
            .starts_with("Replace notes.txt\n\napproved by browser\n")
    );
    let wrong = f
        .runtime
        .resolve(&id, &permission.id, "not-a-real-option", false)
        .await
        .unwrap_err();
    assert_eq!(
        (wrong.status, wrong.message.as_str()),
        (400, "Choose one of the displayed approval options.")
    );
    let allow = permission.options[0].id.clone();
    f.runtime
        .resolve(&id, &permission.id, &allow, false)
        .await
        .unwrap();
    let again = f
        .runtime
        .resolve(&id, &permission.id, &allow, false)
        .await
        .unwrap_err();
    assert_eq!(
        (again.status, again.message.as_str()),
        (409, "This approval is no longer pending.")
    );
    let completed = f.completed(&id).await;
    assert_eq!(last_text(&completed), "The approved write succeeded.");
    assert_eq!(
        completed.title,
        "Write notes.txt using the workspace broker."
    );
    assert_eq!(f.notes(), "approved by browser\n");
    let write_tool = completed.messages[1]
        .tools
        .iter()
        .find(|t| t.title == "Write notes.txt")
        .expect("broker write activity");
    assert_eq!(write_tool.status, grok_bot_model::ToolStatus::Completed);
    assert_eq!(write_tool.output.as_deref(), Some("Saved notes.txt."));
    let inference = f.provider.inference();
    assert!(inference.len() >= 3);
    for request in &inference {
        assert_eq!(tool_names(request), ["search_tool", "use_tool"]);
        assert_eq!(request["model"], "local-test-model");
    }
    assert!(
        tool_outcomes(&inference)
            .iter()
            .any(|t| t.contains("workspace__write_file"))
    );
    let original_session = completed.session_id.clone().expect("session id");
    assert!(completed.session_fingerprint.is_some());

    // Continuation loads the same session and keeps the prior turn in context.
    let before = f.provider.inference_len();
    f.provider.plan(vec![
        Reply::Tool(
            "run_terminal_command",
            json!({"command": "printf bypass > ../outside.txt"}),
        ),
        Reply::Tool("read_file", json!({"target_file": "/etc/hostname"})),
        use_tool(
            "write_file",
            json!({"path": "../outside.txt", "content": "bypass"}),
        ),
        use_tool("read_file", json!({"path": "linked-parent/outside.txt"})),
        use_tool(
            "write_file",
            json!({"path": "linked-file.txt", "content": "bypass"}),
        ),
        use_tool("computer_observe", json!({})),
        text("Forbidden tools and traversal were blocked."),
    ]);
    std::fs::write(f.root.join("outside.txt"), "outside stays unchanged\n").unwrap();
    std::os::unix::fs::symlink(&f.root, f.workspace.join("linked-parent")).unwrap();
    std::os::unix::fs::symlink(
        f.root.join("outside.txt"),
        f.workspace.join("linked-file.txt"),
    )
    .unwrap();
    f.runtime
        .start(&id, "Continue and verify the configured confinement.")
        .await
        .unwrap();
    let continued = f.completed(&id).await;
    assert_eq!(continued.messages.len(), 4);
    assert_eq!(
        continued.session_id.as_deref(),
        Some(original_session.as_str())
    );
    assert_eq!(continued.messages[1].text, "The approved write succeeded.");
    assert_eq!(
        continued.messages[3].text,
        "Forbidden tools and traversal were blocked."
    );
    let after = f.provider.inference()[before..].to_vec();
    assert!(messages(&after[0]).iter().any(|m| m["role"] == "assistant"
        && content_text(&m["content"]).contains("The approved write succeeded.")));
    assert_eq!(
        std::fs::read_to_string(f.root.join("outside.txt")).unwrap(),
        "outside stays unchanged\n"
    );
    let outcomes = tool_outcomes(&after).join("\n");
    assert!(
        outcomes.contains("Tool not found: workspace__computer_observe"),
        "computer tools are not offered: {outcomes}"
    );
    assert!(outcomes.to_lowercase().contains("traversal"), "{outcomes}");
    assert!(outcomes.contains("Symbolic links"), "{outcomes}");
    assert!(!outcomes.contains("outside stays unchanged"));
    for request in &after {
        assert_eq!(tool_names(request), ["search_tool", "use_tool"]);
    }

    // Denial writes nothing.
    f.provider.plan(vec![
        use_tool(
            "write_file",
            json!({"path": "notes.txt", "content": "denied write\n"}),
        ),
        text("The write was denied."),
    ]);
    f.runtime
        .start(&id, "Propose another write so I can deny it.")
        .await
        .unwrap();
    f.decide(&id, "reject_once").await;
    let denied = f.completed(&id).await;
    assert_eq!(f.notes(), "approved by browser\n");
    let tool = denied.messages[5]
        .tools
        .iter()
        .find(|t| t.title == "Write notes.txt")
        .unwrap();
    assert_eq!(tool.status, grok_bot_model::ToolStatus::Failed);
    assert_eq!(
        tool.output.as_deref(),
        Some("The user denied this write or cancelled the run.")
    );

    // Cancelling while an approval is pending settles it and writes nothing.
    f.provider.plan(vec![use_tool(
        "write_file",
        json!({"path": "notes.txt", "content": "cancelled write\n"}),
    )]);
    f.runtime
        .start(&id, "Propose a write so I can cancel the task.")
        .await
        .unwrap();
    let stale = f.pending(&id).await.run.permission.unwrap();
    f.runtime.cancel(&id).await.unwrap();
    let cancelled = f.get(&id);
    assert_eq!(cancelled.run.status, RunStatus::Cancelled);
    assert_eq!(cancelled.run.permission, None);
    assert_eq!(cancelled.run.error, None);
    let late = f
        .runtime
        .resolve(&id, &stale.id, &stale.options[0].id, false)
        .await
        .unwrap_err();
    assert_eq!(late.status, 409);
    assert_eq!(f.notes(), "approved by browser\n");
    // Cancel of an idle conversation is a no-op; an unknown one is 404.
    f.runtime.cancel(&id).await.unwrap();
    assert_eq!(
        f.runtime
            .cancel("00000000-0000-4000-8000-000000000000")
            .await
            .unwrap_err()
            .status,
        404
    );

    // A new key replaces the session (settings clear it) and replays visible history once.
    let api_key = "backend-only-local-fixture-key-78584";
    f.configure(api_key, true).await;
    f.provider.plan(vec![text(&format!(
        "Authenticated local provider reply {api_key}."
    ))]);
    let before = f.provider.inference_len();
    f.runtime
        .start(&id, "Test provider credential redaction.")
        .await
        .unwrap();
    let redacted = f.completed(&id).await;
    assert_eq!(redacted.messages.len(), 10);
    assert_eq!(
        last_text(&redacted),
        "Authenticated local provider reply [redacted]."
    );
    assert_ne!(
        redacted.session_id.as_deref(),
        Some(original_session.as_str())
    );
    let replacement = f.provider.inference()[before..].to_vec();
    let prompt = user_texts(&replacement).join("\n");
    assert!(prompt.contains("The approved write succeeded."), "{prompt}");
    assert!(prompt.contains("Current user message:"));
    assert!(prompt.contains("Test provider credential redaction."));
    assert!(
        !messages(&replacement[0])
            .iter()
            .any(|m| m["role"] == "tool"
                || m["tool_calls"].as_array().is_some_and(|c| !c.is_empty()))
    );
    assert!(
        !serde_json::to_string(&f.public())
            .unwrap()
            .contains(api_key)
    );
    assert!(files_containing(&f.root.join("data").join("grok"), api_key.as_bytes()).is_empty());

    // Attachment-like prompts reach the engine as literal JSON and never expand files.
    let canary = format!("PRIVATE_CONTENT_{}", uuid::Uuid::new_v4());
    let outside = f.root.join("private-canary.txt");
    std::fs::write(&outside, &canary).unwrap();
    let outside = outside.to_string_lossy().into_owned();
    let literal_prompts = [
        format!("@{outside}"),
        format!("Quoted `@{outside}` and (@{outside})"),
        format!("/mcp install unexpected @{outside}"),
        "[Image #1: /tmp/private-image.png]".to_owned(),
        "\u{0085}/mcp install unexpected".to_owned(),
        "Keep DATA:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB as literal text".to_owned(),
    ];
    for prompt in literal_prompts {
        let before = f.provider.inference_len();
        let literal = f.conversation(&f.bot_id()).await;
        f.provider.plan(vec![text("Literal prompt accepted.")]);
        f.runtime.start(&literal, &prompt).await.unwrap();
        let finished = f.completed(&literal).await;
        assert_eq!(finished.messages[0].text, prompt);
        assert_eq!(finished.messages[1].text, "Literal prompt accepted.");
        let requests = f.provider.inference()[before..].to_vec();
        assert!(!requests.is_empty());
        assert!(
            !serde_json::to_string(&requests).unwrap().contains(&canary),
            "prompt expansion read the external file"
        );
        let wire = user_texts(&requests).join("\n");
        assert!(
            !wire.contains("<attached_files>") && !wire.contains('@'),
            "{wire}"
        );
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    for snapshot in snapshots.lock().unwrap().iter() {
        assert!(!snapshot.contains(api_key));
        assert!(!snapshot.contains(&api_key[..16]));
    }
}

/// runtime.integration.test.ts: scripted provider, approval, denial, and the same session.
async fn scripted_file_reply_flow(f: &Fixture) {
    f.configure("private-local-fixture-key", true).await;
    f.provider.state.lock().unwrap().scripted_file = true;
    let id = f.conversation(&f.bot_id()).await;
    f.runtime.start(&id, "fixture approved edit").await.unwrap();
    f.pending(&id).await;
    assert_eq!(f.notes(), "original\n");
    f.decide(&id, "allow_once").await;
    let completed = f.completed(&id).await;
    assert_eq!(f.notes(), "AI approved local file content\n");
    let session = completed.session_id.clone().unwrap();
    assert!(
        !serde_json::to_string(&f.public())
            .unwrap()
            .contains("private-local-fixture-key")
    );
    f.runtime.start(&id, "fixture denied edit").await.unwrap();
    f.decide(&id, "reject_once").await;
    let denied = f.settled(&id).await;
    assert_eq!(denied.session_id.as_deref(), Some(session.as_str()));
    assert_eq!(f.notes(), "AI approved local file content\n");
    let main = f.provider.inference();
    assert!(main.len() >= 4);
    for request in &main {
        let names = tool_names(request);
        assert!(
            !names
                .iter()
                .any(|n| n == "run_terminal_command" || n == "read_file" || n == "search_replace"),
            "{names:?}"
        );
    }
    f.provider.state.lock().unwrap().scripted_file = false;
}

/// backend-runtime.test.ts: fingerprints from another broker or engine source rebuild context once
/// with redacted visible history, and then load the replacement session.
async fn fingerprint_migration(f: &Fixture) {
    let key = "continuation-fixture-secret";
    f.configure(key, true).await;
    let id = f.conversation(&f.bot_id()).await;
    f.provider.plan(vec![text("Hello from the provider.")]);
    f.runtime
        .start(&id, &format!("Remember the visible detail {key}"))
        .await
        .unwrap();
    let first = f.completed(&id).await;
    assert_eq!(
        first.messages[0].text,
        format!("Remember the visible detail {key}")
    );
    for legacy in ["broker-v6", "broker-v2"] {
        let fingerprint = {
            use sha2::Digest;
            let settings = f.store.read(|state| state.settings().clone());
            let input = json!([
                settings,
                "",
                legacy,
                "2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8"
            ])
            .to_string();
            hex::encode(sha2::Sha256::digest(input.as_bytes()))
        };
        let previous = f.get(&id).session_id.clone();
        let conversation = id.clone();
        let stored = fingerprint.clone();
        f.store
            .mutate(move |state| {
                state.conversation_mut(&conversation)?.session_fingerprint = Some(stored);
                Ok(())
            })
            .await
            .unwrap();
        let before = f.provider.inference_len();
        let prompt = format!("Use the remembered detail after {legacy}");
        f.runtime.start(&id, &prompt).await.unwrap();
        let migrated = f.completed(&id).await;
        assert_ne!(migrated.session_id, previous);
        assert_ne!(
            migrated.session_fingerprint.as_deref(),
            Some(fingerprint.as_str())
        );
        let requests = f.provider.inference()[before..].to_vec();
        let wire = turn_prompt(&requests[0]);
        let start = wire
            .find("[{")
            .unwrap_or_else(|| panic!("continuation history: {wire}"));
        let end = wire.find("\n\nCurrent user message:\n").unwrap();
        let history: Vec<Value> = serde_json::from_str(&wire[start..end]).unwrap();
        assert_eq!(
            history[0],
            json!({"role": "user", "text": "Remember the visible detail [redacted]"})
        );
        assert_eq!(
            history[1],
            json!({"role": "assistant", "text": "Hello from the provider."})
        );
        assert!(!wire.contains(key));
        assert_eq!(wire.matches(prompt.as_str()).count(), 1);
        assert!(
            wire.ends_with(&format!("Current user message:\n{prompt}")),
            "{wire}"
        );
        // The replacement session then continues without replay.
        let before = f.provider.inference_len();
        f.runtime
            .start(&id, "Continue without replay")
            .await
            .unwrap();
        let continued = f.completed(&id).await;
        assert_eq!(continued.session_id, migrated.session_id);
        let next = f.provider.inference()[before..].to_vec();
        assert_eq!(turn_prompt(&next[0]), "Continue without replay");
    }
}

/// Provider failures surface as run errors with the key redacted.
async fn provider_failure(f: &Fixture) {
    let key = "provider-failure-fixture-key";
    f.configure(key, true).await;
    let id = f.conversation(&f.bot_id()).await;
    f.provider.plan(vec![Reply::Status(
        500,
        format!("Local fixture provider error echoing {key}"),
    )]);
    f.runtime
        .start(&id, "Trigger a provider failure.")
        .await
        .unwrap();
    let failed = f.settled(&id).await;
    assert_eq!(failed.run.status, RunStatus::Error);
    assert_eq!(
        failed.run.error.as_deref(),
        Some(
            "Internal error: API error (status 500 Internal Server Error): fixture_error: Local fixture provider error echoing [redacted]"
        )
    );
    assert_eq!(last_text(&failed), "");
    assert!(!serde_json::to_string(&f.public()).unwrap().contains(key));
    assert!(
        !serde_json::to_string(&f.store.read(|state| state.disk().clone()))
            .unwrap()
            .contains(&format!("echoing {key}"))
    );
    // The conversation keeps working after the failure.
    f.provider.plan(vec![text("Recovered.")]);
    f.runtime.start(&id, "Try again.").await.unwrap();
    assert_eq!(last_text(&f.completed(&id).await), "Recovered.");
    // This engine reports a provider length stop as a request error (not a `max_tokens` stop
    // reason); the partial text stays visible.
    f.provider
        .plan(vec![Reply::Truncated("Partial answer".into())]);
    f.runtime.start(&id, "Answer at length.").await.unwrap();
    let truncated = f.settled(&id).await;
    assert_eq!(truncated.run.status, RunStatus::Error);
    assert_eq!(
        truncated.run.error.as_deref(),
        Some("Internal error: response truncated by max_tokens")
    );
    assert_eq!(last_text(&truncated), "Partial answer");
}

/// backend-runtime.test.ts: every emitted snapshot hides streamed credential prefixes and
/// credentials in tool activity.
async fn streamed_redaction(f: &Fixture) {
    let key = "private-split-fixture-credential-793515";
    f.configure(key, true).await;
    let snapshots = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut updates = f.store.subscribe();
    let collected = Arc::clone(&snapshots);
    tokio::spawn(async move {
        while let Ok(state) = updates.recv().await {
            collected
                .lock()
                .unwrap()
                .push(serde_json::to_string(&*state).unwrap());
        }
    });
    let id = f.conversation(&f.bot_id()).await;
    let (head, tail) = key.split_at(key.len() - 1);
    f.provider.plan(vec![
        discover("workspace read_file"),
        use_tool("read_file", json!({"path": key})),
        Reply::Chunks(vec![
            "Provider returned ".into(),
            head.into(),
            format!("{tail} safely."),
        ]),
    ]);
    f.runtime.start(&id, "redact-split").await.unwrap();
    let completed = f.completed(&id).await;
    assert_eq!(
        completed.messages[1].text,
        "Provider returned [redacted] safely."
    );
    let read = completed.messages[1]
        .tools
        .iter()
        .find(|t| {
            t.input
                .as_deref()
                .unwrap_or_default()
                .contains("[redacted]")
        })
        .expect("tool input redacted");
    assert!(uuid::Uuid::parse_str(&read.id).is_ok());
    f.provider.plan(vec![Reply::Chunks(vec![
        "Provider returned ".into(),
        head.into(),
    ])]);
    f.runtime.start(&id, "redact-prefix").await.unwrap();
    let prefix = f.completed(&id).await;
    assert_eq!(prefix.messages[3].text, "Provider returned [redacted]");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let snapshots = snapshots.lock().unwrap();
    assert!(snapshots.iter().any(|s| s.contains("Provider returned ")));
    for snapshot in snapshots.iter().chain(std::iter::once(
        &serde_json::to_string(&f.public()).unwrap(),
    )) {
        assert!(!snapshot.contains(key));
        assert!(!snapshot.contains(head));
        assert!(!snapshot.contains(&key[..16]));
    }
}

/// backend-runtime.test.ts: inactivity deadline, its pause during approvals, and cancellation.
async fn deadlines_and_cancellation(f: &Fixture) {
    let id = f.conversation(&f.bot_id()).await;
    // A silent inference hits the backend inactivity deadline.
    f.provider.plan(vec![Reply::Hold]);
    let started = Instant::now();
    f.runtime.start(&id, "silent-hang").await.unwrap();
    let silent = f.settled(&id).await;
    assert_eq!(silent.run.status, RunStatus::Error);
    assert_eq!(
        silent.run.error.as_deref(),
        Some(
            "Grok Build exceeded the inference inactivity deadline without session updates. Check the provider and try again."
        )
    );
    assert_eq!(silent.run.permission, None);
    eprintln!(
        "[runtime] inactivity settled after {:?}, session kept: {}",
        started.elapsed(),
        silent.session_id.is_some()
    );
    f.provider.release.notify_waiters();

    // Inactivity pauses while a broker approval is pending, then resumes after the decision.
    f.provider.plan(vec![
        use_tool(
            "write_file",
            json!({"path": "notes.txt", "content": "AI approved contents\n"}),
        ),
        text("Written."),
    ]);
    f.runtime.start(&id, "write").await.unwrap();
    let pending = f.pending(&id).await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    let still = f.get(&id);
    assert_eq!(still.run.status, RunStatus::AwaitingPermission);
    assert_eq!(
        still.run.permission.as_ref().map(|p| &p.id),
        pending.run.permission.as_ref().map(|p| &p.id)
    );
    f.decide(&id, "allow_once").await;
    let completed = f.completed(&id).await;
    assert_eq!(completed.run.error, None);
    assert_eq!(f.notes(), "AI approved contents\n");

    // After an approval the deadline applies again.
    f.provider.plan(vec![
        use_tool(
            "write_file",
            json!({"path": "notes.txt", "content": "second\n"}),
        ),
        Reply::Hold,
    ]);
    f.runtime.start(&id, "write-hang").await.unwrap();
    f.pending(&id).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    f.decide(&id, "allow_once").await;
    let failed = f.settled(&id).await;
    assert_eq!(failed.run.status, RunStatus::Error);
    assert!(
        failed
            .run
            .error
            .as_deref()
            .unwrap()
            .contains("inactivity deadline")
    );
    assert_eq!(failed.run.permission, None);
    assert_eq!(f.notes(), "second\n");
    f.provider.release.notify_waiters();

    // Cancelling a running inference settles promptly and the conversation continues.
    f.provider.plan(vec![Reply::Hold]);
    f.runtime.start(&id, "hang").await.unwrap();
    f.wait(&id, "running", |c| c.run.status == RunStatus::Running)
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    f.runtime.cancel(&id).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "cancel took {:?}",
        started.elapsed()
    );
    let cancelled = f.get(&id);
    assert_eq!(cancelled.run.status, RunStatus::Cancelled);
    assert_eq!(cancelled.run.error, None);
    f.provider.release.notify_waiters();
    f.provider.plan(vec![text("Back after cancel.")]);
    f.runtime.start(&id, "continue").await.unwrap();
    assert_eq!(last_text(&f.completed(&id).await), "Back after cancel.");
}

/// backend-runtime.test.ts: oversized proposals fail before approval; model-only chat has no files.
async fn write_limits_and_model_only(f: &Fixture) {
    let id = f.conversation(&f.bot_id()).await;
    let mut updates = f.store.subscribe();
    let saw_permission = Arc::new(Mutex::new(false));
    let flag = Arc::clone(&saw_permission);
    tokio::spawn(async move {
        while let Ok(state) = updates.recv().await {
            if state
                .conversations
                .iter()
                .any(|c| c.run.permission.is_some())
            {
                *flag.lock().unwrap() = true;
            }
        }
    });
    for content in [
        format!("{}HIDDEN_SUFFIX", "x".repeat(200_100)),
        format!("{}HIDDEN_SUFFIX", "😀".repeat(50_000)),
    ] {
        f.provider.plan(vec![
            use_tool(
                "write_file",
                json!({"path": "notes.txt", "content": content}),
            ),
            text("Oversized."),
        ]);
        f.runtime.start(&id, "large-write").await.unwrap();
        f.completed(&id).await;
        assert_eq!(f.notes(), "original\n");
    }
    let outcomes = tool_outcomes(&f.provider.inference()).join("\n");
    assert!(outcomes.contains("Write proposals must be at most 160 KiB of UTF-8 text so the complete edit can be reviewed."), "{outcomes}");
    assert!(!*saw_permission.lock().unwrap());

    f.configure("", false).await;
    f.provider.clear();
    f.provider.plan(vec![
        use_tool(
            "write_file",
            json!({"path": "notes.txt", "content": "nope"}),
        ),
        text("No workspace."),
    ]);
    f.runtime.start(&id, "write").await.unwrap();
    let completed = f.completed(&id).await;
    assert_eq!(completed.run.permission, None);
    assert!(tool_outcomes(&f.provider.inference()).iter().any(|t| {
        t.contains("Choose a workspace in Local files settings before using file tools.")
    }));
    assert_eq!(f.notes(), "original\n");
}

/// skills.test.ts runtime parts.
async fn skills(f: &Fixture) {
    f.configure("PRIVATE_KEY_CANARY", false).await;
    let bot = f.bot_id();
    let input = SkillInput {
        name: "plan-review".into(),
        description: "Use when reviewing the local test plan PRIVATE_KEY_CANARY.".into(),
        instructions: "SKILL_BODY_CANARY\nReview every item.".into(),
    };
    let add = |bot: String, input: SkillInput| async move {
        f.store
            .mutate(move |state| state.add_skill(&bot, input, false))
            .await
            .unwrap()
    };
    let skill = add(bot.clone(), input.clone()).await;
    let disabled = add(
        bot.clone(),
        SkillInput {
            name: "disabled".into(),
            instructions: "DISABLED_BODY_CANARY".into(),
            ..input.clone()
        },
    )
    .await;
    {
        let (bot, id) = (bot.clone(), disabled.id.clone());
        f.store
            .mutate(move |state| {
                state.update_skill(
                    &bot,
                    &id,
                    SkillPatch {
                        name: None,
                        description: None,
                        instructions: None,
                        revision: 1,
                        enabled: Some(false),
                    },
                )
            })
            .await
            .unwrap();
    }
    let other = f
        .store
        .mutate(|state| {
            state.add_bot(BotInput {
                name: "Other".into(),
                color: Some("#ff2543".into()),
                instructions: Some(String::new()),
                shape: None,
                pinned: None,
            })
        })
        .await
        .unwrap();
    let foreign = add(
        other.id.clone(),
        SkillInput {
            name: "foreign".into(),
            instructions: "OTHER_BOT_BODY_CANARY".into(),
            ..input.clone()
        },
    )
    .await;
    let id = f.conversation(&bot).await;
    f.provider.clear();
    f.provider.plan(vec![
        discover("workspace list_skills read_skill create_skill"),
        use_tool("list_skills", json!({})),
        use_tool("read_skill", json!({"id": foreign.id})),
        use_tool("read_skill", json!({"id": disabled.id})),
        use_tool("read_skill", json!({"id": skill.id})),
        text("Reviewed with the private skill."),
    ]);
    f.runtime.start(&id, "Use plan-review.").await.unwrap();
    f.completed(&id).await;
    let captured = serde_json::to_string(&f.provider.inference()).unwrap();
    assert!(captured.contains("SKILL_BODY_CANARY"));
    assert!(
        !captured.contains("DISABLED_BODY_CANARY")
            && !captured.contains("OTHER_BOT_BODY_CANARY")
            && !captured.contains("PRIVATE_KEY_CANARY")
    );
    assert!(captured.contains("Enabled private skill does not exist for this bot."));
    let rules: String = f
        .provider
        .inference()
        .iter()
        .flat_map(messages)
        .filter(|m| m["role"] == "system")
        .map(|m| content_text(&m["content"]))
        .collect();
    let index = rules
        .split("Enabled private skill index (data): ")
        .nth(1)
        .expect("skill index in rules")
        .lines()
        .next()
        .unwrap()
        .to_owned();
    let index: Vec<Value> = serde_json::from_str(&index).unwrap();
    assert_eq!(index.len(), 1);
    assert_eq!(
        index[0]["description"],
        "Use when reviewing the local test plan [redacted]."
    );

    // Model-created skills save only after one-use approval; denial and cancel write nothing.
    let created = SkillInput {
        name: "created-skill".into(),
        description: "Use when testing creation.".into(),
        instructions: "CREATED_BODY".into(),
    };
    let count = || {
        f.store
            .read(|state| state.bot(&bot).unwrap().skills.as_ref().map_or(0, Vec::len))
    };
    let baseline = count();
    for decision in ["reject_once", "cancel", "allow_once"] {
        let mut plan = vec![
            discover("workspace create_skill"),
            use_tool("create_skill", json!(created)),
        ];
        if decision == "allow_once" {
            // A retry of the same proposal is idempotent; a second distinct skill is refused.
            plan.push(use_tool("create_skill", json!(created)));
            plan.push(use_tool(
                "create_skill",
                json!({"name": "another", "description": "d", "instructions": "i"}),
            ));
        }
        plan.push(text("Skill request handled."));
        f.provider.plan(plan);
        f.runtime
            .start(&id, &format!("Create a reusable private skill {decision}."))
            .await
            .unwrap();
        let pending = f.pending(&id).await;
        let permission = pending.run.permission.unwrap();
        assert!(permission.description.contains("CREATED_BODY"));
        assert_eq!(count(), baseline);
        if decision == "cancel" {
            f.runtime.cancel(&id).await.unwrap();
        } else {
            let option = permission
                .options
                .iter()
                .find(|o| o.kind == decision)
                .unwrap()
                .id
                .clone();
            f.runtime
                .resolve(&id, &permission.id, &option, false)
                .await
                .unwrap();
        }
        f.settled(&id).await;
        assert_eq!(count(), baseline + usize::from(decision == "allow_once"));
        assert_eq!(
            f.runtime
                .resolve(&id, &permission.id, &permission.options[0].id, false)
                .await
                .unwrap_err()
                .status,
            409
        );
    }
    let outcomes = tool_outcomes(&f.provider.inference()).join("\n");
    assert!(
        outcomes.contains("Only one private skill can be created per model turn."),
        "{outcomes}"
    );
    assert!(outcomes.contains(r#""existing":true"#), "{outcomes}");
    assert!(outcomes.contains("Skill save was denied or cancelled."));
}

/// bot-messages.test.ts runtime parts: approval, idempotent receipt, one distinct message,
/// authenticated broker, recipient chat-only runs, and denial.
async fn bot_creation(f: &Fixture) {
    f.configure("", true).await;
    let owner = f.bot_id();
    let id = f.conversation(&owner).await;
    let input = json!({"name": "팀장", "instructions": "Lead the team.", "color": "#22c55e", "shape": "star"});
    f.provider.clear();
    f.provider.plan(vec![
        discover("workspace create_bot"),
        use_tool("create_bot", input.clone()),
        use_tool("create_bot", input.clone()),
        use_tool("create_bot", json!({"name": "Second"})),
        text("The bot was created."),
    ]);
    f.runtime.start(&id, "팀장 봇 만들어줘").await.unwrap();
    let pending = f.pending(&id).await;
    let permission = pending.run.permission.clone().unwrap();
    assert!(permission.title.contains("팀장") && permission.description.contains("Lead the team."), "{permission:?}");
    assert_eq!(f.store.read(|s| s.disk().bots.len()), 1, "no bot before approval");
    f.decide(&id, "allow_once").await;
    let completed = f.completed(&id).await;
    let bots = f.store.read(|s| s.disk().bots.clone());
    assert_eq!(bots.len(), 2, "exactly one bot created despite the retry: {bots:?}");
    let created = bots.iter().find(|b| b.name == "팀장").expect("created bot");
    assert_eq!(created.instructions, "Lead the team.");
    assert_eq!(created.color, "#22c55e");
    assert_eq!(created.shape, Some(grok_bot_model::AvatarShape::Star));
    let outcomes = tool_outcomes(&f.provider.inference());
    assert!(outcomes.iter().any(|o| o.contains("\"existing\":true")), "{outcomes:?}");
    assert!(outcomes.iter().any(|o| o.contains("Only one bot can be created per model turn")), "{outcomes:?}");
    assert_eq!(last_text(&completed), "The bot was created.");

    f.provider.clear();
    f.provider.plan(vec![
        discover("workspace create_bot"),
        use_tool("create_bot", json!({"name": "Denied"})),
        text("Not created."),
    ]);
    f.runtime.start(&id, "Create another bot").await.unwrap();
    f.decide(&id, "reject_once").await;
    f.completed(&id).await;
    assert!(!f.store.read(|s| s.disk().bots.iter().any(|b| b.name == "Denied")), "denied bot must not exist");
}

async fn bot_messages(f: &Fixture) {
    f.configure("", true).await;
    let sender = f.bot_id();
    let recipient = f
        .store
        .mutate(|state| {
            state.add_bot(BotInput {
                name: "Recipient".into(),
                color: Some("#3b82f6".into()),
                instructions: Some("RECIPIENT_IDENTITY_CANARY".into()),
                shape: None,
                pinned: None,
            })
        })
        .await
        .unwrap();
    let source = f.conversation(&sender).await;
    let input = json!({"toBotId": recipient.id, "text": "Review this plain chat message.", "requestId": "delivery-1"});
    f.provider.clear();
    f.provider.plan(vec![
        discover("workspace list_bots send_message"),
        use_tool("list_bots", json!({})),
        use_tool("send_message", input.clone()),
        use_tool("send_message", input.clone()),
        use_tool(
            "send_message",
            json!({"toBotId": recipient.id, "text": "A second distinct message is forbidden."}),
        ),
        text("The message was queued."),
    ]);
    f.runtime
        .start(&source, "Send a local bot message.")
        .await
        .unwrap();
    let pending = f.pending(&source).await;
    assert!(f.store.read(|state| state.disk().bot_deliveries.is_empty()));
    let permission = pending.run.permission.clone().unwrap();
    assert!(
        permission.description.contains("Recipient")
            && permission
                .description
                .contains("Review this plain chat message.")
    );
    assert!(permission.paths.is_empty());
    assert_eq!(
        permission
            .options
            .iter()
            .map(|o| o.label.as_str())
            .collect::<Vec<_>>(),
        ["Send once", "Deny"]
    );
    let ping = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let forbidden = f
        .runtime
        .mcp(&pending.run.id, None, ping.clone())
        .await
        .unwrap_err();
    assert_eq!(
        (forbidden.status, forbidden.message.as_str()),
        (403, "Invalid workspace broker session.")
    );
    assert_eq!(
        f.runtime
            .mcp(&pending.run.id, Some("Bearer wrong"), ping)
            .await
            .unwrap_err()
            .status,
        403
    );
    f.decide(&source, "allow_once").await;
    let completed = f.completed(&source).await;
    assert_eq!(last_text(&completed), "The message was queued.");
    let delivery = f.store.read(|state| state.disk().bot_deliveries.clone());
    assert_eq!(delivery.len(), 1);
    let delivery = delivery[0].clone();
    assert_eq!(delivery.status, DeliveryStatus::Queued);
    assert_eq!(delivery.source_message_id, completed.messages[1].id);
    let outcomes = tool_outcomes(&f.provider.inference());
    assert!(
        outcomes
            .iter()
            .any(|t| t.contains(&recipient.id) && t.contains("Recipient"))
    );
    assert!(outcomes.iter().filter(|t| t.contains(&delivery.id)).count() >= 2);
    assert!(
        outcomes
            .iter()
            .any(|t| t.contains("one distinct bot message"))
    );
    assert_eq!(
        f.runtime
            .mcp(
                &pending.run.id,
                None,
                json!({"jsonrpc": "2.0", "method": "ping"})
            )
            .await
            .unwrap_err()
            .status,
        403
    );

    // The recipient turn (started by the dispatcher in the app) is chat-only.
    let before = f.provider.inference_len();
    f.provider.plan(vec![
        discover("workspace read_file write_file"),
        use_tool("read_file", json!({"path": "notes.txt"})),
        use_tool(
            "write_file",
            json!({"path": "notes.txt", "content": "must not be saved"}),
        ),
        text("Recipient final answer."),
    ]);
    f.runtime
        .start(&delivery.recipient_conversation_id, &delivery.text)
        .await
        .unwrap();
    let answered = f.completed(&delivery.recipient_conversation_id).await;
    assert_eq!(
        answered.messages[0].source_bot_id.as_deref(),
        Some(sender.as_str())
    );
    assert_eq!(answered.messages[0].role, Role::User);
    assert_eq!(answered.run.permission, None);
    assert_eq!(f.notes(), "original\n");
    let recipient_requests = f.provider.inference()[before..].to_vec();
    assert!(
        tool_outcomes(&recipient_requests)
            .iter()
            .any(|t| t.contains("chat-only"))
    );
    let wire = serde_json::to_string(&recipient_requests).unwrap();
    assert!(
        wire.contains("RECIPIENT_IDENTITY_CANARY")
            && wire.contains("This is a chat-only message from local bot")
    );
    let delivered = f.store.read(|state| state.disk().bot_deliveries[0].clone());
    assert_eq!(
        delivered.execution_run_id.as_deref(),
        Some(answered.run.id.as_str())
    );
    assert_eq!(delivered.status, DeliveryStatus::Completed);

    // Denial records nothing.
    let source = f.conversation(&sender).await;
    f.provider.plan(vec![
        use_tool(
            "send_message",
            json!({"toBotId": recipient.id, "text": "Needs approval."}),
        ),
        text("Denied."),
    ]);
    f.runtime
        .start(&source, "Send another message.")
        .await
        .unwrap();
    f.decide(&source, "reject_once").await;
    f.completed(&source).await;
    assert_eq!(f.store.read(|state| state.disk().bot_deliveries.len()), 1);
}

/// A server restart (new runtime and engine over the same data) loads the saved session from disk.
async fn restart_continuation(f: &Fixture) {
    let id = f.conversation(&f.bot_id()).await;
    f.provider.plan(vec![text("Remembered FIRST-TURN-SECRET.")]);
    f.runtime
        .start(&id, "hello FIRST-TURN-SECRET")
        .await
        .unwrap();
    let first = f.completed(&id).await;
    f.runtime.shutdown().await;
    let restarted = Runtime::new(
        Arc::clone(&f.store),
        CommandQueue::new(),
        RuntimeOptions::new(f.root.parent().unwrap().join("engine")),
    );
    restarted.initialize().await;
    let router = Router::new()
        .route("/internal/mcp/{run_id}", post(broker))
        .with_state(Arc::clone(&restarted));
    restarted.set_origin(serve(router).await);
    let before = f.provider.inference_len();
    f.provider.plan(vec![text("Recalled.")]);
    restarted.start(&id, "RECALL it").await.unwrap();
    let recalled = f.completed(&id).await;
    assert_eq!(recalled.session_id, first.session_id);
    assert_eq!(last_text(&recalled), "Recalled.");
    let request = &f.provider.inference()[before];
    assert!(messages(request).iter().any(|m| m["role"] == "assistant"
        && content_text(&m["content"]).contains("Remembered FIRST-TURN-SECRET.")));
    assert_eq!(turn_prompt(request), "RECALL it");
    // Different provider settings restart the shared engine for the next run.
    f.configure("restart-key", true).await;
    f.provider.plan(vec![text("New engine.")]);
    restarted.start(&id, "after settings change").await.unwrap();
    let changed = f.completed(&id).await;
    assert_ne!(changed.session_id, first.session_id);
    assert_eq!(last_text(&changed), "New engine.");
    restarted.shutdown().await;
}

/// Startup failures: no provider, no origin, and the startup deadline.
async fn startup_errors(base: &Path, engine_root: &Path) {
    let f = Fixture::new(
        base,
        engine_root,
        "startup",
        Options {
            startup: Duration::from_millis(1),
            ..Options::default()
        },
    )
    .await;
    let id = f.conversation(&f.bot_id()).await;
    f.runtime.start(&id, "hello").await.unwrap();
    let failed = f.settled(&id).await;
    assert_eq!(failed.run.status, RunStatus::Error);
    assert!(
        failed
            .run
            .error
            .as_deref()
            .unwrap()
            .contains("startup deadline"),
        "{:?}",
        failed.run
    );
    assert_eq!(failed.run.permission, None);
    assert_eq!(failed.messages[1].text, "");
    f.close().await;

    let root = base.join("unconfigured");
    let store = Store::open(&root.join("data"), system_clock())
        .await
        .unwrap();
    let runtime = Runtime::new(
        Arc::clone(&store),
        CommandQueue::new(),
        RuntimeOptions::new(engine_root),
    );
    let bot = store.read(|state| state.disk().bots[0].id.clone());
    let id = store
        .mutate(move |state| state.add_conversation(&bot))
        .await
        .unwrap()
        .id;
    // Before `initialize` the store still reports the engine as unavailable.
    let unavailable = store.read(|state| state.runtime().clone());
    assert!(!unavailable.available);
    let error = runtime.start(&id, "hello").await.unwrap_err();
    assert_eq!(error.status, 503);
    assert_eq!(
        Some(error.message),
        unavailable
            .error
            .or(Some("Grok Build is unavailable.".into()))
    );
    runtime.initialize().await;
    let error = runtime.start(&id, "hello").await.unwrap_err();
    assert_eq!(
        (error.status, error.message.as_str()),
        (503, "Runtime is not connected to the local server.")
    );
    runtime.set_origin("http://127.0.0.1:9".into());
    let error = runtime.start(&id, "hello").await.unwrap_err();
    assert_eq!(
        (error.status, error.message.as_str()),
        (400, "Configure a provider and model in settings first.")
    );
    assert!(store.read(|state| state.conversation(&id).unwrap().messages.is_empty()));
    runtime.shutdown().await;
    let error = runtime.start(&id, "hello").await.unwrap_err();
    assert_eq!(error.status, 503);
}

/// Permission rules: an allow rule skips the card and labels the tool activity, ask rules and the
/// default still ask, and the safety floor (hash checks, recipient and webhook conversations)
/// holds even with allow rules for every category.
async fn permission_rules(f: &Fixture) {
    use grok_bot_model::{PermissionAction, PermissionCategory, PermissionRuleInput, PermissionRulePatch, ToolStatus};
    f.configure("", true).await;
    let bot = f.bot_id();
    let saw_permission = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let (mut updates, seen) = (f.store.subscribe(), Arc::clone(&saw_permission));
        tokio::spawn(async move {
            while let Ok(state) = updates.recv().await {
                for c in state.conversations.iter().filter(|c| c.run.permission.is_some()) {
                    let mut seen = seen.lock().unwrap();
                    if !seen.contains(&c.id) {
                        seen.push(c.id.clone());
                    }
                }
            }
        });
    }
    let add = |category, action, bot_id: Option<String>| {
        let input = PermissionRuleInput { category, action, bot_id };
        async move { f.store.mutate(move |s| s.add_permission_rule(input, true)).await.unwrap() }
    };
    let allow_writes = add(PermissionCategory::FileWrite, PermissionAction::Allow, None).await;

    // Auto-allow: no card, the write lands, and the activity names the rule.
    let id = f.conversation(&bot).await;
    f.provider.clear();
    f.provider.plan(vec![
        discover("workspace write_file"),
        use_tool("write_file", json!({"path": "notes.txt", "content": "written by my rule\n"})),
        text("Saved without asking."),
    ]);
    f.runtime.start(&id, "Write notes with my rule").await.unwrap();
    let completed = f.completed(&id).await;
    assert_eq!(f.notes(), "written by my rule\n");
    assert_eq!(last_text(&completed), "Saved without asking.");
    let tool = completed.messages[1].tools.iter().find(|t| t.title == "Write notes.txt").expect("write activity");
    assert_eq!(tool.status, ToolStatus::Completed);
    assert_eq!(tool.approved_by_rule.as_deref(), Some(allow_writes.id.as_str()));
    assert_eq!(tool.input.as_deref(), Some("written by my rule\n"));
    assert!(!saw_permission.lock().unwrap().contains(&id), "an allowed write never shows a card");
    assert_eq!(f.public().conversations.iter().find(|c| c.id == id).unwrap().messages[1].tools.iter().filter(|t| t.approved_by_rule.is_some()).count(), 1);

    // The hash conflict check still applies to auto-allowed writes.
    f.provider.plan(vec![
        use_tool("write_file", json!({"path": "notes.txt", "content": "stale overwrite\n", "hash": "0".repeat(64)})),
        text("Conflict reported."),
    ]);
    f.runtime.start(&id, "Stale write").await.unwrap();
    f.completed(&id).await;
    assert_eq!(f.notes(), "written by my rule\n");
    assert!(tool_outcomes(&f.provider.inference()).iter().any(|t| t.contains("File changed on disk")));

    // A bot-specific ask rule beats the all-bots allow rule: the card asks again.
    let ask_here = add(PermissionCategory::FileWrite, PermissionAction::Ask, Some(bot.clone())).await;
    f.provider.plan(vec![
        use_tool("write_file", json!({"path": "notes.txt", "content": "needs a card\n"})),
        text("Asked."),
    ]);
    f.runtime.start(&id, "Write again").await.unwrap();
    let permission = f.decide(&id, "reject_once").await;
    assert!(permission.description.contains("needs a card"));
    let denied = f.completed(&id).await;
    assert_eq!(f.notes(), "written by my rule\n");
    let tool = denied.messages.last().unwrap().tools.iter().find(|t| t.title == "Write notes.txt").unwrap();
    assert_eq!((tool.status, tool.approved_by_rule.as_deref()), (ToolStatus::Failed, None));
    f.store.mutate(|s| s.update_permission_rule(&ask_here.id, PermissionRulePatch { action: PermissionAction::Allow }, true)).await.unwrap();

    // Other categories: bot creation allowed by rule, bot messages still ask by default.
    let allow_bots = add(PermissionCategory::BotCreate, PermissionAction::Allow, None).await;
    f.provider.plan(vec![
        use_tool("create_bot", json!({"name": "Rule bot", "instructions": "Created by rule."})),
        text("Created."),
    ]);
    f.runtime.start(&id, "Create a bot").await.unwrap();
    let created = f.completed(&id).await;
    let rule_bot = f.store.read(|s| s.disk().bots.iter().find(|b| b.name == "Rule bot").cloned()).expect("bot created by rule");
    let tool = created.messages.last().unwrap().tools.iter().find(|t| t.title == "Create bot: Rule bot").unwrap();
    assert_eq!(tool.approved_by_rule.as_deref(), Some(allow_bots.id.as_str()));
    let source = f.conversation(&bot).await;
    f.provider.plan(vec![
        use_tool("send_message", json!({"toBotId": rule_bot.id, "text": "Asked by default."})),
        text("Queued after approval."),
    ]);
    f.runtime.start(&source, "Message the rule bot").await.unwrap();
    f.decide(&source, "allow_once").await;
    f.completed(&source).await;
    let delivery = f.store.read(|s| s.disk().bot_deliveries.last().cloned()).expect("delivery");
    assert!(saw_permission.lock().unwrap().contains(&source));

    // Safety floor: a recipient conversation always asks, even with allow rules for messages.
    add(PermissionCategory::BotMessage, PermissionAction::Allow, None).await;
    f.provider.plan(vec![
        use_tool("send_message", json!({"toBotId": bot, "text": "Recipient reply must ask."})),
        text("Recipient done."),
    ]);
    f.runtime.start(&delivery.recipient_conversation_id, &delivery.text).await.unwrap();
    let card = f.decide(&delivery.recipient_conversation_id, "reject_once").await;
    assert!(card.description.contains("Recipient reply must ask."));
    let answered = f.completed(&delivery.recipient_conversation_id).await;
    assert!(answered.messages.iter().flat_map(|m| &m.tools).all(|t| t.approved_by_rule.is_none()));

    // Safety floor: a webhook-triggered routine run always asks, even with an allow rule.
    let routine = f
        .store
        .mutate(|s| {
            s.add_routine(grok_bot_model::RoutineInput {
                bot_id: bot.clone(),
                name: "Hook".into(),
                prompt: "Write notes.txt".into(),
                schedule: grok_bot_model::RoutineSchedule::Interval { minutes: 60 },
                enabled: false,
            })
        })
        .await
        .unwrap();
    let key = "gbw_runtime-scenario-key";
    let hash = grok_bot_store::webhooks::webhook_key_hash(key);
    f.store.mutate(|s| s.set_routine_webhook(&routine.id, hash, "-key".into())).await.unwrap();
    let now = f.store.now();
    let claimed = f.store.mutate(|s| s.claim_webhook_routine(&routine.id, key, Some("please write AUTO"), now)).await.unwrap();
    f.provider.plan(vec![
        use_tool("write_file", json!({"path": "notes.txt", "content": "webhook write\n"})),
        text("Webhook asked."),
    ]);
    f.runtime.start(&claimed.conversation_id, &claimed.prompt).await.unwrap();
    f.decide(&claimed.conversation_id, "reject_once").await;
    f.completed(&claimed.conversation_id).await;
    assert_eq!(f.notes(), "written by my rule\n");

    // Removing the rules restores the default: ask.
    for rule in f.store.read(|s| s.permission_rules().to_vec()) {
        f.store.mutate(|s| s.delete_permission_rule(&rule.id, true)).await.unwrap();
    }
    f.provider.plan(vec![
        use_tool("write_file", json!({"path": "notes.txt", "content": "default asks\n"})),
        text("Default."),
    ]);
    f.runtime.start(&id, "Write by default").await.unwrap();
    f.decide(&id, "allow_once").await;
    f.completed(&id).await;
    assert_eq!(f.notes(), "default asks\n");
}

#[test]
fn runtime_on_the_in_process_engine() {
    let base = std::env::temp_dir().join(format!("grok-bot-runtime-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let base = std::fs::canonicalize(&base).unwrap();
    let engine_root = base.join("engine");
    // SAFETY: set before the test runtime and engine threads start.
    unsafe { grok_bot_engine::host::prepare_process_environment(&engine_root) };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        macro_rules! scenario {
            ($name:literal, $options:expr, $body:ident) => {{
                let started = Instant::now();
                let f = Fixture::new(&base, &engine_root, $name, $options).await;
                $body(&f).await;
                f.close().await;
                eprintln!("[runtime] {} ok in {:?}", $name, started.elapsed());
            }};
        }
        scenario!("files", Options::default(), file_approvals_and_sessions);
        scenario!("scripted", Options::default(), scripted_file_reply_flow);
        scenario!("fingerprint", Options::default(), fingerprint_migration);
        scenario!("provider-failure", Options::default(), provider_failure);
        scenario!("redaction", Options::default(), streamed_redaction);
        scenario!(
            "deadlines",
            Options {
                idle: Duration::from_millis(400),
                ..Options::default()
            },
            deadlines_and_cancellation
        );
        scenario!("limits", Options::default(), write_limits_and_model_only);
        scenario!("skills", Options::default(), skills);
        scenario!("bot-messages", Options::default(), bot_messages);
        scenario!("bot-creation", Options::default(), bot_creation);
        scenario!("bot-data", Options::default(), bot_memory_and_knowledge);
        scenario!("permission-rules", Options::default(), permission_rules);
        scenario!("restart", Options::default(), restart_continuation);
        startup_errors(&base, &engine_root).await;
    });
    let _ = std::fs::remove_dir_all(&base);
}
