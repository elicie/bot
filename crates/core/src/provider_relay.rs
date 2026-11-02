//! Loopback relay between the in-process engine and the configured OpenAI-compatible provider.
//!
//! Some providers (OpenRouter-style gateways) answer an image or chart request with generated
//! images in an `images` array on the assistant message or stream delta and no `content`. Grok
//! Build reads only `content`, `reasoning_content`, and `tool_calls`, so such a reply arrives as an
//! empty response and the turn fails. The engine therefore talks to this relay instead of the
//! provider: requests are forwarded unchanged (with the real credential), and every `images` entry
//! in a response is stored (`grok_bot_store::generated_images`) and replaced by a Markdown image
//! reference appended to `content`. Everything else streams through byte for byte.
//!
//! The relay listens on `127.0.0.1` on its own random port, separate from the web server, and
//! accepts only the per-process bearer token it gives the engine. The provider API key is read
//! from the store on each request and is never written to the engine configuration.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderName, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use futures::StreamExt;
use grok_bot_store::Store;
use grok_bot_store::generated_images::{ROUTE_PREFIX, save_generated_image};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// Largest request the engine may send (conversation history with inline image attachments).
const MAX_REQUEST_BYTES: usize = 256 * 1024 * 1024;
/// Largest non-streamed response or single SSE line held for rewriting.
const MAX_BUFFERED_BYTES: usize = 128 * 1024 * 1024;
/// Marker that a line or body may carry generated images; anything else passes through untouched.
const IMAGES_MARKER: &[u8] = b"\"images\"";

/// Where the engine reaches the relay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayEndpoint {
    /// OpenAI-compatible base URL (`http://127.0.0.1:<port>/v1`).
    pub base_url: String,
    /// Bearer token the engine sends as its API key.
    pub token: String,
}

struct Relay {
    store: Arc<Store>,
    data_dir: PathBuf,
    token_digest: [u8; 32],
    client: reqwest::Client,
}

/// Bind the relay on a random loopback port and serve it for the life of the Tokio runtime.
pub async fn start(store: Arc<Store>) -> anyhow::Result<RelayEndpoint> {
    let token = super::runtime::random_token();
    let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(30)).build()?;
    let relay = Arc::new(Relay { data_dir: store.data_dir().to_path_buf(), store, token_digest: digest(&format!("Bearer {token}")), client });
    let router = Router::new().route("/v1/{*rest}", any(forward)).layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES)).with_state(relay);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok(RelayEndpoint { base_url: format!("http://127.0.0.1:{port}/v1"), token })
}

fn digest(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

/// OpenAI-style error body the engine reports as the turn error.
fn relay_error(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({ "error": { "message": message, "type": "relay_error" } }))).into_response()
}

/// Request headers not forwarded: hop-by-hop, framing, the engine's relay token, and encoding
/// negotiation (responses must stay uncompressed to be rewritten).
fn skipped_request_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host" | "authorization" | "content-length" | "connection" | "keep-alive" | "transfer-encoding" | "te" | "upgrade" | "proxy-authorization" | "proxy-connection" | "accept-encoding"
    )
}

fn skipped_response_header(name: &HeaderName) -> bool {
    matches!(name.as_str(), "content-length" | "connection" | "keep-alive" | "transfer-encoding" | "content-encoding")
}

async fn forward(State(relay): State<Arc<Relay>>, method: Method, Path(rest): Path<String>, RawQuery(query): RawQuery, headers: HeaderMap, body: Bytes) -> Response {
    let presented = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok()).unwrap_or_default();
    if digest(presented) != relay.token_digest {
        return relay_error(StatusCode::UNAUTHORIZED, "Invalid relay token.");
    }
    let settings = relay.store.read(|state| state.settings().clone());
    if settings.base_url.is_empty() {
        return relay_error(StatusCode::BAD_GATEWAY, "No provider is configured.");
    }
    let mut url = format!("{}/{rest}", settings.base_url.trim_end_matches('/'));
    if let Some(query) = query {
        url.push('?');
        url.push_str(&query);
    }
    let mut request = relay.client.request(method, &url).body(body).header(header::ACCEPT_ENCODING, "identity");
    for (name, value) in &headers {
        if !skipped_request_header(name) {
            request = request.header(name, value);
        }
    }
    if !settings.api_key.is_empty() {
        request = request.bearer_auth(&settings.api_key);
    }
    let upstream = match request.send().await {
        Ok(response) => response,
        Err(error) => return relay_error(StatusCode::BAD_GATEWAY, &format!("Could not reach the provider: {}", without_url(&error))),
    };
    let status = upstream.status();
    let mut response_headers = HeaderMap::new();
    for (name, value) in upstream.headers() {
        if !skipped_response_header(name) {
            response_headers.append(name, value.clone());
        }
    }
    let content_type = upstream.headers().get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()).unwrap_or_default().to_ascii_lowercase();
    let body = if content_type.starts_with("text/event-stream") {
        Body::from_stream(rewrite_stream(upstream, relay.data_dir.clone()))
    } else if status.is_success() && content_type.contains("json") {
        match read_capped(upstream).await {
            Ok(bytes) => Body::from(rewrite_json_body(bytes, relay.data_dir.clone()).await),
            Err(message) => return relay_error(StatusCode::BAD_GATEWAY, &message),
        }
    } else {
        Body::from_stream(upstream.bytes_stream())
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    response
}

/// Transport error text without the request URL (it may carry a query string).
fn without_url(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    if let Some(url) = error.url() {
        text = text.replace(url.as_str(), "<provider>");
    }
    text
}

async fn read_capped(upstream: reqwest::Response) -> Result<Bytes, String> {
    let mut stream = upstream.bytes_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("Provider response failed: {}", without_url(&error)))?;
        if out.len() + chunk.len() > MAX_BUFFERED_BYTES {
            return Err("Provider response is too large.".into());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out.into())
}

async fn rewrite_json_body(bytes: Bytes, data_dir: PathBuf) -> Bytes {
    if !contains(&bytes, IMAGES_MARKER) {
        return bytes;
    }
    tokio::task::spawn_blocking(move || {
        let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else { return bytes };
        if rewrite_completion(&mut value, &data_dir) { serde_json::to_vec(&value).map(Bytes::from).unwrap_or(bytes) } else { bytes }
    })
    .await
    .unwrap_or_default()
}

/// Split an SSE byte stream into lines and rewrite the `data:` lines that carry images.
fn rewrite_stream(upstream: reqwest::Response, data_dir: PathBuf) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    struct Lines {
        pending: Vec<u8>,
        scanned: usize,
    }
    impl Lines {
        /// Complete lines (each with its `\n`) now available.
        fn push(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, std::io::Error> {
            self.pending.extend_from_slice(chunk);
            let mut lines = Vec::new();
            while let Some(offset) = self.pending[self.scanned..].iter().position(|byte| *byte == b'\n') {
                let end = self.scanned + offset + 1;
                lines.push(self.pending.drain(..end).collect());
                self.scanned = 0;
            }
            self.scanned = self.pending.len();
            if self.pending.len() > MAX_BUFFERED_BYTES {
                return Err(std::io::Error::other("provider stream line is too large"));
            }
            Ok(lines)
        }
    }
    let state = (upstream.bytes_stream(), Lines { pending: Vec::new(), scanned: 0 }, data_dir, false);
    futures::stream::unfold(state, |(mut stream, mut lines, data_dir, done)| async move {
        if done {
            return None;
        }
        let (complete, finished) = match stream.next().await {
            Some(Ok(chunk)) => match lines.push(&chunk) {
                Ok(complete) => (complete, false),
                Err(error) => return Some((Err(error), (stream, lines, data_dir, true))),
            },
            Some(Err(error)) => return Some((Err(std::io::Error::other(without_url(&error))), (stream, lines, data_dir, true))),
            None => (vec![std::mem::take(&mut lines.pending)], true),
        };
        let out = if complete.iter().any(|line| contains(line, IMAGES_MARKER)) {
            let directory = data_dir.clone();
            tokio::task::spawn_blocking(move || complete.into_iter().flat_map(|line| rewrite_sse_line(line, &directory)).collect::<Vec<u8>>()).await.unwrap_or_default()
        } else {
            complete.concat()
        };
        Some((Ok(Bytes::from(out)), (stream, lines, data_dir, finished)))
    })
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

/// Rewrite one SSE line (`data: {...}` plus its line ending); other lines are returned unchanged.
fn rewrite_sse_line(line: Vec<u8>, data_dir: &std::path::Path) -> Vec<u8> {
    let Some(payload) = line.strip_prefix(b"data:") else { return line };
    let body_end = payload.len() - payload.iter().rev().take_while(|byte| matches!(byte, b'\n' | b'\r')).count();
    let (json_bytes, ending) = payload.split_at(body_end);
    let Ok(mut value) = serde_json::from_slice::<Value>(json_bytes.trim_ascii_start()) else { return line };
    if !rewrite_completion(&mut value, data_dir) {
        return line;
    }
    let mut out = b"data: ".to_vec();
    out.extend(serde_json::to_vec(&value).unwrap_or_default());
    out.extend_from_slice(ending);
    out
}

/// Move `images` of every choice's `delta` or `message` into its `content`. Returns whether
/// anything changed.
pub(crate) fn rewrite_completion(value: &mut Value, data_dir: &std::path::Path) -> bool {
    let Some(choices) = value.get_mut("choices").and_then(Value::as_array_mut) else { return false };
    let mut changed = false;
    for choice in choices {
        for key in ["delta", "message"] {
            if let Some(target) = choice.get_mut(key).and_then(Value::as_object_mut) {
                changed |= move_images_into_content(target, data_dir);
            }
        }
    }
    changed
}

fn move_images_into_content(target: &mut Map<String, Value>, data_dir: &std::path::Path) -> bool {
    let Some(images) = target.remove("images") else { return false };
    let markdown: String = images.as_array().map(|list| list.iter().map(|image| image_markdown(image, data_dir)).collect()).unwrap_or_default();
    if markdown.is_empty() {
        return true;
    }
    let content = match target.get("content") {
        Some(Value::String(text)) => format!("{text}{markdown}"),
        _ => markdown,
    };
    target.insert("content".into(), Value::String(content));
    true
}

/// Markdown for one `images` entry (`{ "type": "image_url", "image_url": { "url" } }`).
fn image_markdown(image: &Value, data_dir: &std::path::Path) -> String {
    let url = image.pointer("/image_url/url").or_else(|| image.get("url")).or_else(|| image.get("image_url")).and_then(Value::as_str).unwrap_or_default();
    if let Some(data) = url.strip_prefix("data:") {
        let stored = data
            .split_once(";base64,")
            .filter(|(mime, _)| mime.starts_with("image/"))
            .ok_or_else(|| "unsupported image encoding".to_owned())
            .and_then(|(_, encoded)| decode_base64(encoded))
            .and_then(|bytes| save_generated_image(data_dir, &bytes));
        return match stored {
            Ok(id) => format!("\n\n![생성된 이미지]({ROUTE_PREFIX}{id})\n\n"),
            Err(reason) => format!("\n\n(생성된 이미지를 표시할 수 없습니다: {reason})\n\n"),
        };
    }
    if (url.starts_with("https://") || url.starts_with("http://")) && !url.contains([')', ' ', '\n']) {
        return format!("\n\n[생성된 이미지 열기]({url})\n\n");
    }
    "\n\n(생성된 이미지를 표시할 수 없습니다)\n\n".to_owned()
}

fn decode_base64(encoded: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let compact: String = encoded.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    base64::engine::general_purpose::STANDARD.decode(compact).map_err(|_| "invalid base64 image".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==";

    #[test]
    fn stream_delta_images_become_markdown_content() {
        let root = tempfile::tempdir().unwrap();
        let line = format!("data: {}\n\n", json!({"choices": [{"index": 0, "delta": {"role": "assistant", "images": [{"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{PNG_B64}")}}]}}]}));
        let out = String::from_utf8(rewrite_sse_line(line.into_bytes(), root.path())).unwrap();
        assert!(out.ends_with("\n\n"), "{out}");
        let value: Value = serde_json::from_str(out.trim().strip_prefix("data: ").unwrap()).unwrap();
        let delta = &value["choices"][0]["delta"];
        assert!(delta.get("images").is_none());
        let content = delta["content"].as_str().unwrap();
        let id = content.split(ROUTE_PREFIX).nth(1).unwrap().split(')').next().unwrap();
        assert!(grok_bot_store::generated_images::read_generated_image(root.path(), id).is_ok());
    }

    #[test]
    fn message_images_append_and_bad_images_explain() {
        let root = tempfile::tempdir().unwrap();
        let mut value = json!({"choices": [{"message": {"role": "assistant", "content": "Here:", "images": [
            {"image_url": {"url": "data:image/png;base64,!!!"}},
            {"image_url": {"url": "data:text/html;base64,PGI+"}},
            {"image_url": {"url": "https://example.com/a.png"}},
        ]}}]});
        assert!(rewrite_completion(&mut value, root.path()));
        let content = value["choices"][0]["message"]["content"].as_str().unwrap();
        assert!(content.starts_with("Here:"));
        assert!(content.contains("invalid base64 image"));
        assert!(content.contains("unsupported image encoding"));
        assert!(content.contains("[생성된 이미지 열기](https://example.com/a.png)"));
    }

    #[test]
    fn other_lines_pass_through_unchanged() {
        let root = tempfile::tempdir().unwrap();
        for line in ["data: [DONE]\n", ": keep-alive\n", "data: {\"choices\":[{\"delta\":{\"content\":\"\\\"images\\\"\"}}]}\r\n"] {
            assert_eq!(rewrite_sse_line(line.as_bytes().to_vec(), root.path()), line.as_bytes());
        }
    }
}
