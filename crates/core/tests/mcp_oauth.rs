//! Actual HTTP socket gates for OAuth; no remote account, discovery, or credential is contacted.
mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use common::{TestApp, run};
use grok_bot_model::{RunState, RunStatus, new_id};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn api(app: &TestApp, local: bool, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let source = if local { Ipv4Addr::LOCALHOST } else { Ipv4Addr::new(127, 0, 0, 2) };
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind(SocketAddr::new(IpAddr::V4(source), 0)).unwrap();
    let mut stream = socket.connect(SocketAddr::from(([127, 0, 0, 1], app.port))).await.unwrap();
    let body = body.map(|body| body.to_string()).unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: {}\r\nX-Grok-Bot: 1\r\nContent-Type: application/json\r\nForwarded: for=127.0.0.1\r\nX-Forwarded-For: 127.0.0.1\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        app.port,
        app.origin,
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.unwrap();
    let response = String::from_utf8(bytes).unwrap();
    let status = response.split(' ').nth(1).unwrap().parse().unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    let body = if headers.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        body.split("\r\n").skip(1).step_by(2).collect::<Vec<_>>().join("")
    } else {
        body.to_owned()
    };
    (status, serde_json::from_str(&body).unwrap())
}

#[test]
fn oauth_commands_enforce_socket_locality_idle_and_non_disclosure() {
    run("mcp-oauth-http", async |root| {
        let app = TestApp::start(&root.join("engine"), root.join("data")).await;
        let bot = app.store.snapshot().bots[0].id.clone();
        let config = json!({"mcpServers": {
            "remote": {"url": "https://mcp.example.invalid/mcp"},
            "plain": {"url": "http://127.0.0.1:1/mcp"},
            "manual": {"url": "https://mcp.example.invalid/mcp", "headers": {"Authorization": "Bearer manual-secret-canary"}},
            "stdio": {"command": "node", "args": ["never-start.js"]}
        }})
        .to_string();
        app.store.mutate(|state| state.set_mcp_servers(&bot, &config, true)).await.unwrap();
        let prefix = format!("/api/bots/{bot}/mcp-servers");
        let remote = format!("{prefix}/remote/oauth");
        let (code, status) = api(&app, true, "GET", &remote, None).await;
        assert_eq!(code, 200);
        assert_eq!(status["status"], "not_connected");
        assert_eq!(status["canLogin"], true);
        assert!(!status.to_string().contains("example.invalid"));
        let (_, status) = api(&app, false, "GET", &remote, None).await;
        assert_eq!(status["canLogin"], false);
        for name in ["plain", "manual", "stdio"] {
            let (code, status) = api(&app, true, "GET", &format!("{prefix}/{name}/oauth"), None).await;
            assert_eq!(code, 200);
            assert_eq!(status["status"], "unavailable");
            assert_eq!(status["canLogin"], false);
        }
        assert_eq!(api(&app, true, "GET", &format!("{prefix}/missing/oauth"), None).await.0, 404);
        // Rejection precedes deserialization and ignores spoofed loopback forwarding headers.
        for command in ["login", "logout"] {
            let (code, body) = api(&app, false, "POST", &format!("{remote}/{command}"), Some(json!({"private": "oauth-request-canary"}))).await;
            assert_eq!(code, 403);
            assert!(!body.to_string().contains("oauth-request-canary"));
        }
        let (code, body) = api(&app, true, "POST", &format!("{remote}/login"), Some(json!({"clientSecret": "oauth-request-canary"}))).await;
        assert_eq!(code, 400);
        assert!(!body.to_string().contains("oauth-request-canary"));
        let (code, _) = api(&app, true, "POST", &format!("{remote}/login"), Some(json!({"clientId": ""}))).await;
        assert_eq!(code, 400, "invalid client is rejected before any network request");
        let (code, _) = api(&app, true, "POST", &format!("{prefix}/plain/oauth/login"), Some(json!({}))).await;
        assert_eq!(code, 400, "HTTP can never initiate OAuth");
        let (code, body) = api(&app, true, "POST", &format!("{remote}/logout"), Some(json!({}))).await;
        assert_eq!(code, 200);
        assert_eq!(body["status"], "not_connected");
        assert!(app.store.snapshot().mcp_servers.iter().all(|server| !server.enabled), "login/logout never enables a server");
        let conversation = app.store.mutate(|state| state.add_conversation(&bot)).await.unwrap().id;
        app.store
            .mutate(|state| {
                state.conversation_mut(&conversation)?.run = RunState { id: new_id(), status: RunStatus::Running, ..RunState::idle() };
                Ok(())
            })
            .await
            .unwrap();
        for command in ["login", "logout"] {
            assert_eq!(api(&app, true, "POST", &format!("{remote}/{command}"), Some(json!({}))).await.0, 409);
        }
        let state = serde_json::to_string(&app.store.snapshot()).unwrap();
        assert!(!state.contains("oauth-request-canary"));
        assert!(!std::fs::read_to_string(root.join("data/state.json")).unwrap().contains("oauth-request-canary"));
        app.close().await;
    });
}
