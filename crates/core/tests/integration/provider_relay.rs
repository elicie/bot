//! Provider relay against the real in-process Grok Build engine and a mock OpenAI-compatible
//! provider that answers with generated images (`delta.images`, no `content`), as some gateways do
//! for chart and drawing requests.
//!
//! Proves that the turn completes with the image linked in the reply instead of failing as an empty
//! response, that the image is served from `/api/generated-images/:id`, that the provider receives
//! the real API key while the engine configuration holds only the relay token, and that unreferenced
//! images are swept when their conversation is deleted.

use crate::common;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use common::{TestApp, run};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const API_KEY: &str = "sk-relay-test-0123456789";
/// A 1×1 PNG.
const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==";

#[derive(Clone, Default)]
struct Provider {
    authorizations: Arc<Mutex<Vec<String>>>,
}

async fn completions(State(provider): State<Provider>, headers: HeaderMap, Json(request): Json<Value>) -> Response {
    provider.authorizations.lock().unwrap().push(headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned());
    let model = request["model"].as_str().unwrap_or_default().to_owned();
    let main = request["tools"].as_array().into_iter().flatten().any(|tool| tool["function"]["name"] == "search_tool");
    let chunk = |delta: Value, finish: Value| json!({"id": "chatcmpl-local", "object": "chat.completion.chunk", "created": 1, "model": model, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]});
    let mut last = chunk(json!({}), json!("stop"));
    last["usage"] = json!({"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15});
    let first = if main {
        // Image only, in the same shape the real gateway sent: role plus `images`, no `content`.
        chunk(
            json!({"role": "assistant", "images": [{"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{PNG_BASE64}")}, "index": 0}]}),
            Value::Null,
        )
    } else {
        chunk(json!({"role": "assistant", "content": "Chart title"}), Value::Null)
    };
    let body = [first, last].iter().map(|value| format!("data: {value}\n\n")).collect::<String>() + "data: [DONE]\n\n";
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

async fn start_provider(provider: Provider) -> String {
    let router = Router::new().route("/v1/chat/completions", post(completions)).with_state(provider);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("{origin}/v1")
}

async fn fetch(app: &TestApp, route: &str) -> (String, Vec<u8>) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", app.port)).await.unwrap();
    let request = format!("GET {route} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", app.port);
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let split = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    (String::from_utf8_lossy(&response[..split]).to_ascii_lowercase(), response[split + 4..].to_vec())
}

async fn settled(app: &TestApp, conversation: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (_, state) = app.api("GET", "/api/state", None).await;
        let found = state["conversations"].as_array().unwrap().iter().find(|c| c["id"] == conversation).cloned().unwrap();
        if ["completed", "error", "cancelled", "interrupted"].contains(&found["run"]["status"].as_str().unwrap()) {
            return found;
        }
        assert!(Instant::now() < deadline, "run did not settle: {found}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn files_containing(root: &Path, needle: &str) -> Vec<String> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else { return found };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_containing(&path, needle));
        } else if std::fs::read(&path).is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle.as_bytes())) {
            found.push(path.display().to_string());
        }
    }
    found
}

#[test]
fn provider_images_become_linked_generated_images() {
    run("provider-relay", async |root| {
        let engine = root.join("engine");
        scenario(&root, &engine).await;
    });
}

async fn scenario(root: &Path, engine: &Path) {
    let provider = Provider::default();
    let base_url = start_provider(provider.clone()).await;
    let data = root.join("data");
    let app = TestApp::start(engine, data.clone()).await;
    let (status, body) =
        app.api("PUT", "/api/settings", Some(json!({"baseUrl": base_url, "model": "local-test-model", "apiKey": API_KEY, "workspace": ""}))).await;
    assert_eq!(status, 200, "{body}");
    let (_, state) = app.api("GET", "/api/state", None).await;
    let bot = state["bots"][0]["id"].as_str().unwrap().to_owned();
    let (_, created) = app.api("POST", "/api/conversations", Some(json!({"botId": bot}))).await;
    let conversation = created["conversation"]["id"].as_str().unwrap().to_owned();

    let (status, _) = app.api("POST", &format!("/api/conversations/{conversation}/messages"), Some(json!({"text": "Draw a bar chart."}))).await;
    assert_eq!(status, 202);
    let done = settled(&app, &conversation).await;
    assert_eq!(done["run"]["status"], "completed", "{done}");
    let text = done["messages"].as_array().unwrap().last().unwrap()["text"].as_str().unwrap().to_owned();
    let id =
        text.split("/api/generated-images/").nth(1).and_then(|rest| rest.get(..36)).unwrap_or_else(|| panic!("no generated image link in {text:?}")).to_owned();
    assert!(text.contains(&format!("![생성된 이미지](/api/generated-images/{id})")), "{text}");

    let (head, bytes) = fetch(&app, &format!("/api/generated-images/{id}")).await;
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert!(head.contains("content-type: image/png"), "{head}");
    assert!(head.contains("content-security-policy: default-src 'none'"), "{head}");
    assert!(bytes.starts_with(b"\x89PNG"));
    let (head, _) = fetch(&app, "/api/generated-images/00000000-0000-4000-8000-000000000000").await;
    assert!(head.starts_with("http/1.1 404"), "{head}");
    let (head, _) = fetch(&app, "/api/generated-images/..%2Fstate.json").await;
    assert!(head.starts_with("http/1.1 404"), "{head}");

    // The provider saw the real key on every request; the engine's files never hold it.
    let authorizations = provider.authorizations.lock().unwrap().clone();
    assert!(!authorizations.is_empty());
    assert!(authorizations.iter().all(|value| value == &format!("Bearer {API_KEY}")), "{authorizations:?}");
    assert_eq!(files_containing(engine, API_KEY), Vec::<String>::new());

    // Deleting the conversation's bot leaves the image unreferenced; the sweep keeps it during the
    // grace period (a reply may still be streaming), so it is still served right afterwards.
    let (status, _) = app.api("POST", "/api/bots", Some(json!({"name": "Spare"}))).await;
    assert_eq!(status, 201);
    let (status, body) = app.api("DELETE", &format!("/api/bots/{bot}"), Some(json!({}))).await;
    assert_eq!(status, 200, "{body}");
    assert!(data.join("generated-images").join(&id).exists());
    let removed = grok_bot_store::generated_images::prune_generated_images(&data, &Default::default(), Duration::ZERO);
    assert_eq!(removed, 1);
    let (head, _) = fetch(&app, &format!("/api/generated-images/{id}")).await;
    assert!(head.starts_with("http/1.1 404"), "{head}");

    app.close().await;
}
