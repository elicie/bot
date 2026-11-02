//! HTTP tests for vault secrets and custom MCP servers: loopback-only changes (the server has no
//! login), LAN read-only views, write-only secret values, and the loopback-only MCP JSON route.

mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

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
        app.close().await;
    });
}
