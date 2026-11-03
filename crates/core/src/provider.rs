//! Bounded OpenAI-compatible provider check (`GET {baseUrl}/models`).

use std::time::Duration;

use grok_bot_model::ProviderCheck;
use reqwest::StatusCode;
use reqwest::header::AUTHORIZATION;

const TIMEOUT: Duration = Duration::from_secs(6);
const MAX_PROVIDER_BYTES: usize = 1024 * 1024;
const MAX_MODELS: usize = 10_000;

const UNREACHABLE: &str = "Provider could not be reached within 6 seconds. Check its URL and that it is running.";
const INCOMPATIBLE: &str = "Provider did not return an OpenAI-compatible model list.";
const TOO_LARGE: &str = "Provider model list exceeded 1 MiB.";
const MISSING_MODEL: &str = "The configured model is not in the provider model list.";

/// 6 s timeout, no redirects, 1 MiB body cap, `{ data: [{ id }] }` (max 10000), sorted unique ids.
/// When `model` is non-empty and missing from the list, `ok` is false.
///
/// `base_url` must already be canonical (no trailing slash); the store owns that validation.
pub async fn check_provider(base_url: &str, api_key: &str, model: &str) -> ProviderCheck {
    check_provider_within(base_url, api_key, model, TIMEOUT).await
}

/// Why the check threw in the Node version: a `ZodError`/`SyntaxError` versus anything else.
enum Failure {
    Unreachable,
    Incompatible,
}

fn failed(error: impl Into<String>) -> ProviderCheck {
    ProviderCheck { ok: false, models: Vec::new(), error: Some(error.into()) }
}

async fn check_provider_within(base_url: &str, api_key: &str, model: &str, timeout: Duration) -> ProviderCheck {
    match fetch_models(base_url, api_key, model, timeout).await {
        Ok(check) => check,
        Err(Failure::Incompatible) => failed(INCOMPATIBLE),
        Err(Failure::Unreachable) => failed(UNREACHABLE),
    }
}

async fn fetch_models(base_url: &str, api_key: &str, model: &str, timeout: Duration) -> Result<ProviderCheck, Failure> {
    // `fetch` with `redirect: 'error'` rejects on any redirect; it also ignores proxy variables.
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::custom(|attempt| attempt.error("redirects are not allowed")))
        .no_proxy()
        .build()
        .map_err(|_| Failure::Unreachable)?;
    let mut request = client.get(format!("{base_url}/models"));
    if !api_key.is_empty() {
        request = request.header(AUTHORIZATION, format!("Bearer {api_key}"));
    }
    let mut response = request.send().await.map_err(|_| Failure::Unreachable)?;
    let status = response.status();
    if !status.is_success() {
        return Ok(failed(format!("Provider returned HTTP {}. Check its URL and credentials.", status.as_u16())));
    }
    // A null-body status has no `response.body` in fetch, which the Node code reports as unreachable.
    if status == StatusCode::NO_CONTENT || status == StatusCode::RESET_CONTENT {
        return Err(Failure::Unreachable);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Failure::Unreachable)? {
        if body.len() + chunk.len() > MAX_PROVIDER_BYTES {
            return Ok(failed(TOO_LARGE));
        }
        body.extend_from_slice(&chunk);
    }
    let mut models = model_ids(&body).ok_or(Failure::Incompatible)?;
    // JavaScript's default sort compares UTF-16 code units.
    models.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    models.dedup();
    if !model.is_empty() && !models.iter().any(|id| id == model) {
        return Ok(ProviderCheck { ok: false, models, error: Some(MISSING_MODEL.into()) });
    }
    Ok(ProviderCheck { ok: true, models, error: None })
}

/// `z.object({ data: z.array(z.object({ id: z.string() })).max(10000) })` over
/// `JSON.parse(Buffer.toString('utf8'))`.
fn model_ids(body: &[u8]) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(body)).ok()?;
    let data = value.as_object()?.get("data")?.as_array()?;
    if data.len() > MAX_MODELS {
        return None;
    }
    data.iter().map(|item| item.as_object()?.get("id")?.as_str().map(str::to_owned)).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    #[derive(Clone)]
    struct Reply {
        bytes: Vec<u8>,
        /// Keep the connection open without finishing the response.
        stall: bool,
    }

    fn http(status: &str, headers: &str, body: &[u8]) -> Reply {
        let mut bytes = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n", body.len()).into_bytes();
        bytes.extend_from_slice(body);
        Reply { bytes, stall: false }
    }

    fn json(body: &str) -> Reply {
        http("200 OK", "Content-Type: application/json\r\n", body.as_bytes())
    }

    /// Body without a length: delimited by connection close, delivered in many reads.
    fn streamed(body: &[u8]) -> Reply {
        let mut bytes = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n".to_vec();
        bytes.extend_from_slice(body);
        Reply { bytes, stall: false }
    }

    async fn provider(reply: Reply) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (seen, reply) = (seen.clone(), reply.clone());
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buffer = [0u8; 4096];
                    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                        match socket.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => head.extend_from_slice(&buffer[..read]),
                        }
                    }
                    seen.lock().unwrap().push(String::from_utf8_lossy(&head).to_lowercase());
                    let _ = socket.write_all(&reply.bytes).await;
                    if reply.stall {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });
        (format!("http://{address}/v1"), requests)
    }

    fn check(ok: bool, models: &[&str], error: Option<&str>) -> ProviderCheck {
        ProviderCheck { ok, models: models.iter().map(|id| id.to_string()).collect(), error: error.map(str::to_owned) }
    }

    #[tokio::test]
    async fn lists_sorted_unique_models_and_sends_the_bearer_key() {
        let (url, requests) = provider(json(r#"{"object":"list","data":[{"id":"test-model","owned_by":"x"},{"id":"other"},{"id":"test-model"}]}"#)).await;
        assert_eq!(check_provider(&url, "fixture-key", "test-model").await, check(true, &["other", "test-model"], None));
        let requests = requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("get /v1/models http/1.1\r\n"), "{}", requests[0]);
        assert!(requests[0].contains("\r\nauthorization: bearer fixture-key\r\n"), "{}", requests[0]);
    }

    #[tokio::test]
    async fn omits_authorization_without_a_key_and_allows_an_empty_model() {
        let (url, requests) = provider(json(r#"{"data":[]}"#)).await;
        assert_eq!(check_provider(&url, "", "").await, check(true, &[], None));
        assert!(!requests.lock().unwrap()[0].contains("authorization"));
    }

    #[tokio::test]
    async fn reports_http_errors_without_echoing_the_body() {
        let (url, _) = provider(http("401 Unauthorized", "", br#"{"error":"super-secret-provider-body"}"#)).await;
        let result = check_provider(&url, "key", "test-model").await;
        assert_eq!(result, check(false, &[], Some("Provider returned HTTP 401. Check its URL and credentials.")));
        let (url, _) = provider(http("500 Internal Server Error", "", b"")).await;
        assert_eq!(check_provider(&url, "", "").await.error.as_deref(), Some("Provider returned HTTP 500. Check its URL and credentials."));
    }

    #[tokio::test]
    async fn rejects_redirects_without_following_them() {
        let (url, requests) = provider(http("302 Found", "Location: /v1/elsewhere\r\n", b"")).await;
        assert_eq!(check_provider(&url, "key", "").await, check(false, &[], Some(UNREACHABLE)));
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn caps_the_model_list_at_one_mebibyte() {
        let prefix = br#"{"data":[{"id":"m"}]}"#;
        let mut exact = prefix.to_vec();
        exact.resize(MAX_PROVIDER_BYTES, b' ');
        let (url, _) = provider(streamed(&exact)).await;
        assert_eq!(check_provider(&url, "", "m").await, check(true, &["m"], None));
        exact.push(b' ');
        let (url, _) = provider(streamed(&exact)).await;
        assert_eq!(check_provider(&url, "", "m").await, check(false, &[], Some(TOO_LARGE)));
        let (url, _) = provider(http("200 OK", "", &exact)).await;
        assert_eq!(check_provider(&url, "", "m").await, check(false, &[], Some(TOO_LARGE)));
    }

    #[tokio::test]
    async fn rejects_invalid_json_and_unexpected_shapes() {
        let too_many = format!(r#"{{"data":[{}]}}"#, vec![r#"{"id":"m"}"#; MAX_MODELS + 1].join(","));
        for body in [
            "{invalid",
            "",
            "[]",
            "null",
            r#"{"models":[]}"#,
            r#"{"data":{}}"#,
            r#"{"data":[{"id":1}]}"#,
            r#"{"data":[{"name":"m"}]}"#,
            r#"{"data":[null]}"#,
            r#"{"data":[["m"]]}"#,
            &too_many,
        ] {
            let (url, _) = provider(json(body)).await;
            assert_eq!(check_provider(&url, "", "").await, check(false, &[], Some(INCOMPATIBLE)), "{body:.40}");
        }
        let exactly_max = format!(r#"{{"data":[{}]}}"#, vec![r#"{"id":"m"}"#; MAX_MODELS].join(","));
        let (url, _) = provider(json(&exactly_max)).await;
        assert_eq!(check_provider(&url, "", "").await, check(true, &["m"], None));
    }

    #[tokio::test]
    async fn reports_a_configured_model_missing_from_the_list() {
        let (url, _) = provider(json(r#"{"data":[{"id":"b"},{"id":"a"}]}"#)).await;
        assert_eq!(check_provider(&url, "", "missing").await, check(false, &["a", "b"], Some(MISSING_MODEL)));
    }

    #[tokio::test]
    async fn sorts_ids_by_utf16_code_units_like_javascript() {
        let (url, _) = provider(json("{\"data\":[{\"id\":\"\u{ff61}\"},{\"id\":\"\u{1f600}\"},{\"id\":\"B\"},{\"id\":\"a\"}]}")).await;
        assert_eq!(check_provider(&url, "", "").await.models, ["B", "a", "\u{1f600}", "\u{ff61}"]);
    }

    #[tokio::test]
    async fn times_out_on_silent_or_stalled_providers() {
        let silent = Reply { bytes: Vec::new(), stall: true };
        let (url, _) = provider(silent).await;
        let started = std::time::Instant::now();
        let timeout = Duration::from_millis(300);
        assert_eq!(check_provider_within(&url, "", "", timeout).await, check(false, &[], Some(UNREACHABLE)));
        assert!(started.elapsed() < Duration::from_secs(5));

        let mut stalled = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{\"data\":".to_vec();
        stalled.extend_from_slice(b"[");
        let (url, _) = provider(Reply { bytes: stalled, stall: true }).await;
        assert_eq!(check_provider_within(&url, "", "", timeout).await, check(false, &[], Some(UNREACHABLE)));
        assert_eq!(TIMEOUT, Duration::from_secs(6));
    }

    #[tokio::test]
    async fn reports_unreachable_and_bodyless_providers() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        assert_eq!(check_provider(&format!("http://{address}/v1"), "", "").await, check(false, &[], Some(UNREACHABLE)));
        assert_eq!(check_provider("not a url", "", "").await, check(false, &[], Some(UNREACHABLE)));
        let (url, _) = provider(http("200 OK", "", b"")).await;
        assert_eq!(check_provider(&url, "", "").await, check(false, &[], Some(INCOMPATIBLE)));
        let (url, _) = provider(Reply { bytes: b"HTTP/1.1 204 No Content\r\n\r\n".to_vec(), stall: false }).await;
        assert_eq!(check_provider(&url, "", "").await, check(false, &[], Some(UNREACHABLE)));
        // A key that cannot be sent as a header fails like `fetch` does.
        let (url, _) = provider(json(r#"{"data":[]}"#)).await;
        assert_eq!(check_provider(&url, "bad\nkey", "").await, check(false, &[], Some(UNREACHABLE)));
    }
}
