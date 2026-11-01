//! Port of `tests/backend-api.test.ts`.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use common::{AppOptions, TestApp, raw, run};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn settings(workspace: &Path, base_url: &str) -> Value {
    json!({ "baseUrl": base_url, "model": "test-model", "workspace": workspace })
}

const BASE: &str = "http://127.0.0.1:12345/v1";

fn fixture(root: &Path, name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = root.join(name);
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("notes.txt"), "original\n").unwrap();
    (dir.join("data"), workspace)
}

async fn first_sse_event(port: u16, origin: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let request = format!("GET /api/events HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: {origin}\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut seen = Vec::new();
    let mut buffer = [0u8; 8192];
    while !String::from_utf8_lossy(&seen).contains("\n\n{") && !String::from_utf8_lossy(&seen).contains("}\n\n") {
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut buffer)).await.expect("sse data").unwrap();
        if read == 0 {
            break;
        }
        seen.extend_from_slice(&buffer[..read]);
    }
    String::from_utf8_lossy(&seen).into_owned()
}

#[test]
fn backend_api() {
    run("http-api", async |root| {
        let engine = root.join("engine");
        settings_are_private(&root, &engine).await;
        hostile_requests_are_rejected(&root, &engine).await;
        network_hosts(&root, &engine).await;
        workspace_files(&root, &engine).await;
        provider_test(&root, &engine).await;
        invalid_state_and_interrupted_runs(&root, &engine).await;
    });
}

async fn settings_are_private(root: &Path, engine: &Path) {
    let (data, workspace) = fixture(root, "settings");
    let app = TestApp::start(engine, data.clone()).await;
    let (_, initial) = app.api("GET", "/api/state", None).await;
    assert_eq!(initial["bots"].as_array().unwrap().len(), 1);
    assert_eq!(initial["bots"][0]["name"], "Grok");
    let key = "fixture-secret-not-for-the-browser";
    let mut body = settings(&workspace, BASE);
    body["apiKey"] = json!(key);
    let (status, text) = app.request("PUT", "/api/settings", Some(body.to_string()), &[]).await;
    assert_eq!(status, 200, "{text}");
    assert!(!text.contains(key));
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["settings"]["hasApiKey"], true);
    let disk: Value = serde_json::from_str(&std::fs::read_to_string(data.join("state.json")).unwrap()).unwrap();
    assert_eq!(disk["settings"]["apiKey"], key);
    assert_eq!(std::fs::metadata(data.join("state.json")).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(&data).unwrap().permissions().mode() & 0o777, 0o700);
    let first = first_sse_event(app.port, &app.origin).await;
    assert!(first.contains("event: state"), "{first}");
    assert!(!first.contains(key));
    let mut bad = settings(&workspace, "https://user:password@example.invalid/v1");
    assert_eq!(app.api("PUT", "/api/settings", Some(bad.clone())).await.0, 400);
    bad = settings(&workspace, BASE);
    bad["model"] = json!(" ");
    assert_eq!(app.api("PUT", "/api/settings", Some(bad)).await.0, 400);
    app.close().await;
    let restarted = TestApp::start(engine, data).await;
    let snapshot = restarted.store.snapshot();
    assert!(snapshot.settings.has_api_key);
    assert_eq!(snapshot.bots[0].id, initial["bots"][0]["id"].as_str().unwrap());
    restarted.close().await;
}

async fn hostile_requests_are_rejected(root: &Path, engine: &Path) {
    let (data, _) = fixture(root, "hostile");
    let app = TestApp::start(engine, data).await;
    let hostile = raw(app.port, "GET", "/api/state", &[("Host".into(), "attacker.invalid".into())], None).await;
    assert_eq!(hostile.0, 403);
    assert_eq!(app.request("GET", "/api/state", None, &[("Origin", "http://evil.invalid")]).await.0, 403);
    assert_eq!(app.request("POST", "/api/bots", Some(json!({"name": "Unsafe"}).to_string()), &[("X-Grok-Bot", "")]).await.0, 403);
    assert_eq!(app.request("POST", "/api/bots", Some(json!({"name": "Unsafe"}).to_string()), &[("Origin", "")]).await.0, 403);
    assert_eq!(app.request("POST", "/api/bots", Some("{".into()), &[]).await.0, 400);
    let huge = json!({ "name": "x".repeat(2 * 1024 * 1024) }).to_string();
    assert_eq!(app.request("POST", "/api/bots", Some(huge), &[]).await.0, 413);
    app.close().await;
}

async fn network_hosts(root: &Path, engine: &Path) {
    let (data, _) = fixture(root, "network");
    let app = TestApp::try_start(AppOptions {
        engine_root: engine,
        data_dir: data,
        allowed_hosts: vec!["192.168.10.154".into(), "devbox".into(), "Devbox.tailc6b81b.ts.net.".into()],
    })
    .await
    .unwrap();
    let port = app.port;
    let request = async |method: &str, route: &str, host: String, origin: Option<String>| {
        let mut headers = vec![("Host".to_owned(), host), ("Content-Type".into(), "application/json".into()), ("X-Grok-Bot".into(), "1".into())];
        if let Some(origin) = origin {
            headers.push(("Origin".into(), origin));
        }
        let body = (method == "POST").then(|| json!({"name": "Network bot"}).to_string());
        raw(port, method, route, &headers, body.as_deref()).await.0
    };
    let host = format!("192.168.10.154:{port}");
    assert_eq!(request("GET", "/api/state", host.clone(), None).await, 200);
    assert_eq!(request("POST", "/api/bots", host.clone(), Some(format!("http://{host}"))).await, 201);
    for name in ["devbox", "DEVBOX", "devbox.", "devbox.tailc6b81b.ts.net", "DEVBOX.tailc6b81b.ts.net."] {
        let requested = format!("{name}:{port}");
        let own_origin = format!("http://{}:{port}", name.to_ascii_lowercase());
        assert_eq!(request("GET", "/", requested.clone(), None).await, 404, "{name}");
        assert_eq!(request("GET", "/api/state", requested.clone(), None).await, 200, "{name}");
        assert_eq!(request("POST", "/api/bots", requested.clone(), Some(own_origin)).await, 201, "{name}");
        assert_eq!(request("POST", "/api/bots", requested, Some("http://attacker.invalid".into())).await, 403, "{name}");
    }
    assert_eq!(request("POST", "/api/bots", host.clone(), Some("http://attacker.invalid".into())).await, 403);
    assert_eq!(request("GET", "/api/state", format!("attacker.invalid:{port}"), None).await, 403);
    assert_eq!(request("GET", "/api/state", format!("192.168.10.155:{port}"), None).await, 403);
    assert_eq!(request("GET", "/api/state", format!("192.168.10.154:{}", port + 1), None).await, 403);
    assert_eq!(request("GET", "/api/state", format!("192.168.10.154:{port}@attacker.invalid"), None).await, 403);
    app.close().await;
}

async fn workspace_files(root: &Path, engine: &Path) {
    use std::os::unix::fs::symlink;
    let (data, workspace) = fixture(root, "files");
    let dir = data.parent().unwrap().to_path_buf();
    let app = TestApp::start(engine, data).await;
    assert_eq!(app.api("GET", "/api/files", None).await.0, 400);
    assert_eq!(app.api("PUT", "/api/settings", Some(settings(&workspace, BASE))).await.0, 200);
    std::fs::create_dir(workspace.join("nested")).unwrap();
    std::fs::write(dir.join("outside.txt"), "outside stays unchanged\n").unwrap();
    symlink(dir.join("outside.txt"), workspace.join("linked.txt")).unwrap();
    symlink(&dir, workspace.join("parent-link")).unwrap();
    std::fs::hard_link(dir.join("outside.txt"), workspace.join("hard.txt")).unwrap();
    std::fs::write(workspace.join("binary.bin"), [0u8, 1, 2]).unwrap();
    std::fs::write(workspace.join("large.txt"), "x".repeat(1024 * 1024 + 1)).unwrap();
    let (_, listing) = app.api("GET", "/api/files?path=", None).await;
    assert_eq!(listing["entries"][0]["name"], "nested", "{listing}");
    assert!(!listing["entries"].as_array().unwrap().iter().any(|e| e["name"] == "linked.txt"));
    let (_, document) = app.api("GET", "/api/file?path=notes.txt", None).await;
    assert_eq!(document["content"], "original\n");
    let hash = document["hash"].as_str().unwrap().to_owned();
    assert!(hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()));
    let save = json!({ "path": "notes.txt", "content": "saved from browser\n", "hash": hash });
    assert_eq!(app.api("PUT", "/api/file", Some(save)).await.0, 200);
    assert_eq!(std::fs::read_to_string(workspace.join("notes.txt")).unwrap(), "saved from browser\n");
    let stale = json!({ "path": "notes.txt", "content": "stale write", "hash": hash });
    assert_eq!(app.api("PUT", "/api/file", Some(stale)).await.0, 409);
    for filename in ["../outside.txt", "/etc/passwd", "nested/../../outside.txt", "parent-link/outside.txt", "linked.txt", "hard.txt", "nested/../notes.txt"] {
        let encoded: String = filename.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
        assert_eq!(app.api("GET", &format!("/api/file?path={encoded}"), None).await.0, 403, "{filename}");
        assert_eq!(app.api("PUT", "/api/file", Some(json!({ "path": filename, "content": "blocked", "hash": "" }))).await.0, 403, "{filename}");
    }
    assert_eq!(app.api("GET", "/api/file?path=binary.bin", None).await.0, 415);
    assert_eq!(app.api("GET", "/api/file?path=large.txt", None).await.0, 413);
    assert_eq!(app.api("GET", "/api/file?path=missing.txt", None).await.0, 404);
    assert_eq!(app.api("PUT", "/api/file", Some(json!({ "path": "new.txt", "content": "new file\n", "hash": "" }))).await.0, 200);
    assert_eq!(std::fs::read_to_string(dir.join("outside.txt")).unwrap(), "outside stays unchanged\n");
    let alias = dir.join("workspace-alias");
    symlink(&workspace, &alias).unwrap();
    assert_eq!(app.api("PUT", "/api/settings", Some(settings(&alias, BASE))).await.0, 403);
    app.close().await;
}

async fn provider_test(root: &Path, engine: &Path) {
    use axum::http::StatusCode;
    use std::sync::atomic::{AtomicU16, Ordering};
    let (data, workspace) = fixture(root, "provider");
    let status = std::sync::Arc::new(AtomicU16::new(200));
    let shared = status.clone();
    let provider = axum::Router::new().route(
        "/v1/models",
        axum::routing::get(move || {
            let code = shared.load(Ordering::SeqCst);
            async move {
                let body = if code == 200 { json!({"data": [{"id": "test-model"}, {"id": "other"}]}) } else { json!({"error": "super-secret-provider-body"}) };
                (StatusCode::from_u16(code).unwrap(), axum::Json(body))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, provider).await });
    let app = TestApp::start(engine, data).await;
    assert_eq!(app.api("PUT", "/api/settings", Some(settings(&workspace, &base))).await.0, 200);
    let (_, check) = app.api("POST", "/api/provider/test", Some(json!({}))).await;
    assert_eq!(check["ok"], true, "{check}");
    assert_eq!(check["models"], json!(["other", "test-model"]));
    status.store(401, Ordering::SeqCst);
    let (_, failed) = app.api("POST", "/api/provider/test", Some(json!({}))).await;
    assert_eq!(failed["ok"], false);
    assert!(failed["error"].as_str().unwrap().contains("401"));
    assert!(!failed.to_string().contains("super-secret-provider-body"));
    app.close().await;
    server.abort();
}

async fn invalid_state_and_interrupted_runs(root: &Path, engine: &Path) {
    let (data, _) = fixture(root, "invalid");
    let app = TestApp::start(engine, data.clone()).await;
    let bot = app.store.snapshot().bots[0].id.clone();
    let conversation = app.store.mutate(|s| s.add_conversation(&bot)).await.unwrap();
    app.store
        .mutate(|s| {
            let c = s.conversation_mut(&conversation.id)?;
            c.run.id = "interrupted-run".into();
            c.run.status = grok_bot_model::RunStatus::Running;
            c.run.started_at = Some(grok_bot_model::iso(chrono::Utc::now()));
            Ok(())
        })
        .await
        .unwrap();
    app.close().await;
    let restarted = TestApp::start(engine, data.clone()).await;
    let run = restarted.store.read(|s| s.conversation(&conversation.id).unwrap().run.clone());
    assert_eq!(run.status, grok_bot_model::RunStatus::Interrupted);
    assert!(run.error.unwrap().contains("server stopped"));
    restarted.close().await;
    std::fs::write(data.join("state.json"), "{invalid").unwrap();
    let error = TestApp::try_start(AppOptions { engine_root: engine, data_dir: data.clone(), allowed_hosts: vec![] }).await.err().expect("invalid state rejected");
    assert!(format!("{error:#}").contains("existing state.json was not replaced"), "{error:#}");
    assert_eq!(std::fs::read_to_string(data.join("state.json")).unwrap(), "{invalid");
}
