//! HTTP tests for vault secrets and custom MCP servers: loopback-only changes (the server has no
//! login), LAN read-only views, write-only secret values, and the loopback-only MCP JSON route.

mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use common::{TestApp, run};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The server treats only 127.0.0.1 and ::1 as loopback clients, so 127.0.0.2 stands in for a LAN
/// client (as in `permissions_webhooks.rs`).
const LOCAL: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);
const LAN: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);
const VALUE: &str = "http-vault-secret-value-42";

async fn api(app: &TestApp, source: Ipv4Addr, method: &str, route: &str, body: Option<Value>) -> (u16, Value) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind(SocketAddr::new(IpAddr::V4(source), 0)).unwrap();
    let mut stream = socket.connect(SocketAddr::from(([127, 0, 0, 1], app.port))).await.unwrap();
    let body = body.map(|b| b.to_string());
    let mut request = format!(
        "{method} {route} HTTP/1.1\r\nConnection: close\r\nHost: 127.0.0.1:{}\r\nOrigin: {}\r\nX-Grok-Bot: 1\r\nContent-Type: application/json\r\n",
        app.port, app.origin
    );
    if let Some(body) = &body {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    request.push_str(body.as_deref().unwrap_or_default());
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response).await;
    let text = String::from_utf8_lossy(&response).into_owned();
    let status = text.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let (head, body) = text.split_once("\r\n\r\n").map(|(h, b)| (h.to_owned(), b.to_owned())).unwrap_or_default();
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        body.split("\r\n").skip(1).step_by(2).collect::<Vec<_>>().join("")
    } else {
        body
    };
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

#[test]
fn vault_and_custom_mcp_routes_are_loopback_only() {
    let log = Log::default();
    let writer = log.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt().with_max_level(tracing::Level::TRACE).without_time().with_ansi(false).with_writer(move || writer.clone()).finish(),
    )
    .unwrap();
    run("vault-mcp", async |root| {
        let engine = root.join("engine");
        let app = TestApp::start(&engine, root.join("data")).await;
        let bot = app.store.snapshot().bots[0].id.clone();
        assert_eq!(api(&app, LOCAL, "GET", "/api/vault", None).await, (200, json!({ "canEdit": true })));
        assert_eq!(api(&app, LAN, "GET", "/api/vault", None).await, (200, json!({ "canEdit": false })));

        // Secrets: LAN clients cannot add, replace, or delete; values never come back.
        let input = json!({ "name": "API_TOKEN", "value": VALUE, "botId": bot });
        let (status, error) = api(&app, LAN, "POST", "/api/secrets", Some(input.clone())).await;
        assert_eq!(status, 403, "{error}");
        assert!(app.store.snapshot().secrets.is_empty());
        assert_eq!(api(&app, LOCAL, "POST", "/api/secrets", Some(json!({ "name": "API_TOKEN", "value": VALUE, "extra": 1 }))).await.0, 400);
        let (status, state) = api(&app, LOCAL, "POST", "/api/secrets", Some(input)).await;
        assert_eq!(status, 201);
        assert_eq!(
            state["secrets"],
            json!([{ "id": state["secrets"][0]["id"], "name": "API_TOKEN", "botId": bot, "createdAt": state["secrets"][0]["createdAt"], "updatedAt": state["secrets"][0]["updatedAt"] }])
        );
        let id = state["secrets"][0]["id"].as_str().unwrap().to_owned();
        for source in [LOCAL, LAN] {
            let (_, state) = api(&app, source, "GET", "/api/state", None).await;
            assert!(!state.to_string().contains(VALUE));
            assert_eq!(state["secrets"][0]["name"], "API_TOKEN");
        }
        assert_eq!(api(&app, LAN, "PUT", &format!("/api/secrets/{id}"), Some(json!({ "value": "lan-replacement-value" }))).await.0, 403);
        assert_eq!(api(&app, LAN, "DELETE", &format!("/api/secrets/{id}"), Some(json!({}))).await.0, 403);
        let (status, state) = api(&app, LOCAL, "PUT", &format!("/api/secrets/{id}"), Some(json!({ "value": "local-replacement-value" }))).await;
        assert_eq!(status, 200);
        assert!(!state.to_string().contains("local-replacement-value"));
        assert_eq!(app.store.read(|s| s.secrets()[0].value.clone()), "local-replacement-value");

        // Custom MCP servers: the JSON route and every change that can run a server are loopback only.
        let route = format!("/api/bots/{bot}/mcp-servers");
        let document = json!({ "mcpServers": { "tools": { "url": "https://mcp.example.com/mcp", "headers": { "Authorization": "Bearer ${secret:API_TOKEN}", "X-Literal": "literal-header" } } } }).to_string();
        assert_eq!(api(&app, LAN, "PUT", &route, Some(json!({ "json": document }))).await.0, 403);
        let (status, error) = api(&app, LOCAL, "PUT", &route, Some(json!({ "json": "{\"workspace\": {\"url\": \"http://x\"}}" }))).await;
        assert_eq!(status, 400, "{error}");
        let (status, state) = api(&app, LOCAL, "PUT", &route, Some(json!({ "json": document }))).await;
        assert_eq!(status, 200);
        assert_eq!(state["mcpServers"][0]["name"], "tools");
        assert_eq!(state["mcpServers"][0]["enabled"], false, "servers start disabled");
        assert_eq!(state["mcpServers"][0]["secretRefs"], json!(["API_TOKEN"]));
        assert_eq!(api(&app, LAN, "GET", &route, None).await.0, 403, "the JSON can hold literal credentials");
        let (status, body) = api(&app, LOCAL, "GET", &route, None).await;
        assert_eq!(status, 200);
        let served: Value = serde_json::from_str(body["json"].as_str().unwrap()).unwrap();
        assert_eq!(served["mcpServers"]["tools"]["headers"]["X-Literal"], "literal-header");
        let (_, lan_state) = api(&app, LAN, "GET", "/api/state", None).await;
        assert!(!lan_state.to_string().contains("mcp.example.com") && !lan_state.to_string().contains("literal-header"));

        let toggle = format!("{route}/tools");
        assert_eq!(api(&app, LAN, "PATCH", &toggle, Some(json!({ "enabled": true }))).await.0, 403);
        let (status, state) = api(&app, LOCAL, "PATCH", &toggle, Some(json!({ "enabled": true }))).await;
        assert_eq!((status, &state["mcpServers"][0]["enabled"]), (200, &json!(true)));
        let (status, state) = api(&app, LAN, "PATCH", &toggle, Some(json!({ "enabled": false }))).await;
        assert_eq!((status, &state["mcpServers"][0]["enabled"]), (200, &json!(false)), "disabling is allowed from LAN");
        assert_eq!(api(&app, LOCAL, "PATCH", &format!("{route}/missing"), Some(json!({ "enabled": true }))).await.0, 404);

        // mcp_tool rules: only loopback may allow without asking.
        let rule = json!({ "category": "mcp_tool", "action": "allow", "botId": bot, "server": "tools" });
        assert_eq!(api(&app, LAN, "POST", "/api/permission-rules", Some(rule.clone())).await.0, 403);
        let (status, state) = api(&app, LOCAL, "POST", "/api/permission-rules", Some(rule)).await;
        assert_eq!((status, &state["permissionRules"][0]["server"]), (201, &json!("tools")));

        assert_eq!(api(&app, LOCAL, "DELETE", &format!("/api/secrets/{id}"), Some(json!({}))).await.0, 200);
        assert!(app.store.snapshot().secrets.is_empty());
        draft_probe_routes(&app, &bot).await;
        let captured = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        for secret in ["probe-scoped-secret", "probe-global-secret", "probe-direct-secret", "literal-probe-credential"] {
            assert!(!captured.contains(secret), "probe credentials must not reach any SDK/application log");
        }
        app.close().await;
    });
}

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
struct PeerState {
    requests: Arc<Mutex<Vec<Value>>>,
    store: Arc<grok_bot_store::Store>,
}

struct McpPeer {
    url: String,
    state: PeerState,
    task: tokio::task::JoinHandle<()>,
}

impl McpPeer {
    async fn start(app: &TestApp) -> Self {
        let state = PeerState { requests: Default::default(), store: app.store.clone() };
        let router = axum::Router::new().route("/mcp/{mode}", axum::routing::any(mcp_peer)).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { url, state, task }
    }
}

impl Drop for McpPeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn mcp_peer(
    State(state): State<PeerState>,
    Path(mode): Path<String>,
    method: axum::http::Method,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    // The API must release the store lock before talking to the peer.
    let _ = state.store.read(|s| s.secrets().len());
    state.requests.lock().unwrap().push(json!({
        "mode": mode, "http": method.as_str(), "body": body,
        "authorization": headers.get("authorization").and_then(|v| v.to_str().ok()),
        "global": headers.get("x-global").and_then(|v| v.to_str().ok()),
        "direct": headers.get("x-direct").and_then(|v| v.to_str().ok()),
        "literal": headers.get("x-literal").and_then(|v| v.to_str().ok()),
    }));
    if mode == "redirect" {
        return (StatusCode::TEMPORARY_REDIRECT, [("location", "/mcp/modern")]).into_response();
    }
    if mode == "echo" {
        return (
            StatusCode::UNAUTHORIZED,
            [("www-authenticate", "Bearer literal-probe-credential")],
            "credential echo: probe-scoped-secret probe-global-secret probe-direct-secret literal-probe-credential",
        )
            .into_response();
    }
    if method == axum::http::Method::GET {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if method == axum::http::Method::DELETE {
        return StatusCode::OK.into_response();
    }
    let rpc = body["method"].as_str().unwrap_or("");
    let id = body["id"].clone();
    if mode == "oversize" {
        return Json(json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32603,"message":"x".repeat(1024 * 1024 + 1)}})).into_response();
    }
    if mode == "oversize-chunked" {
        return (
            [("content-type", "application/json")],
            axum::body::Body::from_stream(futures::stream::once(async { Ok::<_, std::io::Error>("x".repeat(1024 * 1024 + 1)) })),
        )
            .into_response();
    }
    if mode == "legacy-http" && rpc == "server/discover" {
        return (StatusCode::BAD_REQUEST, "legacy literal-probe-credential").into_response();
    }
    let result = match rpc {
        "server/discover" if mode == "modern" => serde_json::to_value(
            rmcp::model::DiscoverResult::new(
                vec![rmcp::model::ProtocolVersion::V_2026_07_28],
                rmcp::model::ServerCapabilities::builder().enable_tools().build(),
            )
            .with_server_info(rmcp::model::Implementation::new("literal-probe-credential", "1")),
        )
        .unwrap(),
        "server/discover" => {
            return Json(json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32601,"message":"legacy literal-probe-credential"}})).into_response();
        }
        "initialize" => {
            assert_eq!(body["params"]["protocolVersion"], "2025-11-25", "initialize-era version pin");
            assert!(body["params"]["capabilities"].get("sampling").is_none());
            assert!(body["params"]["capabilities"].get("elicitation").is_none());
            json!({"protocolVersion":"2025-11-25", "capabilities":if mode == "empty" { json!({}) } else { json!({"tools":{}}) }, "serverInfo":{"name":"literal-probe-credential", "version":"1"}})
        }
        "notifications/initialized" => return StatusCode::ACCEPTED.into_response(),
        "tools/list" if mode == "rpc-error" => {
            return Json(
                json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32603,"message":"probe-scoped-secret probe-direct-secret literal-probe-credential"}}),
            )
            .into_response();
        }
        "tools/list" => {
            let tool = json!({"name":"read_only", "description":"literal-probe-credential", "inputSchema":{"type":"object"}});
            if mode == "cursor-loop" {
                json!({"tools":[tool], "nextCursor":"repeat"})
            } else if body["params"]["cursor"] == "more" {
                json!({"tools":[tool]})
            } else {
                json!({"tools":[tool], "nextCursor":"more"})
            }
        }
        other => panic!("test must never invoke a tool or model hook: {other}"),
    };
    let message = json!({"jsonrpc":"2.0", "id":id, "result":result});
    let mut response =
        if mode == "sse" { ([("content-type", "text/event-stream")], format!("data: {message}\n\n")).into_response() } else { Json(message).into_response() };
    if rpc == "initialize" {
        response.headers_mut().insert("mcp-session-id", "probe-session".parse().unwrap());
    }
    response
}

async fn draft_probe_routes(app: &TestApp, bot: &str) {
    let route = format!("/api/bots/{bot}/mcp-servers/test");
    let peer = McpPeer::start(app).await;
    for input in [
        json!({"name":"PROBE_TOKEN", "value":"probe-global-secret"}),
        json!({"name":"PROBE_TOKEN", "value":"probe-scoped-secret", "botId":bot}),
        json!({"name":"GLOBAL_TOKEN", "value":"probe-global-secret"}),
        json!({"name":"DIRECT_TOKEN", "value":"global-direct-fallback"}),
    ] {
        assert_eq!(api(app, LOCAL, "POST", "/api/secrets", Some(input)).await.0, 201);
    }
    let document = |mode: &str| {
        json!({"mcpServers":{"draft":{"url":format!("{}/mcp/{mode}", peer.url), "headers":{
        "Authorization":"Bearer ${secret:PROBE_TOKEN}", "X-Global":"${secret:GLOBAL_TOKEN}", "X-Direct":"${secret:DIRECT_TOKEN}", "X-Literal":"literal-probe-credential"
    }}}}).to_string()
    };
    let body = |mode: &str| json!({"json":document(mode), "secrets":[{"name":"DIRECT_TOKEN", "value":" probe-direct-secret \n", "botId":bot}]});
    let disk = std::fs::read(app.data_dir.join("state.json")).unwrap();
    let snapshot = serde_json::to_value(app.store.snapshot()).unwrap();
    let mut events = app.store.subscribe();
    for (mode, count) in [("legacy", 2), ("legacy-http", 2), ("modern", 2), ("sse", 2), ("empty", 0)] {
        let result = api(app, LOCAL, "POST", &route, Some(body(mode))).await;
        assert_eq!(result, (200, json!({"toolCount":count})), "{mode}");
    }
    assert_eq!(std::fs::read(app.data_dir.join("state.json")).unwrap(), disk);
    assert_eq!(serde_json::to_value(app.store.snapshot()).unwrap(), snapshot);
    assert!(events.try_recv().is_err(), "probe must not broadcast state changes");
    {
        let requests = peer.state.requests.lock().unwrap();
        for request in requests.iter() {
            assert_eq!(request["authorization"], "Bearer probe-scoped-secret");
            assert_eq!(request["global"], "probe-global-secret");
            assert_eq!(request["direct"], "probe-direct-secret");
            assert_eq!(request["literal"], "literal-probe-credential");
            assert_ne!(request["body"]["method"], "tools/call");
        }
        for mode in ["legacy", "legacy-http", "sse"] {
            let methods: Vec<_> = requests.iter().filter(|r| r["mode"] == mode && r["http"] == "POST").map(|r| r["body"]["method"].as_str().unwrap()).collect();
            assert_eq!(methods, ["server/discover", "initialize", "notifications/initialized", "tools/list", "tools/list"]);
            assert!(requests.iter().any(|r| r["mode"] == mode && r["http"] == "DELETE"));
        }
        let modern: Vec<_> = requests.iter().filter(|r| r["mode"] == "modern" && r["http"] == "POST").map(|r| r["body"]["method"].as_str().unwrap()).collect();
        assert_eq!(modern, ["server/discover", "tools/list", "tools/list"]);
        assert!(!requests.iter().any(|r| r["mode"] == "empty" && r["body"]["method"] == "tools/list"));
    }
    for (mode, category) in [
        ("echo", "인증 오류"),
        ("rpc-error", "도구 조회 오류"),
        ("cursor-loop", "도구 조회 오류"),
        ("oversize", "응답 오류"),
        ("oversize-chunked", "응답 오류"),
        ("redirect", "연결 오류"),
    ] {
        let (status, error) = api(app, LOCAL, "POST", &route, Some(body(mode))).await;
        assert_eq!(status, 502, "{mode}: {error}");
        assert!(error["error"].as_str().unwrap().contains(category), "{mode}: {error}");
        for value in ["probe-scoped-secret", "probe-direct-secret", "literal-probe-credential", &peer.url] {
            assert!(!error.to_string().contains(value));
        }
    }
    assert!(
        !peer.state.requests.lock().unwrap().iter().skip_while(|r| r["mode"] != "redirect").any(|r| r["mode"] == "modern"),
        "no redirects followed, even same-origin"
    );
    let requests = peer.state.requests.lock().unwrap().len();
    for json in [
        "{",
        "{}",
        r#"{"a":{"url":"http://x"},"b":{"url":"http://x"}}"#,
        r#"{"a":{"url":"http://x"},"a":{"url":"http://x"}}"#,
        r#"{"a":{"url":"http://x","headers":{"X":"${secret:MISSING}"}}}"#,
        r#"{"a":{"url":"http://x","literal-probe-credential":1}}"#,
    ] {
        let (status, error) = api(app, LOCAL, "POST", &route, Some(json!({"json":json}))).await;
        assert_eq!(status, 400, "{error}");
        assert!(!error.to_string().contains("literal-probe-credential"));
    }
    for input in [
        json!({"json":document("legacy"), "secrets":[{"name":"lower", "value":"probe-direct-secret"}]}),
        json!({"json":document("legacy"), "secrets":[{"name":"NEW_TOKEN", "value":"short"}]}),
        json!({"json":document("legacy"), "secrets":[{"name":"NEW_TOKEN", "value":"probe-direct-secret", "botId":grok_bot_model::new_id()}]}),
        json!({"json":document("legacy"), "secrets":[{"name":"DIRECT_TOKEN", "value":"probe-direct-secret"},{"name":" DIRECT_TOKEN ", "value":"probe-direct-secret"}]}),
        json!({"json":document("legacy"), "literal-probe-credential":"probe-direct-secret"}),
    ] {
        assert_eq!(api(app, LOCAL, "POST", &route, Some(input)).await.0, 400);
    }
    assert_eq!(
        api(app, LOCAL, "POST", &route, Some(json!({"json":document("legacy"), "secrets":[{"name":"PROBE_TOKEN", "value":"probe-direct-secret"}]}))).await.0,
        409
    );
    assert_eq!(api(app, LOCAL, "POST", &format!("/api/bots/{}/mcp-servers/test", grok_bot_model::new_id()), Some(body("legacy"))).await.0, 404);
    assert_eq!(api(app, LAN, "POST", &route, Some(json!({"json":9}))).await.0, 403, "check loopback before body parsing");
    assert_eq!(api(app, LAN, "POST", &route, Some(body("legacy"))).await.0, 403);
    assert_eq!(peer.state.requests.lock().unwrap().len(), requests, "invalid/LAN drafts must not contact any peer");
    assert_eq!(std::fs::read(app.data_dir.join("state.json")).unwrap(), disk);
    stdio_probe(app, bot, &route).await;

    // The static /test POST route preserves PATCH for a saved server literally named test.
    let base = format!("/api/bots/{bot}/mcp-servers");
    assert_eq!(api(app, LOCAL, "PUT", &base, Some(json!({"json":r#"{"test":{"url":"http://127.0.0.1:1"}}"#}))).await.0, 200);
    assert_eq!(api(app, LOCAL, "PATCH", &route, Some(json!({"enabled":true}))).await.0, 200);
}

async fn stdio_probe(app: &TestApp, bot: &str, route: &str) {
    let log = app.data_dir.join("stdio-probe.json");
    let script = r#"
const fs = require('fs'), cp = require('child_process');
const child = cp.spawn(process.execPath, ['-e', 'setInterval(()=>{},1000)'], {stdio:'ignore'});
const path = process.argv[1], methods = [];
const save = () => fs.writeFileSync(path, JSON.stringify({pid:process.pid, child:child.pid, methods}));
save();
require('readline').createInterface({input:process.stdin}).on('line', line => {
  const req = JSON.parse(line); methods.push(req.method); save();
  let result;
  if (req.method === 'initialize') {
    if (req.params.protocolVersion !== '2025-11-25' || req.params.capabilities.sampling || process.env.DIRECT_TOKEN !== 'probe-direct-secret') process.exit(2);
    result = {protocolVersion:'2025-11-25', capabilities:{tools:{}}, serverInfo:{name:'literal-probe-credential',version:'1'}};
  } else if (req.method === 'tools/list') result = {tools:[{name:'smoke', inputSchema:{type:'object'}}]};
  else if (req.method === 'notifications/initialized') return;
  else process.exit(3);
  console.log(JSON.stringify({jsonrpc:'2.0',id:req.id,result}));
});
setInterval(()=>{},1000);
"#;
    let input = json!({"json":json!({"smoke":{"command":"node", "args":["-e",script,log], "env":{"DIRECT_TOKEN":"${secret:DIRECT_TOKEN}"}}}).to_string(),
        "secrets":[{"name":"DIRECT_TOKEN", "value":"probe-direct-secret", "botId":bot}]});
    let result = api(app, LOCAL, "POST", route, Some(input)).await;
    assert_eq!(result, (200, json!({"toolCount":1})));
    let record: Value = serde_json::from_slice(&std::fs::read(&log).unwrap()).unwrap();
    assert_eq!(record["methods"], json!(["initialize", "notifications/initialized", "tools/list"]));
    #[cfg(unix)]
    {
        let pid = record["pid"].as_i64().unwrap() as i32;
        // SAFETY: signal 0 tests the former child PID without sending a signal.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "direct child has been killed AND waited");
        let child = record["child"].as_i64().unwrap();
        // SIGKILL is sent to the whole group, but the kernel can schedule the direct child's
        // wait completion before a descendant has observed its pending signal.
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let status = tokio::fs::read_to_string(format!("/proc/{child}/stat")).await.unwrap_or_default();
                if status.is_empty() || status.split_whitespace().nth(2) == Some("Z") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the process-group descendant must not remain running");
    }
    let marker = app.data_dir.join("lan-must-not-spawn");
    let input = json!({"json":json!({"nope":{"command":"node", "args":["-e", "require('fs').writeFileSync(process.argv[1],'spawned')",marker]}}).to_string()});
    assert_eq!(api(app, LAN, "POST", route, Some(input)).await.0, 403);
    assert!(!marker.exists());
}
