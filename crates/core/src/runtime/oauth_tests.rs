//! Deterministic synthetic OAuth -> service -> Runtime -> ACP -> real loopback MCP -> engine test.
//! It is deliberately NOT real-account or real-authorization-server acceptance. Private in-memory
//! network fixtures replace only external discovery/token exchange and the relay's HTTPS upstream.
//! The real service callback/vault, runtime, engine, local HTTP relay, broker and state SSE all run.

use super::*;

use std::net::SocketAddr;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State as AxumState;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures::StreamExt;
use grok_bot_model::{Conversation, PermissionAction, PermissionCategory, PermissionRuleInput, RunStatus, SettingsInput, ToolStatus};
use grok_bot_store::Store;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

use crate::http::{self, HttpContext};
use crate::mcp_oauth::relay::test_support::{self, ACCESS, REFRESH, REMOTE, Upstream};
use crate::queue::CommandQueue;
use crate::runtime::RuntimeOptions;
use crate::scheduler::{RoutineScheduler, SchedulerOptions};

const WAIT: Duration = Duration::from_secs(30);
const CHILD: &str = "GROK_BOT_SYNTHETIC_OAUTH_RUNTIME_CHILD_4777";
const TEST: &str = "runtime::oauth::tests::synthetic_oauth_runtime_end_to_end";

#[test]
fn synthetic_oauth_runtime_end_to_end() {
    if std::env::var_os(CHILD).is_some_and(|value| value == "1") {
        let root = tempfile::tempdir().unwrap();
        let engine_root = root.path().join("engine");
        // SAFETY: this exact-filtered child runs one test only. No Tokio, store writer, engine,
        // HTTP server or other test is started before the process-global environment is prepared.
        unsafe { grok_bot_engine::host::prepare_process_environment(&engine_root) };
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(90), exercise(root.path(), &engine_root)).await.expect("synthetic OAuth runtime test deadline");
        });
        return;
    }
    // Do not change HOME/GROK_HOME in the parallel unit-test process. current_exe is this core
    // unit-test binary, not a new application server or a shared integration-test harness.
    let root = tempfile::tempdir().unwrap();
    let output = tempfile::NamedTempFile::new_in(root.path()).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .current_dir(root.path())
        .stdout(Stdio::from(output.as_file().try_clone().unwrap()))
        .stderr(Stdio::from(output.as_file().try_clone().unwrap()))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("synthetic OAuth runtime child exceeded its deadline");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "synthetic OAuth runtime child failed:\n{}", std::fs::read_to_string(output.path()).unwrap());
}

#[derive(Clone)]
enum Reply {
    Tool(&'static str, Value),
    Text(Vec<String>),
}

#[derive(Default)]
struct Provider {
    plan: std::sync::Mutex<std::collections::VecDeque<Reply>>,
    requests: std::sync::Mutex<Vec<Value>>,
}

fn sse(model: &str, deltas: Vec<Value>, finish: &str) -> Response {
    let chunk = |delta: Value, reason: Value| json!({"id":"synthetic-oauth", "object":"chat.completion.chunk", "created":1, "model":model, "choices":[{"index":0,"delta":delta,"finish_reason":reason}]});
    let mut body = String::new();
    for value in std::iter::once(chunk(json!({"role":"assistant"}), Value::Null))
        .chain(deltas.into_iter().map(|delta| chunk(delta, Value::Null)))
        .chain(std::iter::once(chunk(json!({}), json!(finish))))
    {
        body.push_str(&format!("data: {value}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

async fn completions(AxumState(provider): AxumState<Arc<Provider>>, Json(request): Json<Value>) -> Response {
    let main = request["tools"].as_array().into_iter().flatten().any(|tool| tool["function"]["name"] == "search_tool");
    let model = request["model"].as_str().unwrap();
    provider.requests.lock().unwrap().push(request.clone());
    let reply = if main {
        provider.plan.lock().unwrap().pop_front().expect("unexpected extra synthetic model inference")
    } else {
        Reply::Text(vec!["Synthetic OAuth fixture".into()])
    };
    match reply {
        Reply::Tool(name, arguments) => sse(
            model,
            vec![
                json!({"tool_calls":[{"index":0,"id":format!("synthetic-{}", provider.requests.lock().unwrap().len()), "type":"function", "function":{"name":name,"arguments":arguments.to_string()}}]}),
            ],
            "tool_calls",
        ),
        Reply::Text(chunks) => sse(model, chunks.into_iter().map(|chunk| json!({"content":chunk})).collect(), "stop"),
    }
}

async fn serve(router: Router) -> (String, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
    });
    (origin, server)
}

async fn wait_for(store: &Store, id: &str, predicate: impl Fn(&Conversation) -> bool) -> Conversation {
    let mut events = store.subscribe();
    tokio::time::timeout(WAIT, async {
        loop {
            let conversation = store.read(|state| state.conversation(id).unwrap().clone());
            if predicate(&conversation) {
                return conversation;
            }
            events.recv().await.unwrap();
        }
    })
    .await
    .expect("synthetic runtime state deadline")
}

fn local_declaration(runtime: &Runtime, id: &str) -> (String, String) {
    let run = runtime.run_for_conversation(id).unwrap();
    assert!(run._oauth_leases.len() == 1);
    // This is the production declaration builder that open_session passes to ACP, not a fake
    // ACP peer. The real engine's successful local HTTP handshake/call below proves consumption.
    let declarations = serde_json::to_value(runtime.mcp_servers(&run)).unwrap();
    let serialized = declarations.to_string();
    assert_no_tokens(&serialized);
    assert!(!serialized.contains(REMOTE));
    let remote = declarations.as_array().unwrap().iter().find(|server| server["name"] == "remote").unwrap();
    let url = remote["url"].as_str().unwrap().to_owned();
    let headers = remote["headers"].as_array().unwrap();
    assert!(headers.len() == 1 && headers[0]["name"] == "Authorization");
    let authorization = headers[0]["value"].as_str().unwrap().to_owned();
    assert!(url.starts_with("http://127.0.0.1:") && url.ends_with("/mcp"));
    assert!(authorization.starts_with("Bearer ") && authorization.len() == 71);
    assert!(authorization == run._oauth_leases[0].authorization && url == run._oauth_leases[0].url);
    (url, authorization)
}

async fn closed(url: &str) {
    let url = url::Url::parse(url).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if TcpStream::connect(("127.0.0.1", url.port().unwrap())).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the per-run OAuth lease listener must close");
}

#[track_caller]
fn assert_no_tokens(value: &str) {
    // Prefix checks also reject accidentally published intermediate split-token chunks.
    for forbidden in [ACCESS, REFRESH, &ACCESS[..20], &REFRESH[..20]] {
        assert!(!value.contains(forbidden), "synthetic OAuth credential escaped its private boundary");
    }
}

fn assert_engine_files_redacted(directory: &Path) {
    // These directories belong exclusively to this synthetic child fixture, never the caller's
    // actual HOME, credentials, notes or history. Do not follow links out of the fixture.
    for entry in std::fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            assert_engine_files_redacted(&entry.path());
        } else if kind.is_file() {
            assert_no_tokens(&String::from_utf8_lossy(&std::fs::read(entry.path()).unwrap()));
        }
    }
}

async fn exercise(root: &Path, engine_root: &Path) {
    let provider = Arc::new(Provider::default());
    let (provider_origin, provider_server) = serve(Router::new().route("/v1/chat/completions", post(completions)).with_state(provider.clone())).await;
    let store = Store::open(&root.join("data"), Arc::new(|| chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap())).await.unwrap();
    let queue = CommandQueue::new();
    let runtime = Runtime::new(store.clone(), queue.clone(), RuntimeOptions::new(engine_root));
    runtime.initialize().await;
    let scheduler = RoutineScheduler::new(store.clone(), runtime.clone(), queue.clone(), SchedulerOptions { auto_start: false, ..SchedulerOptions::default() });
    let context = Arc::new(HttpContext::new(store.clone(), runtime.clone(), scheduler.clone(), queue, http::default_allowed_hosts(&[]), None));
    let (origin, broker_server) = serve(http::router(context.clone())).await;
    runtime.set_origin(origin.clone());
    store
        .mutate(|state| {
            state.update_settings(SettingsInput {
                base_url: format!("{provider_origin}/v1"),
                model: "synthetic-oauth-model".into(),
                workspace: String::new(),
                api_key: None,
                clear_api_key: Some(true),
            })
        })
        .await
        .unwrap();
    let bot = store.snapshot().bots[0].id.clone();
    store.mutate(|state| state.set_mcp_servers(&bot, &json!({"mcpServers":{"remote":{"url":REMOTE}}}).to_string(), true)).await.unwrap();
    let record = store.read(|state| state.mcp_servers_for(&bot).next().unwrap().clone());
    let service = test_support::connected_service(store.clone(), &record).await;
    runtime.set_oauth(service.clone());
    // Match Backend's default provider boundary as well: a provider that deliberately invents or
    // echoes a known OAuth token must not put it into native engine history before projection.
    runtime.set_relay(crate::provider_relay::start(store.clone()).await.unwrap());
    assert!(service.status(&record).await.status == "connected");
    assert!(!store.read(|state| state.mcp_servers_for(&bot).next().unwrap().enabled), "OAuth login never enables a capability");
    store.mutate(|state| state.set_mcp_server_enabled(&bot, "remote", true, true)).await.unwrap();
    let saved_config = store.read(|state| state.mcp_json(&bot).unwrap());
    let upstream = Arc::new(Upstream::default());
    assert!(runtime.oauth_test_upstream.set(upstream.clone()).is_ok());
    let id = store.mutate(|state| state.add_conversation(&bot)).await.unwrap().id;
    let mut broadcast = store.subscribe();
    let client = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).connect_timeout(Duration::from_secs(5)).build().unwrap();
    let response = client.get(format!("{origin}/api/events")).send().await.unwrap();
    assert!(response.status().is_success());
    assert!(response.headers()[header::CONTENT_TYPE] == "text/event-stream");
    let events = tokio::spawn(async move {
        let mut stream = response.bytes_stream();
        let mut wire = Vec::new();
        while let Some(chunk) = stream.next().await {
            wire.extend_from_slice(&chunk.unwrap());
        }
        String::from_utf8(wire).unwrap()
    });

    provider.plan.lock().unwrap().extend([
        Reply::Tool("search_tool", json!({"query":"remote lookup", "limit":5})),
        Reply::Tool("use_tool", json!({"tool_name":"remote__lookup", "tool_input":{"query":"alpha"}})),
        Reply::Text(vec!["OAuth lookup returned ".into(), ACCESS[..ACCESS.len() - 1].into(), format!("{} safely.", &ACCESS[ACCESS.len() - 1..])]),
    ]);
    runtime.start(&id, "Look up the synthetic alpha record.").await.unwrap();
    let first_lease = local_declaration(&runtime, &id);
    let pending = wait_for(&store, &id, |conversation| conversation.run.status == RunStatus::AwaitingPermission || conversation.run.status.is_terminal()).await;
    assert!(pending.run.status == RunStatus::AwaitingPermission, "the real custom MCP permission request must appear");
    let permission = pending.run.permission.unwrap();
    assert!(permission.title == "MCP 서버 remote: lookup" && permission.description.contains("alpha"));
    let allow = permission.options.iter().find(|option| option.kind == "allow_once").unwrap();
    upstream.assert_not_called();
    runtime.resolve(&id, &permission.id, &allow.id, false).await.unwrap();
    let completed = wait_for(&store, &id, |conversation| conversation.run.status.is_terminal()).await;
    assert!(completed.run.status == RunStatus::Completed);
    assert!(completed.messages.last().unwrap().text == "OAuth lookup returned [redacted] safely.");
    assert!(completed.messages.last().unwrap().tools.iter().any(|tool| tool.status == ToolStatus::Completed
        && tool.output.as_deref().is_some_and(|output| output.contains("oauth-tool-ok:alpha") && output.contains("[redacted]"))));
    upstream.assert_used();
    closed(&first_lease.0).await;
    assert_engine_files_redacted(engine_root);
    assert_engine_files_redacted(&store.data_dir().join("grok"));
    assert!(store.read(|state| state.mcp_json(&bot).unwrap()) == saved_config);

    // A real next turn loads history through the engine, then cancellation interrupts an actual
    // upstream tool call instead of merely cancelling a mocked transport or an idle provider.
    store
        .mutate(|state| {
            state.add_permission_rule(
                PermissionRuleInput {
                    category: PermissionCategory::McpTool,
                    action: PermissionAction::Allow,
                    bot_id: Some(bot.clone()),
                    server: Some("remote".into()),
                },
                true,
            )
        })
        .await
        .unwrap();
    upstream.hold_next_call();
    provider.plan.lock().unwrap().push_back(Reply::Tool("use_tool", json!({"tool_name":"remote__lookup", "tool_input":{"query":"cancel"}})));
    runtime.start(&id, "Start a synthetic lookup that I will stop.").await.unwrap();
    let cancel_lease = local_declaration(&runtime, &id);
    assert!(cancel_lease.1 != first_lease.1, "each turn gets a new lease credential even if the OS reuses a port");
    upstream.wait_holding().await;
    assert!(store.read(|state| state.conversation(&id).unwrap().session_id.clone()) == completed.session_id, "the real engine loaded the same history");
    tokio::time::timeout(WAIT, runtime.cancel(&id)).await.unwrap().unwrap();
    assert!(store.read(|state| state.conversation(&id).unwrap().run.status) == RunStatus::Cancelled);
    closed(&cancel_lease.0).await;
    upstream.assert_forward_dropped();

    // Drive an actual HTTP 401 through Runtime::prepare_oauth's own callback, without a second
    // engine retry policy obscuring the classification. A dropped prepared (unadmitted) lease
    // closes too; no direct service.report_unauthorized call is used in this test.
    let prepared = runtime.prepare_oauth(&id, &Turn::Prompt("Synthetic authorization check".into())).await.unwrap().unwrap();
    assert!(prepared.failures.is_empty() && prepared.leases.len() == 1);
    let lease = &prepared.leases[0];
    let denied_url = lease.url.clone();
    upstream.unauthorized_next();
    let denied = client.post(&lease.url).header(header::AUTHORIZATION, &lease.authorization).body("{}").send().await.unwrap();
    assert!(denied.status().as_u16() == 401);
    assert_no_tokens(&denied.text().await.unwrap());
    assert!(service.status(&record).await.status == "login_required");
    drop(prepared);
    closed(&denied_url).await;

    store.flush().await.unwrap();
    let requests = provider.requests.lock().unwrap().clone();
    assert!(requests.len() >= 4);
    assert_no_tokens(&serde_json::to_string(&requests).unwrap());
    let tool_results = requests
        .iter()
        .flat_map(|request| request["messages"].as_array().into_iter().flatten())
        .filter(|message| message["role"] == "tool")
        .map(|message| message["content"].to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(tool_results.contains("remote__lookup") && tool_results.contains("oauth-tool-ok:alpha") && tool_results.contains("[redacted]"));
    assert_no_tokens(&serde_json::to_string(&store.snapshot()).unwrap());
    assert_no_tokens(&store.read(|state| serde_json::to_string(state.disk()).unwrap()));
    assert_no_tokens(&std::fs::read_to_string(store.data_dir().join("state.json")).unwrap());
    let encrypted = std::fs::read(store.data_dir().join("mcp-oauth/credentials.enc")).unwrap();
    assert!(!encrypted.windows(ACCESS.len()).any(|bytes| bytes == ACCESS.as_bytes()));
    let mut broadcasts = 0;
    loop {
        match broadcast.try_recv() {
            Ok(state) => {
                broadcasts += 1;
                assert_no_tokens(&serde_json::to_string(&*state).unwrap());
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(_) => panic!("synthetic state broadcast collection lost an update"),
        }
    }
    assert!(broadcasts > 5);
    context.stopping.send_replace(true);
    let wire = tokio::time::timeout(WAIT, events).await.unwrap().unwrap();
    assert!(wire.contains("event: state\n") && wire.contains("OAuth lookup returned [redacted] safely."));
    assert_no_tokens(&wire);
    let payloads = wire
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .inspect(|line| {
            serde_json::from_str::<Value>(line).unwrap();
        })
        .count();
    assert!(payloads > 5, "initial and broadcast snapshots were delivered over real HTTP SSE");

    scheduler.close().await;
    runtime.shutdown().await;
    service.shutdown().await;
    broker_server.abort();
    provider_server.abort();
    let _ = broker_server.await;
    let _ = provider_server.await;
}
