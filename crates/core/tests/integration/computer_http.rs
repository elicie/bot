//! Managed-computer HTTP locality and LAN privacy. Ports `tests/computer-privacy.test.ts` and the
//! route-level cases of `tests/computer-api.test.ts`: every `/api/computer*` route needs a loopback
//! socket regardless of forwarded-address headers, and LAN snapshots, mutation responses, and
//! already-open SSE streams hide local-only computer approvals and tool details.

use crate::common;

use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::path::PathBuf;

use grok_bot_core::app::{Backend, BackendOptions};
use grok_bot_model::{Message, RunState};
use grok_bot_store::Store;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const CANARY: &str = "LOCAL_COMPUTER_QUERY_AND_ACTION_SECRET";

/// A non-loopback IPv4 address of this machine (no packet is sent).
fn lan_address() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

struct App {
    port: u16,
    lan: Ipv4Addr,
    store: std::sync::Arc<Store>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl App {
    async fn start(root: &std::path::Path, lan: Ipv4Addr) -> Self {
        let backend = Backend::new(BackendOptions {
            data_dir: root.join("data"),
            engine_root: root.join("engine"),
            web_root: None,
            embedded_web: None,
            allowed_hosts: vec![lan.to_string()],
            now: None,
            scheduler: None,
            runtime: None,
        })
        .await
        .unwrap();
        let store = backend.store.clone();
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(backend.serve(listener, async move {
            let _ = stopped.await;
        }));
        Self { port, lan, store, stop: Some(stop), task: Some(task) }
    }

    async fn close(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.task.take().unwrap().await.unwrap().unwrap();
    }

    /// A same-origin request through `address` (`127.0.0.1` or the LAN address).
    async fn request(&self, address: &str, method: &str, route: &str, body: Option<Value>, extra: &[(&str, &str)]) -> (u16, String) {
        let host = format!("{address}:{}", self.port);
        let mut headers = vec![
            ("Host".to_owned(), host.clone()),
            ("Origin".to_owned(), format!("http://{host}")),
            ("X-Grok-Bot".to_owned(), "1".to_owned()),
            ("Content-Type".to_owned(), "application/json".to_owned()),
        ];
        headers.extend(extra.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
        let mut stream = TcpStream::connect((address, self.port)).await.unwrap();
        let body = body.map(|b| b.to_string());
        let mut request = format!("{method} {route} HTTP/1.1\r\nConnection: close\r\n");
        for (name, value) in &headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        if let Some(body) = &body {
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");
        request.push_str(body.as_deref().unwrap_or(""));
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response).await;
        let text = String::from_utf8_lossy(&response).into_owned();
        let status = text.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        (status, text.split_once("\r\n\r\n").map(|(_, b)| b.to_owned()).unwrap_or_default())
    }

    fn lan(&self) -> String {
        self.lan.to_string()
    }
}

/// Read an SSE stream until `matches` accepts the accumulated text.
async fn read_until(stream: &mut TcpStream, seen: &mut String, matches: impl Fn(&str) -> bool) {
    let mut chunk = [0u8; 16 * 1024];
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !matches(seen) {
        let read = tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await.expect("SSE event timed out").unwrap();
        assert!(read > 0, "SSE stream closed");
        seen.push_str(&String::from_utf8_lossy(&chunk[..read]));
    }
}

#[test]
fn computer_http() {
    let Some(lan) = lan_address() else {
        eprintln!("skipping: this locality test requires a network interface");
        return;
    };
    common::run("computer-http", async |root: PathBuf| {
        let app = App::start(&root, lan).await;
        computer_routes_are_loopback_only(&app).await;
        lan_state_and_sse_hide_local_computer_details(&app).await;
        app.close().await;
    });
}

async fn computer_routes_are_loopback_only(app: &App) {
    let bot = app.store.read(|s| s.disk().bots[0].id.clone());
    let lan = app.lan();
    let local_only = "Managed browser control requires a local loopback connection.";
    let forwarded = [("X-Forwarded-For", "127.0.0.1"), ("Forwarded", "for=127.0.0.1")];
    for (method, route, body) in [
        ("GET", format!("/api/computer?owner={bot}"), None),
        (
            "POST",
            "/api/computer".to_owned(),
            Some(json!({ "ownerId": bot, "requestId": grok_bot_model::new_id(), "userInitiated": true, "origins": ["https://example.com"] })),
        ),
        ("DELETE", "/api/computer".to_owned(), Some(json!({ "ownerId": bot }))),
        ("GET", format!("/api/computer/preview?owner={bot}&epoch={0}&frame={0}", grok_bot_model::new_id()), None),
        (
            "POST",
            "/api/computer/actions".to_owned(),
            Some(
                json!({ "ownerId": bot, "requestId": grok_bot_model::new_id(), "epoch": grok_bot_model::new_id(), "frameId": null, "action": { "type": "observe" } }),
            ),
        ),
        ("POST", "/api/computer/actions".to_owned(), Some(json!({ "malformed": true }))),
    ] {
        let (status, text) = app.request(&lan, method, &route, body, &forwarded).await;
        assert_eq!(status, 403, "{method} {route}: {text}");
        assert!(text.contains(local_only), "{text}");
    }

    let (status, text) = app.request("127.0.0.1", "GET", &format!("/api/computer?owner={bot}"), None, &[]).await;
    assert_eq!(status, 200, "{text}");
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["state"], "stopped");
    assert_eq!(value["frameId"], Value::Null);
    assert_eq!(value["viewport"], json!({ "width": 960, "height": 600 }));
    assert_eq!(app.request("127.0.0.1", "GET", "/api/computer?owner=not-a-uuid", None, &[]).await.0, 400);
    assert_eq!(app.request("127.0.0.1", "GET", &format!("/api/computer?owner={bot}&extra=1"), None, &[]).await.0, 400);
    assert_eq!(app.request("127.0.0.1", "GET", &format!("/api/computer?owner={}", grok_bot_model::new_id()), None, &[]).await.0, 404);
    let preview = app.request("127.0.0.1", "GET", &format!("/api/computer/preview?owner={bot}&epoch={0}&frame={0}", grok_bot_model::new_id()), None, &[]).await;
    assert_eq!(preview.0, 409, "{}", preview.1);
    let refused = json!({ "ownerId": bot, "requestId": grok_bot_model::new_id(), "userInitiated": false, "origins": ["https://example.com"] });
    assert_eq!(app.request("127.0.0.1", "POST", "/api/computer", Some(refused), &[]).await.0, 400);
    // A private destination fails before any browser starts.
    let private = json!({ "ownerId": bot, "requestId": grok_bot_model::new_id(), "userInitiated": true, "origins": ["http://127.0.0.1:12345"] });
    let (status, text) = app.request("127.0.0.1", "POST", "/api/computer", Some(private), &[]).await;
    assert_eq!(status, 403, "{text}");
    assert!(text.contains("non-public address"), "{text}");
    let action =
        json!({ "ownerId": bot, "requestId": grok_bot_model::new_id(), "epoch": grok_bot_model::new_id(), "frameId": null, "action": { "type": "observe" } });
    assert_eq!(app.request("127.0.0.1", "POST", "/api/computer/actions", Some(action), &[]).await.0, 409);
    let missing_frame = json!({ "ownerId": bot, "requestId": grok_bot_model::new_id(), "epoch": grok_bot_model::new_id(), "action": { "type": "observe" } });
    assert_eq!(app.request("127.0.0.1", "POST", "/api/computer/actions", Some(missing_frame), &[]).await.0, 400);
    let (status, text) = app.request("127.0.0.1", "DELETE", "/api/computer", Some(json!({ "ownerId": bot })), &[]).await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("\"state\":\"stopped\""), "{text}");
}

async fn lan_state_and_sse_hide_local_computer_details(app: &App) {
    let bot = app.store.read(|s| s.disk().bots[0].id.clone());
    let lan = app.lan();
    let conversation = app.store.mutate(|s| s.add_conversation(&bot)).await.unwrap().id;

    let mut events = TcpStream::connect((lan.as_str(), app.port)).await.unwrap();
    events.write_all(format!("GET /api/events HTTP/1.1\r\nHost: {lan}:{}\r\n\r\n", app.port).as_bytes()).await.unwrap();
    let mut seen = String::new();
    read_until(&mut events, &mut seen, |text| text.contains("event: state")).await;
    let before = seen.len();

    let permission_id = grok_bot_model::new_id();
    let run: RunState = serde_json::from_value(json!({
        "id": grok_bot_model::new_id(), "status": "awaiting_permission",
        "permission": { "id": permission_id, "title": "Observe managed browser", "kind": "other", "description": format!("https://public.example/?token={CANARY}#{CANARY}"), "paths": [], "localOnly": true, "options": [{ "id": "once", "label": "Allow once", "kind": "allow_once" }] },
    }))
    .unwrap();
    let message: Message = serde_json::from_value(json!({
        "id": grok_bot_model::new_id(), "role": "assistant", "text": "", "createdAt": "2026-11-01T00:00:00.000Z",
        "tools": [{ "id": grok_bot_model::new_id(), "kind": "other", "status": "pending", "title": "workspace__computer_act", "input": CANARY, "output": CANARY, "localOnly": true }],
    }))
    .unwrap();
    let id = conversation.clone();
    app.store
        .mutate(move |s| {
            let conversation = s.conversation_mut(&id)?;
            conversation.run = run;
            conversation.messages.push(message);
            Ok(())
        })
        .await
        .unwrap();
    read_until(&mut events, &mut seen, |text| text[before..].contains("localOnly")).await;
    assert!(!seen.contains(CANARY), "SSE leaked local computer details");

    let (_, lan_state) = app.request(&lan, "GET", "/api/state", None, &[]).await;
    assert!(!lan_state.contains(CANARY));
    assert!(lan_state.contains("localhost"), "{lan_state}");
    let (_, local_state) = app.request("127.0.0.1", "GET", "/api/state", None, &[]).await;
    assert!(local_state.contains(CANARY));

    // A LAN client cannot decide a local-only approval even with its permission ID.
    let decision =
        app.request(&lan, "POST", &format!("/api/conversations/{conversation}/permissions/{permission_id}"), Some(json!({ "optionId": "once" })), &[]).await;
    assert_ne!(decision.0, 200);
    assert!(!decision.1.contains(CANARY));

    let id = conversation.clone();
    app.store
        .mutate(move |s| {
            s.conversation_mut(&id)?.run.status = grok_bot_model::RunStatus::Completed;
            Ok(())
        })
        .await
        .unwrap();
    let (status, mutation) = app.request(&lan, "PATCH", &format!("/api/bots/{bot}"), Some(json!({ "pinned": true })), &[]).await;
    assert_eq!(status, 200, "{mutation}");
    assert!(!mutation.contains(CANARY));

    let id = conversation.clone();
    app.store
        .mutate(move |s| {
            if let Some(permission) = s.conversation_mut(&id)?.run.permission.as_mut() {
                permission.local_only = Some(false);
            }
            Ok(())
        })
        .await
        .unwrap();
    let (_, file_approval) = app.request(&lan, "GET", "/api/state", None, &[]).await;
    assert!(file_approval.contains(CANARY), "ordinary approvals stay visible to LAN clients");
}
