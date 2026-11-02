//! Per-run, frozen-credential MCP relay; this is not an OAuth client or a general gateway.
//!
//! Understood as: the engine receives only a disposable loopback lease, while this module owns
//! the frozen upstream bearer, pins each HTTPS request, and sanitizes every upstream response.
//! No refresh, redirects, ambient proxies, credential persistence, or request retries occur here.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::connect_info::Connected;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::{FutureExt, StreamExt};
use grok_bot_model::{HttpError, HttpResult, McpPair};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinHandle;
use url::{Host, Url};

use crate::computer::network::is_global_ip;

const MAX_BODY: usize = 8 * 1024 * 1024;
const MAX_SSE_EVENT: usize = 1024 * 1024;
// A tools/list JSON-RPC event can contain the entire catalog in one data line (Notion exceeds
// 64 KiB). Bound that line by the existing event budget, not a smaller arbitrary transport chunk.
const MAX_SSE_LINE: usize = MAX_SSE_EVENT;
const MAX_SSE_FIELDS: usize = 1024;
const MAX_DNS_ANSWERS: usize = 64;
// The store allows 64 configured pairs; runtime adds one OAuth Authorization pair.
const MAX_CONFIG_HEADERS: usize = 64 + 1;
// A legal MCP header document is up to 64 KiB before the frozen OAuth bearer is added.
// This configuration budget does not relax axum's on-the-wire request header limits.
const MAX_CONFIG_BYTES: usize = 128 * 1024;
// Cover the complete upstream redaction set, not only the retired-token history: 256 retired
// tokens, 128 active access/refresh pairs, 200 legacy secrets (8192 UTF-16 units each), provider
// credentials, configured/full-Bearer values, and local leases. UTF-8 legacy secrets alone may
// occupy nearly 4.7 MiB; the OAuth strings can add another 4 MiB. Never truncate active keys.
const MAX_KEYS: usize = 1024;
const MAX_KEY_BYTES: usize = 16 * 1024 * 1024;
const MAX_IN_FLIGHT: usize = 32;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const BODY_IDLE: Duration = Duration::from_secs(30);
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(2);
const ERROR_JSON: &str = r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"MCP relay request failed."}}"#;
const AUTH_JSON: &str = r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32001,"message":"MCP authorization is required."}}"#;
const ERROR_SSE: &str = "event: error\ndata: {\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32603,\"message\":\"MCP relay response failed.\"}}\n\n";
const REQUEST_HEADERS: [&str; 5] = ["accept", "content-type", "mcp-session-id", "mcp-protocol-version", "last-event-id"];
const RESPONSE_HEADERS: [&str; 4] = ["content-type", "mcp-session-id", "mcp-protocol-version", "cache-control"];

type Unauthorized = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// Keep this in the owning run. It deliberately has neither `Clone` nor a credential-printing
/// `Debug` implementation. Dropping it closes the listener, interrupts forwards, and stops bodies.
pub(crate) struct Lease {
    pub url: String,
    pub authorization: String,
    stop: watch::Sender<bool>,
    _server: JoinHandle<()>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        // The server task owns the listener and completes graceful connection shutdown. Pending
        // forwards and response bodies observe this same signal; no background reader is spawned.
    }
}

pub(crate) async fn start(remote_url: &str, headers: Vec<McpPair>, keys: Vec<String>, on_unauthorized: Unauthorized) -> HttpResult<Lease> {
    start_inner(
        remote_url,
        headers,
        keys,
        on_unauthorized,
        #[cfg(test)]
        None,
    )
    .await
}

struct Relay {
    remote: Url,
    configured: HeaderMap,
    authorization: HeaderValue,
    local_digest: [u8; 32],
    host: String,
    redactor: Arc<Redactor>,
    on_unauthorized: Unauthorized,
    unauthorized: AtomicBool,
    stop: watch::Receiver<bool>,
    slots: Arc<Semaphore>,
    #[cfg(test)]
    fetch: Option<TestFetch>,
}

async fn start_inner(
    remote_url: &str,
    headers: Vec<McpPair>,
    keys: Vec<String>,
    on_unauthorized: Unauthorized,
    #[cfg(test)] fetch: Option<TestFetch>,
) -> HttpResult<Lease> {
    let remote = remote_url_checked(remote_url)?;
    let (configured, authorization, redactor) = configuration(headers, keys)?;
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|_| HttpError::unavailable("MCP relay could not start."))?;
    let authorization_local = format!("Bearer {}", hex::encode(random));
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.map_err(|_| HttpError::unavailable("MCP relay could not start."))?;
    let address = listener.local_addr().map_err(|_| HttpError::unavailable("MCP relay could not start."))?;
    let (stop_tx, stop) = watch::channel(false);
    let relay = Arc::new(Relay {
        remote,
        configured,
        authorization,
        local_digest: digest(authorization_local.as_bytes()),
        host: address.to_string(),
        redactor: Arc::new(redactor),
        on_unauthorized,
        unauthorized: AtomicBool::new(false),
        stop: stop.clone(),
        slots: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
        #[cfg(test)]
        fetch,
    });
    // A fallback rather than a route also rejects encoded/alternate paths, queries, HEAD, and
    // OPTIONS in the same authenticated handler; axum's default HTTP header bounds remain intact.
    let router = Router::new().fallback(forward).with_state(relay);
    let listener = LeaseListener { inner: listener, stop: stop.clone(), slots: Arc::new(Semaphore::new(MAX_IN_FLIGHT)) };
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router.into_make_service_with_connect_info::<Peer>()).with_graceful_shutdown(stopped(stop)).await;
    });
    Ok(Lease { url: format!("http://{address}/mcp"), authorization: authorization_local, stop: stop_tx, _server: server })
}

fn remote_url_checked(input: &str) -> HttpResult<Url> {
    let invalid = || HttpError::bad_request("MCP relay requires a public HTTPS endpoint without credentials.");
    if input.len() > 4096
        || !input.get(..8).is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
        || input.chars().any(|c| c.is_whitespace() || c.is_control() || c == '\\')
        || input[8..].split(['/', '?', '#']).next().is_none_or(|authority| authority.is_empty() || authority.contains('@'))
    {
        return Err(invalid());
    }
    let bytes = input.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'%' && (index + 2 >= bytes.len() || !bytes[index + 1].is_ascii_hexdigit() || !bytes[index + 2].is_ascii_hexdigit()) {
            return Err(invalid());
        }
    }
    let url = Url::parse(input).map_err(|_| invalid())?;
    if url.scheme() != "https"
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port() == Some(0)
    {
        return Err(invalid());
    }
    match url.host() {
        Some(Host::Ipv4(ip)) if !is_global_ip(IpAddr::V4(ip)) => return Err(invalid()),
        Some(Host::Ipv6(ip)) if !is_global_ip(IpAddr::V6(ip)) => return Err(invalid()),
        _ => {}
    }
    Ok(url)
}

fn forbidden_config_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "connection"
            | "keep-alive"
            | "content-length"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "upgrade"
            | "cookie"
            | "set-cookie"
            | "www-authenticate"
            | "authentication-info"
            | "accept-encoding"
            | "content-encoding"
            | "origin"
            | "referer"
            | "forwarded"
            | "location"
            | "range"
            | "expect"
            | "via"
            | "max-forwards"
            | "x-http-method-override"
            | "x-http-method"
            | "x-method-override"
    ) || name.starts_with("proxy-")
        || name.starts_with("x-forwarded-")
        || name.starts_with("access-control-")
        || name.starts_with("sec-")
}

fn configuration(headers: Vec<McpPair>, mut keys: Vec<String>) -> HttpResult<(HeaderMap, HeaderValue, Redactor)> {
    let invalid = || HttpError::bad_request("MCP relay headers or redaction keys are invalid.");
    if headers.len() > MAX_CONFIG_HEADERS || !normalize_redaction_keys(&mut keys) {
        return Err(invalid());
    }
    let mut configured = HeaderMap::new();
    let mut authorization = None;
    let mut size = 0usize;
    for pair in headers {
        size = size.saturating_add(pair.name.len()).saturating_add(pair.value.len());
        if size > MAX_CONFIG_BYTES || pair.name.len() > 128 || pair.value.chars().any(char::is_control) {
            return Err(invalid());
        }
        let name = HeaderName::from_bytes(pair.name.as_bytes()).map_err(|_| invalid())?;
        let mut value = HeaderValue::from_str(&pair.value).map_err(|_| invalid())?;
        value.set_sensitive(true);
        if name == header::AUTHORIZATION {
            let Some((scheme, token)) = pair.value.split_once(' ') else { return Err(invalid()) };
            if !scheme.eq_ignore_ascii_case("Bearer") || !super::flow::bearer_valid(token) || authorization.is_some() {
                return Err(invalid());
            }
            keys.push(token.to_owned());
            keys.push(pair.value);
            authorization = Some(value);
        } else {
            if forbidden_config_header(name.as_str()) || configured.contains_key(&name) {
                return Err(invalid());
            }
            // Configured header values may themselves be vault secrets. They cannot be echoed
            // into response bodies or session/protocol headers either.
            keys.push(pair.value);
            configured.insert(name, value);
        }
    }
    let authorization = authorization.ok_or_else(invalid)?;
    if !normalize_redaction_keys(&mut keys) {
        return Err(invalid());
    }
    Ok((configured, authorization, Redactor { keys }))
}

fn normalize_redaction_keys(keys: &mut Vec<String>) -> bool {
    // The service and headers can name the same secret repeatedly. Charge unique nonempty
    // values only, both before accepting the service set and after adding frozen header values.
    keys.retain(|key| !key.is_empty());
    keys.sort_unstable_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    keys.dedup();
    keys.len() <= MAX_KEYS && keys.iter().try_fold(0usize, |total, key| total.checked_add(key.len())).is_some_and(|total| total <= MAX_KEY_BYTES)
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[allow(deprecated)]
fn authorized(headers: &HeaderMap, expected: &[u8; 32]) -> bool {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let presented = values.next().map(HeaderValue::as_bytes).unwrap_or_default();
    let actual = digest(presented);
    // Compare fixed-size digests, not variable-size credentials. Duplicate credentials are denied.
    ring::constant_time::verify_slices_are_equal(&actual, expected).is_ok() && values.next().is_none()
}

async fn stopped(mut stop: watch::Receiver<bool>) {
    loop {
        if *stop.borrow_and_update() {
            return;
        }
        if stop.changed().await.is_err() {
            return;
        }
    }
}

fn error_response(status: StatusCode) -> Response {
    let mut response = Response::new(Body::from(if status == StatusCode::UNAUTHORIZED { AUTH_JSON } else { ERROR_JSON }));
    *response.status_mut() = status;
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn forward(State(relay): State<Arc<Relay>>, ConnectInfo(peer): ConnectInfo<Peer>, request: Request) -> Response {
    let headers = request.headers();
    if peer.0.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST)
        || !authorized(headers, &relay.local_digest)
        || headers.contains_key(header::ORIGIN)
        || headers.get_all(header::HOST).iter().count() != 1
        || headers.get(header::HOST).is_none_or(|host| host.as_bytes() != relay.host.as_bytes())
    {
        return error_response(StatusCode::FORBIDDEN);
    }
    // Reject absolute-form proxy requests as well as query, path, and authority changes.
    if request.uri().scheme().is_some() || request.uri().authority().is_some() || request.uri().path_and_query().is_none_or(|path| path.as_str() != "/mcp") {
        return error_response(StatusCode::NOT_FOUND);
    }
    if !matches!(*request.method(), Method::GET | Method::POST | Method::DELETE) {
        return error_response(StatusCode::METHOD_NOT_ALLOWED);
    }
    let Ok(permit) = relay.slots.clone().try_acquire_owned() else {
        return error_response(StatusCode::SERVICE_UNAVAILABLE);
    };
    tokio::select! {
        biased;
        _ = stopped(relay.stop.clone()) => error_response(StatusCode::SERVICE_UNAVAILABLE),
        response = forward_authorized(&relay, request, permit) => response,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    Transport,
    Invalid,
    TooLarge,
    Timeout,
}

struct UpstreamRequest {
    url: Url,
    method: Method,
    headers: HeaderMap,
    body: Vec<u8>,
}

struct UpstreamResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: BoxStream<'static, Result<Vec<u8>, Fault>>,
}

#[cfg(test)]
type TestFetch = Arc<dyn Fn(UpstreamRequest) -> BoxFuture<'static, Result<UpstreamResponse, Fault>> + Send + Sync>;

async fn forward_authorized(relay: &Relay, request: Request, permit: OwnedSemaphorePermit) -> Response {
    let (parts, body) = request.into_parts();
    // Only protocol headers from the engine are considered; all configured values are immutable.
    let mut headers = HeaderMap::new();
    let nominated = connection_headers(&parts.headers);
    for name in REQUEST_HEADERS {
        if nominated.iter().any(|item| item.eq_ignore_ascii_case(name)) {
            continue;
        }
        let mut values = parts.headers.get_all(name).iter();
        if let Some(value) = values.next() {
            if values.next().is_some() {
                return error_response(StatusCode::BAD_REQUEST);
            }
            headers.insert(HeaderName::from_static(name), value.clone());
        }
    }
    for (name, value) in &relay.configured {
        headers.insert(name.clone(), value.clone());
    }
    headers.insert(header::AUTHORIZATION, relay.authorization.clone());
    headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    if parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|size| size > MAX_BODY as u64)
    {
        return error_response(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let body = match bounded_body(body.into_data_stream().map(|chunk| chunk.map(|bytes| bytes.to_vec()).map_err(|_| Fault::Transport)).boxed(), MAX_BODY).await
    {
        Ok(body) => body,
        Err(Fault::TooLarge) => return error_response(StatusCode::PAYLOAD_TOO_LARGE),
        Err(_) => return error_response(StatusCode::BAD_REQUEST),
    };
    let upstream_request = UpstreamRequest { url: relay.remote.clone(), method: parts.method, headers, body };
    let response = tokio::time::timeout(HEADER_TIMEOUT, async {
        #[cfg(test)]
        if let Some(fetch) = &relay.fetch {
            return fetch(upstream_request).await;
        }
        fetch_upstream(upstream_request).await
    })
    .await;
    let response = match response {
        Ok(Ok(response)) => response,
        _ => return error_response(StatusCode::BAD_GATEWAY),
    };
    if response.status == StatusCode::UNAUTHORIZED {
        // Drop the raw response before invoking application code. The callback receives no raw
        // errors, URL, headers, or body, and it is never run for DNS/TLS/timeout failures.
        drop(response);
        if !relay.unauthorized.swap(true, Ordering::AcqRel)
            && let Ok(future) = std::panic::catch_unwind(AssertUnwindSafe(|| (relay.on_unauthorized)()))
        {
            let _ = tokio::time::timeout(CALLBACK_TIMEOUT, AssertUnwindSafe(future).catch_unwind()).await;
        }
        return error_response(StatusCode::UNAUTHORIZED);
    }
    if !response.status.is_success() {
        // Informational responses cannot safely carry a JSON body or switch this local transport.
        return error_response(if response.status.is_informational() { StatusCode::BAD_GATEWAY } else { response.status });
    }
    if response.headers.get(header::CONTENT_ENCODING).is_some_and(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity")) {
        return error_response(StatusCode::BAD_GATEWAY);
    }
    let mut safe_headers = match response_headers(&response.headers, &relay.redactor) {
        Ok(headers) => headers,
        Err(_) => return error_response(StatusCode::BAD_GATEWAY),
    };
    let media_type =
        response.headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()).unwrap_or_default().split(';').next().unwrap_or_default().trim();
    let status = response.status;
    let body = if status == StatusCode::NO_CONTENT || status == StatusCode::RESET_CONTENT || status == StatusCode::ACCEPTED {
        // MCP notifications and session deletion may be empty. Never relay an unexamined body.
        Body::empty()
    } else if media_type.eq_ignore_ascii_case("application/json") {
        if response
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|size| size > MAX_BODY as u64)
        {
            return error_response(StatusCode::BAD_GATEWAY);
        }
        let bytes = match bounded_body(response.body, MAX_BODY).await {
            Ok(bytes) => bytes,
            Err(_) => return error_response(StatusCode::BAD_GATEWAY),
        };
        let bytes = match relay.redactor.json(&bytes, MAX_BODY) {
            Ok(bytes) => bytes,
            Err(_) => return error_response(StatusCode::BAD_GATEWAY),
        };
        safe_headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Body::from(bytes)
    } else if media_type.eq_ignore_ascii_case("text/event-stream") {
        safe_headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        let stream = sse_stream(response.body, relay.redactor.clone(), relay.stop.clone(), permit);
        Body::from_stream(stream)
    } else {
        return error_response(StatusCode::BAD_GATEWAY);
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = safe_headers;
    response
}

async fn fetch_upstream(request: UpstreamRequest) -> Result<UpstreamResponse, Fault> {
    let host = request.url.host_str().ok_or(Fault::Invalid)?;
    let port = request.url.port_or_known_default().ok_or(Fault::Invalid)?;
    let addresses: Vec<SocketAddr> = match request.url.host() {
        Some(Host::Ipv4(ip)) => vec![SocketAddr::new(IpAddr::V4(ip), port)],
        Some(Host::Ipv6(ip)) => vec![SocketAddr::new(IpAddr::V6(ip), port)],
        Some(Host::Domain(_)) => tokio::net::lookup_host((host, port)).await.map_err(|_| Fault::Transport)?.take(MAX_DNS_ANSWERS + 1).collect(),
        None => return Err(Fault::Invalid),
    };
    validate_addresses(&addresses)?;
    // A fresh client for every request: no cached DNS or existing pool can bypass this check.
    // DNS overrides change the actual socket destination, not the URL, Host, certificate, or SNI.
    let client = reqwest::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(BODY_IDLE)
        .pool_max_idle_per_host(0)
        .resolve_to_addrs(host, &addresses)
        .build()
        .map_err(|_| Fault::Transport)?;
    let response = client.request(request.method, request.url).headers(request.headers).body(request.body).send().await.map_err(|_| Fault::Transport)?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.bytes_stream().map(|chunk| chunk.map(|bytes| bytes.to_vec()).map_err(|_| Fault::Transport)).boxed();
    Ok(UpstreamResponse { status, headers, body })
}

fn validate_addresses(addresses: &[SocketAddr]) -> Result<(), Fault> {
    if addresses.is_empty() || addresses.len() > MAX_DNS_ANSWERS || addresses.iter().any(|address| !is_global_ip(address.ip())) {
        return Err(Fault::Invalid);
    }
    Ok(())
}

async fn bounded_body(mut body: BoxStream<'static, Result<Vec<u8>, Fault>>, limit: usize) -> Result<Vec<u8>, Fault> {
    let mut buffer = Vec::new();
    while let Some(chunk) = tokio::time::timeout(BODY_IDLE, body.next()).await.map_err(|_| Fault::Timeout)? {
        let chunk = chunk?;
        if chunk.len() > limit.saturating_sub(buffer.len()) {
            return Err(Fault::TooLarge);
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(buffer)
}

fn connection_headers(headers: &HeaderMap) -> Vec<&str> {
    headers.get_all(header::CONNECTION).iter().filter_map(|value| value.to_str().ok()).flat_map(|value| value.split(',').map(str::trim)).collect()
}

fn response_headers(headers: &HeaderMap, redactor: &Redactor) -> Result<HeaderMap, Fault> {
    let mut safe = HeaderMap::new();
    // A Connection-nominated header is hop-by-hop even when its name appears in our allowlist.
    let nominated = connection_headers(headers);
    for name in RESPONSE_HEADERS {
        if nominated.iter().any(|item| item.eq_ignore_ascii_case(name)) {
            continue;
        }
        let mut values = headers.get_all(name).iter();
        if let Some(value) = values.next() {
            if values.next().is_some() {
                return Err(Fault::Invalid);
            }
            let value = value.to_str().map_err(|_| Fault::Invalid)?;
            let value = redactor.text(value, MAX_CONFIG_BYTES)?;
            safe.insert(HeaderName::from_static(name), HeaderValue::from_str(&value).map_err(|_| Fault::Invalid)?);
        }
    }
    Ok(safe)
}

struct Redactor {
    keys: Vec<String>,
}

impl Redactor {
    fn text(&self, input: &str, limit: usize) -> Result<String, Fault> {
        let mut output = String::new();
        let mut cursor = 0;
        // Cache the next match of each key. Re-searching *every* remaining suffix for every
        // replacement is quadratic on repetitive input and could stall the async executor.
        let mut next: Vec<Option<usize>> = self.keys.iter().map(|key| input.find(key)).collect();
        while cursor < input.len() {
            let found = next
                .iter()
                .enumerate()
                .filter_map(|(index, at)| at.map(|at| (at, self.keys[index].len())))
                .min_by(|(left_at, left_len), (right_at, right_len)| left_at.cmp(right_at).then_with(|| right_len.cmp(left_len)));
            let Some((at, length)) = found else {
                append_bounded(&mut output, &input[cursor..], limit)?;
                break;
            };
            append_bounded(&mut output, &input[cursor..at], limit)?;
            append_bounded(&mut output, "[redacted]", limit)?;
            cursor = at + length;
            for (index, at) in next.iter_mut().enumerate() {
                if at.is_some_and(|at| at < cursor) {
                    *at = input[cursor..].find(&self.keys[index]).map(|at| at + cursor);
                }
            }
        }
        if self.keys.iter().any(|key| output.contains(key)) {
            // Extremely short/adversarial keys can overlap the replacement marker itself.
            return Err(Fault::Invalid);
        }
        Ok(output)
    }

    fn value(&self, value: &mut Value, limit: usize) -> Result<(), Fault> {
        match value {
            Value::String(text) => *text = self.text(text, limit)?,
            Value::Array(items) => {
                for item in items {
                    self.value(item, limit)?;
                }
            }
            Value::Object(fields) => {
                // Object names are strings too; an upstream echo in a key must not leak either.
                let old = std::mem::take(fields);
                for (key, mut value) in old {
                    self.value(&mut value, limit)?;
                    fields.insert(self.text(&key, limit)?, value);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn json(&self, bytes: &[u8], limit: usize) -> Result<Vec<u8>, Fault> {
        if bytes.len() > limit {
            return Err(Fault::TooLarge);
        }
        let mut value: Value = serde_json::from_slice(bytes).map_err(|_| Fault::Invalid)?;
        self.value(&mut value, limit)?;
        let mut output = BoundedJson { bytes: Vec::new(), limit };
        serde_json::to_writer(&mut output, &value).map_err(|_| Fault::TooLarge)?;
        Ok(output.bytes)
    }
}

struct BoundedJson {
    bytes: Vec<u8>,
    limit: usize,
}

impl io::Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "MCP relay response exceeds the limit."));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn append_bounded(output: &mut String, text: &str, limit: usize) -> Result<(), Fault> {
    if text.len() > limit.saturating_sub(output.len()) {
        return Err(Fault::TooLarge);
    }
    output.push_str(text);
    Ok(())
}

#[derive(Default)]
struct SseParser {
    line: Vec<u8>,
    fields: Vec<String>,
    event_bytes: usize,
    after_cr: bool,
    started: bool,
}

impl SseParser {
    fn byte(&mut self, byte: u8, redactor: &Redactor) -> Result<Option<Vec<u8>>, Fault> {
        if self.after_cr {
            self.after_cr = false;
            if byte == b'\n' {
                return Ok(None);
            }
        }
        if byte == b'\r' || byte == b'\n' {
            self.after_cr = byte == b'\r';
            return self.finish_line(redactor);
        }
        if self.line.len() >= MAX_SSE_LINE || byte == 0 {
            return Err(Fault::TooLarge);
        }
        self.line.push(byte);
        Ok(None)
    }

    fn finish_line(&mut self, redactor: &Redactor) -> Result<Option<Vec<u8>>, Fault> {
        let bytes = std::mem::take(&mut self.line);
        let mut line = String::from_utf8(bytes).map_err(|_| Fault::Invalid)?;
        if !self.started {
            self.started = true;
            if let Some(stripped) = line.strip_prefix('\u{feff}') {
                line = stripped.to_owned();
            }
        }
        if line.is_empty() {
            return self.finish_event(redactor);
        }
        self.event_bytes = self.event_bytes.saturating_add(line.len() + 1);
        if self.event_bytes > MAX_SSE_EVENT || self.fields.len() >= MAX_SSE_FIELDS {
            return Err(Fault::TooLarge);
        }
        self.fields.push(line);
        Ok(None)
    }

    fn finish_event(&mut self, redactor: &Redactor) -> Result<Option<Vec<u8>>, Fault> {
        if self.fields.is_empty() {
            return Ok(None);
        }
        let mut data = String::new();
        let mut has_data = false;
        for line in &self.fields {
            let (name, value) = sse_field(line);
            if name == "data" {
                if has_data {
                    append_bounded(&mut data, "\n", MAX_SSE_EVENT)?;
                }
                append_bounded(&mut data, value, MAX_SSE_EVENT)?;
                has_data = true;
            }
        }
        let json = if has_data { Some(redactor.json(data.as_bytes(), MAX_SSE_EVENT)?) } else { None };
        let mut output = String::new();
        let mut wrote_data = false;
        for line in self.fields.drain(..) {
            if sse_field(&line).0 == "data" {
                if !wrote_data {
                    append_bounded(&mut output, "data: ", MAX_SSE_EVENT)?;
                    // Serialization of a JSON value always produces valid UTF-8.
                    append_bounded(&mut output, std::str::from_utf8(json.as_deref().ok_or(Fault::Invalid)?).map_err(|_| Fault::Invalid)?, MAX_SSE_EVENT)?;
                    append_bounded(&mut output, "\n", MAX_SSE_EVENT)?;
                    wrote_data = true;
                }
            } else {
                append_bounded(&mut output, &redactor.text(&line, MAX_SSE_LINE)?, MAX_SSE_EVENT)?;
                append_bounded(&mut output, "\n", MAX_SSE_EVENT)?;
            }
        }
        append_bounded(&mut output, "\n", MAX_SSE_EVENT)?;
        self.event_bytes = 0;
        Ok(Some(output.into_bytes()))
    }

    fn complete(&self) -> bool {
        self.line.is_empty() && self.fields.is_empty()
    }
}

fn sse_field(line: &str) -> (&str, &str) {
    let (name, value) = line.split_once(':').unwrap_or((line, ""));
    (name, value.strip_prefix(' ').unwrap_or(value))
}

struct SseStream {
    body: BoxStream<'static, Result<Vec<u8>, Fault>>,
    parser: SseParser,
    chunk: Vec<u8>,
    cursor: usize,
    redactor: Arc<Redactor>,
    done: bool,
    _permit: OwnedSemaphorePermit,
}

fn sse_stream(
    body: BoxStream<'static, Result<Vec<u8>, Fault>>,
    redactor: Arc<Redactor>,
    stop: watch::Receiver<bool>,
    permit: OwnedSemaphorePermit,
) -> BoxStream<'static, Result<Bytes, io::Error>> {
    let state = SseStream { body, parser: SseParser::default(), chunk: Vec::new(), cursor: 0, redactor, done: false, _permit: permit };
    futures::stream::unfold(state, |mut state| async move {
        if state.done {
            return None;
        }
        loop {
            while state.cursor < state.chunk.len() {
                let byte = state.chunk[state.cursor];
                state.cursor += 1;
                match state.parser.byte(byte, &state.redactor) {
                    Ok(Some(event)) => return Some((Ok(Bytes::from(event)), state)),
                    Ok(None) => {}
                    Err(_) => {
                        state.done = true;
                        return Some((Ok(Bytes::from_static(ERROR_SSE.as_bytes())), state));
                    }
                }
            }
            state.chunk.clear();
            state.cursor = 0;
            match tokio::time::timeout(BODY_IDLE, state.body.next()).await {
                Ok(Some(Ok(chunk))) if chunk.len() <= MAX_BODY => state.chunk = chunk,
                Ok(None) if state.parser.complete() => return None,
                _ => {
                    state.done = true;
                    return Some((Ok(Bytes::from_static(ERROR_SSE.as_bytes())), state));
                }
            }
            // Even a peer producing an endless sequence of empty chunks/blank lines cannot keep
            // this poll busy forever or starve lease cancellation on the same executor.
            tokio::task::yield_now().await;
        }
    })
    .take_until(stopped(stop))
    .boxed()
}

// Axum's graceful shutdown alone leaves partial headers and write-backpressured responses
// alive. These are reproduced in the private tests below. The guard merely gives accepted TCP
// IO the same stop signal as handlers/bodies; HTTP parsing and header limits remain axum's.
struct LeaseListener {
    inner: TcpListener,
    stop: watch::Receiver<bool>,
    slots: Arc<Semaphore>,
}

#[derive(Clone, Copy)]
struct Peer(SocketAddr);

impl Connected<axum::serve::IncomingStream<'_, LeaseListener>> for Peer {
    fn connect_info(stream: axum::serve::IncomingStream<'_, LeaseListener>) -> Self {
        Self(*stream.remote_addr())
    }
}

impl axum::serve::Listener for LeaseListener {
    type Io = LeaseIo;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            // The semaphore is private and never closed. Graceful shutdown cancels this accept
            // future even if all slots are occupied by partial headers or slow consumers.
            let Ok(permit) = self.slots.clone().acquire_owned().await else { continue };
            match self.inner.accept().await {
                Ok((socket, peer)) if peer.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST) => {
                    return (LeaseIo { socket, stop: Box::pin(stopped(self.stop.clone())), closed: false, _permit: permit }, peer);
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

struct LeaseIo {
    socket: TcpStream,
    stop: BoxFuture<'static, ()>,
    closed: bool,
    _permit: OwnedSemaphorePermit,
}

impl LeaseIo {
    fn check(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if !self.closed && self.stop.as_mut().poll(cx).is_ready() {
            self.closed = true;
        }
        if self.closed { Err(io::Error::new(io::ErrorKind::ConnectionAborted, "MCP relay connection closed.")) } else { Ok(()) }
    }
}

impl AsyncRead for LeaseIo {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buffer: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if let Err(error) = self.check(cx) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.socket).poll_read(cx, buffer)
    }
}

impl AsyncWrite for LeaseIo {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buffer: &[u8]) -> Poll<io::Result<usize>> {
        if let Err(error) = self.check(cx) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.socket).poll_write(cx, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Err(error) = self.check(cx) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.socket).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.socket).poll_shutdown(cx)
    }
}

#[cfg(test)]
#[path = "test_support.rs"]
pub(crate) mod test_support;

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::task::Poll;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::sync::Notify;

    // Synthetic, non-account fixtures. Assertions deliberately report booleans/counters rather
    // than formatting bearer values, upstream bodies, or requests on failure.
    const TOKEN: &str = "relay-userId:relay-grantId:relay-secret";
    const VAULT_KEY: &str = "relay-fixture-vault-39";
    const REMOTE: &str = "https://relay.example/resource/mcp?fixed=1";
    const TEST_WAIT: Duration = Duration::from_secs(3);

    fn headers() -> Vec<McpPair> {
        vec![McpPair { name: "Authorization".into(), value: format!("Bearer {TOKEN}") }, McpPair { name: "X-Configured".into(), value: VAULT_KEY.into() }]
    }

    fn noop() -> Unauthorized {
        Arc::new(|| Box::pin(async {}))
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(TEST_WAIT).build().unwrap()
    }

    fn reply(status: StatusCode, content_type: &'static str, chunks: Vec<Vec<u8>>) -> UpstreamResponse {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        UpstreamResponse { status, headers, body: futures::stream::iter(chunks.into_iter().map(Ok)).boxed() }
    }

    async fn lease(fetch: TestFetch) -> Lease {
        start_inner(REMOTE, headers(), vec![VAULT_KEY.into()], noop(), Some(fetch)).await.unwrap()
    }

    fn redactor() -> Arc<Redactor> {
        Arc::new(configuration(headers(), vec![VAULT_KEY.into()]).unwrap().2)
    }

    async fn collect_sse(chunks: Vec<Vec<u8>>, redactor: Arc<Redactor>) -> Vec<u8> {
        let (_stop, receiver) = watch::channel(false);
        let permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let mut stream = sse_stream(futures::stream::iter(chunks.into_iter().map(Ok)).boxed(), redactor, receiver, permit);
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk.unwrap());
        }
        bytes
    }

    #[test]
    fn destination_syntax_and_every_dns_answer_are_checked() {
        assert!(remote_url_checked(REMOTE).is_ok());
        for url in [
            "http://relay.example/mcp",
            "https://127.0.0.1/mcp",
            "https://[::1]/mcp",
            "https://user:pass@relay.example/mcp",
            "https://@relay.example/mcp",
            "https://relay.example/mcp#part",
            "https://relay.example:0/mcp",
            "https://relay.example/mcp%zz",
            " https://relay.example/mcp",
            "https://relay.example\\other/mcp",
            "https:///relay.example/mcp",
        ] {
            assert!(remote_url_checked(url).is_err());
        }
        let public: SocketAddr = "1.1.1.1:443".parse().unwrap();
        let public_v6: SocketAddr = "[2606:4700:4700::1111]:443".parse().unwrap();
        assert!(validate_addresses(&[public, public_v6]).is_ok());
        assert!(validate_addresses(&[]).is_err());
        assert!(validate_addresses(&vec![public; MAX_DNS_ANSWERS + 1]).is_err());
        for address in ["10.0.0.1:443", "127.0.0.1:443", "169.254.169.254:443", "[::ffff:127.0.0.1]:443", "[2001:db8::1]:443"] {
            let address = address.parse().unwrap();
            assert!(validate_addresses(&[public, address]).is_err());
            assert!(validate_addresses(&[address, public]).is_err());
        }
    }

    #[test]
    fn opaque_bearer_header_and_redaction_share_the_exchange_validator() {
        let access = "userId:grantId:secret";
        let pairs = vec![McpPair { name: "Authorization".into(), value: format!("Bearer {access}") }];
        let (_, header, redactor) = configuration(pairs, vec![]).unwrap();
        assert!(header.is_sensitive());
        assert!(header.as_bytes() == format!("Bearer {access}").as_bytes());
        let body = redactor.json(serde_json::to_string(&serde_json::json!({"text":access})).unwrap().as_bytes(), MAX_BODY).unwrap();
        assert!(!String::from_utf8(body).unwrap().contains(access));
        for rejected in ["user id:grant:secret", "user\tid:grant:secret", "user:id:sec\"ret", "user:id:sec,ret", "user:id:sec\\ret", "user:id:secrét", "short"]
        {
            assert!(!super::super::flow::bearer_valid(rejected));
            let pairs = vec![McpPair { name: "Authorization".into(), value: format!("Bearer {rejected}") }];
            assert!(configuration(pairs, vec![]).is_err());
        }
    }

    #[test]
    fn immutable_configuration_excludes_routing_and_credential_overrides() {
        for name in [
            "Host",
            "Cookie",
            "Proxy-Authorization",
            "Connection",
            "Content-Length",
            "Transfer-Encoding",
            "Accept-Encoding",
            "Origin",
            "X-Forwarded-Host",
            "Sec-Fetch-Site",
        ] {
            let mut values = headers();
            values.push(McpPair { name: name.into(), value: "fixture".into() });
            assert!(configuration(values, vec![]).is_err());
        }
        let mut values = headers();
        values.push(McpPair { name: "authorization".into(), value: "Bearer duplicate-fixture".into() });
        assert!(configuration(values, vec![]).is_err());
        assert!(configuration(vec![], vec![]).is_err());
        assert!(configuration(vec![McpPair { name: "Authorization".into(), value: "Basic fixture".into() }], vec![]).is_err());
        let mut values = headers();
        values.push(McpPair { name: "X-Fixture".into(), value: "bad\r\nheader".into() });
        assert!(configuration(values, vec![]).is_err());
    }

    #[test]
    fn saturated_oauth_history_active_legacy_and_lease_keys_fit_without_truncation() {
        let mut keys = Vec::new();
        for kind in ["retired", "active"] {
            for index in 0..256 {
                let prefix = format!("relay-fixture-{kind}-{index:03}-");
                keys.push(format!("{prefix}{}", "A".repeat(8192 - prefix.len())));
            }
        }
        for index in 0..200 {
            let prefix = format!("relay-fixture-legacy-{index:03}-");
            let key = format!("{prefix}{}", "界".repeat(8192 - prefix.len()));
            assert!(key.encode_utf16().count() == 8192);
            keys.push(key);
        }
        let provider = format!("relay-fixture-provider-{}", "P".repeat(8192 - "relay-fixture-provider-".len()));
        keys.push(provider.clone());
        for index in 0..10 {
            keys.push(format!("relay-fixture-local-{index:02}-{}", "b".repeat(64)));
        }
        let local_lease = keys.last().unwrap().clone();
        let frozen = keys[256].clone();
        let bearer = format!("Bearer {frozen}");
        let mut headers = vec![McpPair { name: "Authorization".into(), value: bearer.clone() }];
        // A legal 64-pair header document fits under 64 KiB including JSON framing; the frozen
        // 8-KiB OAuth credential is then added separately by the runtime.
        let mut configured_document = serde_json::Map::new();
        for index in 0..64 {
            let prefix = format!("relay-fixture-header-{index:02}-");
            let value = if index < 8 { format!("{prefix}{}", "C".repeat(7600 - prefix.len())) } else { prefix };
            let name = format!("X-Fixture-{index:02}");
            configured_document.insert(name.clone(), Value::String(value.clone()));
            headers.push(McpPair { name, value: value.clone() });
            keys.push(value);
        }
        assert!(serde_json::to_vec(&configured_document).unwrap().len() < 64 * 1024);
        assert!(headers.len() == MAX_CONFIG_HEADERS);
        assert!(headers.iter().map(|pair| pair.name.len() + pair.value.len()).sum::<usize>() > 64 * 1024);
        assert!(keys.iter().map(String::len).sum::<usize>() < MAX_KEY_BYTES);
        let mut duplicate_sources = keys.clone();
        duplicate_sources.extend(keys.iter().cloned());
        duplicate_sources.push(String::new());
        assert!(duplicate_sources.len() > MAX_KEYS);
        assert!(duplicate_sources.iter().map(String::len).sum::<usize>() > MAX_KEY_BYTES);
        let (_, _, redactor) = configuration(headers, duplicate_sources).unwrap();
        assert!(redactor.keys.len() == keys.len() + 1);
        assert!(keys.iter().all(|key| redactor.keys.iter().any(|stored| stored == key)));
        assert!(redactor.keys.iter().any(|stored| stored == &bearer));
        let samples = [&keys[0], &keys[256], &keys[512], &provider, &local_lease, keys.last().unwrap(), &bearer];
        let bytes = serde_json::to_vec(&samples).unwrap();
        let bytes = redactor.json(&bytes, MAX_BODY).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value.as_array().unwrap().iter().all(|item| item == "[redacted]"));
    }

    #[test]
    fn unique_key_and_header_budgets_reject_excess_before_and_after_header_additions() {
        // The default frozen bearer contributes its bare token, full header, and X-Configured.
        let keys = (0..MAX_KEYS - 3).map(|index| format!("relay-fixture-boundary-{index:04}")).collect::<Vec<_>>();
        let (_, _, redactor) = configuration(headers(), keys.clone()).unwrap();
        assert!(redactor.keys.len() == MAX_KEYS);
        let mut excessive_after_headers = keys;
        excessive_after_headers.push("relay-fixture-extra-unique".into());
        assert!(configuration(headers(), excessive_after_headers).is_err());
        let excessive_before_headers = (0..=MAX_KEYS).map(|index| format!("relay-fixture-boundary-{index:04}")).collect();
        assert!(configuration(headers(), excessive_before_headers).is_err());
        // A source set that fills the byte budget may not silently discard required header keys.
        assert!(configuration(headers(), vec!["K".repeat(MAX_KEY_BYTES)]).is_err());
        assert!(configuration(headers(), vec!["K".repeat(MAX_KEY_BYTES + 1)]).is_err());
        let mut excessive_headers = headers();
        excessive_headers.push(McpPair { name: "X-Too-Large".into(), value: "H".repeat(MAX_CONFIG_BYTES + 1) });
        assert!(configuration(excessive_headers, vec![]).is_err());
    }

    #[tokio::test]
    async fn local_lease_rejects_auth_origin_host_path_query_and_other_methods() {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let lease = lease(Arc::new(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(reply(StatusCode::OK, "application/json", vec![b"{}".to_vec()])) })
        }))
        .await;
        let local = client();
        assert!(lease.authorization.starts_with("Bearer "));
        assert!(lease.authorization.len() == 7 + 64);
        assert!(!lease.authorization.contains(TOKEN));
        let response = local.post(&lease.url).send().await.unwrap();
        assert!(response.status() == StatusCode::FORBIDDEN);
        let response = local.post(&lease.url).header(header::AUTHORIZATION, "Bearer invalid-fixture").send().await.unwrap();
        assert!(response.status() == StatusCode::FORBIDDEN);
        let response = local.post(&lease.url).header(header::AUTHORIZATION, &lease.authorization).header(header::ORIGIN, "null").send().await.unwrap();
        assert!(response.status() == StatusCode::FORBIDDEN);
        let response = local.post(&lease.url).header(header::AUTHORIZATION, &lease.authorization).header(header::HOST, "localhost").send().await.unwrap();
        assert!(response.status() == StatusCode::FORBIDDEN);
        for suffix in ["?url=https://elsewhere.example", "/other"] {
            let response = local.post(format!("{}{suffix}", lease.url)).header(header::AUTHORIZATION, &lease.authorization).send().await.unwrap();
            assert!(response.status() == StatusCode::NOT_FOUND);
        }
        for method in [Method::OPTIONS, Method::PUT, Method::HEAD] {
            let response = local.request(method, &lease.url).header(header::AUTHORIZATION, &lease.authorization).send().await.unwrap();
            assert!(response.status() == StatusCode::METHOD_NOT_ALLOWED);
            assert!(response.headers().keys().all(|name| !name.as_str().starts_with("access-control-")));
        }
        let mut duplicate = HeaderMap::new();
        duplicate.append(header::AUTHORIZATION, HeaderValue::from_str(&lease.authorization).unwrap());
        duplicate.append(header::AUTHORIZATION, HeaderValue::from_str(&lease.authorization).unwrap());
        assert!(!authorized(&duplicate, &digest(lease.authorization.as_bytes())));
        assert!(count.load(Ordering::SeqCst) == 0);
        let response = local.post(&lease.url).header(header::AUTHORIZATION, &lease.authorization).body("{}").send().await.unwrap();
        assert!(response.status() == StatusCode::OK);
        assert!(count.load(Ordering::SeqCst) == 1);
    }

    #[tokio::test]
    async fn upstream_401_notifies_once_without_retry_or_raw_challenge() {
        let calls = Arc::new(AtomicUsize::new(0));
        let unauthorized = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let fetch: TestFetch = Arc::new(move |request| {
            seen.fetch_add(1, Ordering::SeqCst);
            assert!(request.method == Method::POST);
            assert!(request.url.as_str() == REMOTE);
            assert!(request.headers.get(header::AUTHORIZATION).unwrap().as_bytes() == format!("Bearer {TOKEN}").as_bytes());
            assert!(request.headers.get("x-configured").unwrap().as_bytes() == VAULT_KEY.as_bytes());
            assert!(request.headers.get("x-engine-secret").is_none());
            assert!(request.headers.get(header::COOKIE).is_none());
            assert!(request.headers.get(header::ACCEPT_ENCODING).unwrap() == "identity");
            assert!(request.body == b"{\"method\":\"tools/call\"}");
            Box::pin(async {
                let mut response = reply(StatusCode::UNAUTHORIZED, "text/html", vec![format!("raw error: {TOKEN}").into_bytes()]);
                response.headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_str(&format!("Bearer token={TOKEN}")).unwrap());
                response.headers.insert(header::SET_COOKIE, HeaderValue::from_str(TOKEN).unwrap());
                Ok(response)
            })
        });
        let noticed = unauthorized.clone();
        let callback: Unauthorized = Arc::new(move || {
            let noticed = noticed.clone();
            Box::pin(async move {
                noticed.fetch_add(1, Ordering::SeqCst);
            })
        });
        let lease = start_inner(REMOTE, headers(), vec![], callback, Some(fetch)).await.unwrap();
        for _ in 0..2 {
            let response = client()
                .post(&lease.url)
                .header(header::AUTHORIZATION, &lease.authorization)
                .header("x-engine-secret", "engine-fixture")
                .header(header::COOKIE, "fixture=1")
                .body(r#"{"method":"tools/call"}"#)
                .send()
                .await
                .unwrap();
            assert!(response.status() == StatusCode::UNAUTHORIZED);
            assert!(response.headers().get(header::WWW_AUTHENTICATE).is_none());
            assert!(response.headers().get(header::SET_COOKIE).is_none());
            let bytes = response.bytes().await.unwrap();
            assert!(bytes.as_ref() == AUTH_JSON.as_bytes());
            assert!(!String::from_utf8_lossy(&bytes).contains(TOKEN));
        }
        assert!(calls.load(Ordering::SeqCst) == 2);
        assert!(unauthorized.load(Ordering::SeqCst) == 1);
    }

    #[tokio::test]
    async fn transport_and_non_401_failures_never_infer_needs_auth_or_retry() {
        for status in [None, Some(StatusCode::FORBIDDEN), Some(StatusCode::INTERNAL_SERVER_ERROR), Some(StatusCode::TEMPORARY_REDIRECT)] {
            let count = Arc::new(AtomicUsize::new(0));
            let unauthorized = Arc::new(AtomicUsize::new(0));
            let seen = count.clone();
            let fetch: TestFetch = Arc::new(move |_| {
                seen.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    let Some(status) = status else { return Err(Fault::Transport) };
                    let mut response = reply(status, "application/json", vec![format!(r#"{{"error":"{TOKEN}"}}"#).into_bytes()]);
                    response.headers.insert(header::LOCATION, HeaderValue::from_static("https://other.example/mcp"));
                    Ok(response)
                })
            });
            let noticed = unauthorized.clone();
            let callback: Unauthorized = Arc::new(move || {
                noticed.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {})
            });
            let lease = start_inner(REMOTE, headers(), vec![], callback, Some(fetch)).await.unwrap();
            let response = client().post(&lease.url).header(header::AUTHORIZATION, &lease.authorization).send().await.unwrap();
            assert!(response.status() == status.unwrap_or(StatusCode::BAD_GATEWAY));
            assert!(response.headers().get(header::LOCATION).is_none());
            assert!(response.bytes().await.unwrap().as_ref() == ERROR_JSON.as_bytes());
            assert!(count.load(Ordering::SeqCst) == 1);
            assert!(unauthorized.load(Ordering::SeqCst) == 0);
        }
    }

    #[test]
    fn json_decodes_escaping_before_recursively_redacting_values_and_keys() {
        let encoded = TOKEN.chars().map(|c| format!("\\u{:04x}", c as u32)).collect::<String>();
        let input = format!(r#"{{"nested":[{{"text":"prefix {encoded} suffix"}},"{VAULT_KEY}"],"{TOKEN}":"Bearer {TOKEN}","number":3}}"#);
        let bytes = redactor().json(input.as_bytes(), MAX_BODY).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains(TOKEN));
        assert!(!text.contains(VAULT_KEY));
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value["nested"][0]["text"] == "prefix [redacted] suffix");
        assert!(value["nested"][1] == "[redacted]");
        assert!(value["[redacted]"] == "[redacted]");
        assert!(value["number"] == 3);
        assert!(redactor().json(b"not json", MAX_BODY) == Err(Fault::Invalid));
    }

    #[tokio::test]
    async fn json_response_is_bounded_sanitized_and_only_safe_headers_survive() {
        let lease = lease(Arc::new(|_| {
            Box::pin(async {
                let mut response =
                    reply(StatusCode::OK, "application/json; charset=utf-8", vec![format!(r#"{{"value":"{TOKEN}","nested":["{VAULT_KEY}"]}}"#).into_bytes()]);
                response.headers.insert("mcp-session-id", HeaderValue::from_str(&format!("session-{TOKEN}")).unwrap());
                response.headers.insert("mcp-protocol-version", HeaderValue::from_static("2025-03-26"));
                response.headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                response.headers.insert(header::SET_COOKIE, HeaderValue::from_str(TOKEN).unwrap());
                response.headers.insert(header::LOCATION, HeaderValue::from_static("https://other.example/mcp"));
                response.headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_str(TOKEN).unwrap());
                response.headers.insert("x-debug", HeaderValue::from_str(TOKEN).unwrap());
                Ok(response)
            })
        }))
        .await;
        let response =
            client().get(&lease.url).header(header::AUTHORIZATION, &lease.authorization).header("mcp-session-id", "existing-session").send().await.unwrap();
        assert!(response.status() == StatusCode::OK);
        assert!(response.headers().get("mcp-session-id").unwrap() == "session-[redacted]");
        assert!(response.headers().get("mcp-protocol-version").unwrap() == "2025-03-26");
        for name in ["set-cookie", "location", "www-authenticate", "x-debug"] {
            assert!(response.headers().get(name).is_none());
        }
        let bytes = response.bytes().await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value["value"] == "[redacted]");
        assert!(value["nested"][0] == "[redacted]");
    }

    #[tokio::test]
    async fn sse_redacts_escaped_payloads_across_every_network_boundary() {
        let encoded = TOKEN.chars().map(|c| format!("\\u{:04x}", c as u32)).collect::<String>();
        let source = format!(
            "\u{feff}: heartbeat {VAULT_KEY}\r\nevent: message\r\nid: event-1\r\nretry: 1000\r\ndata: {{\r\ndata: \"text\":\"prefix {encoded} suffix\"}}\r\n\r\n"
        );
        for width in [1, 2, 7, 31, 4096] {
            let chunks = source.as_bytes().chunks(width).map(<[u8]>::to_vec).collect();
            let bytes = collect_sse(chunks, redactor()).await;
            let text = std::str::from_utf8(&bytes).unwrap();
            assert!(!text.contains(TOKEN));
            assert!(!text.contains(VAULT_KEY));
            assert!(text.contains(": heartbeat [redacted]\n"));
            assert!(text.contains("event: message\n"));
            assert!(text.contains("id: event-1\n"));
            assert!(text.contains("retry: 1000\n"));
            let data = text.lines().find_map(|line| line.strip_prefix("data: ")).unwrap();
            let value: Value = serde_json::from_str(data).unwrap();
            assert!(value["text"] == "prefix [redacted] suffix");
        }
    }

    #[tokio::test]
    async fn catalog_json_larger_than_64k_is_one_valid_bounded_sse_event() {
        let tools: Vec<_> = (0..20).map(|index| serde_json::json!({
            "name":format!("catalog_tool_{index}"),
            "description":format!("Synthetic catalog description {TOKEN}"),
            "inputSchema":{"type":"object","properties":{"query":{"type":"string","description":"field documentation ".repeat(300)}},"required":["query"]}
        })).collect();
        let mut source = b"event: message\ndata: ".to_vec();
        source.extend(serde_json::to_vec(&serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"tools":tools}})).unwrap());
        source.extend_from_slice(b"\n\n");
        assert!(source.len() > 64 * 1024 && source.len() < MAX_SSE_EVENT);
        let output = collect_sse(source.chunks(4096).map(<[u8]>::to_vec).collect(), redactor()).await;
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains(TOKEN));
        assert!(!output.contains("event: error"));
        let data = output.lines().find_map(|line| line.strip_prefix("data: ")).unwrap();
        let response: Value = serde_json::from_str(data).unwrap();
        assert_eq!(response["result"]["tools"].as_array().unwrap().len(), 20);
        assert_eq!(response["result"]["tools"][0]["inputSchema"]["properties"]["query"]["type"], "string");
    }

    #[tokio::test]
    async fn caps_cover_json_input_output_sse_lines_events_and_invalid_data() {
        let body = futures::stream::iter([Ok(vec![b'a'; 5]), Ok(vec![b'a'; 6])]).boxed();
        assert!(bounded_body(body, 10).await == Err(Fault::TooLarge));
        let short_key = Redactor { keys: vec!["x".into()] };
        assert!(short_key.json(br#"["xxxx","xxxx"]"#, 24) == Err(Fault::TooLarge));
        for bytes in [
            [b"data: ".as_slice(), &vec![b'a'; MAX_SSE_LINE + 1], b"\n\n"].concat(),
            b"data: not-json\n\n".to_vec(),
            b"data: {\"incomplete\":\"fixture\"}".to_vec(),
            b"data: \xff\n\n".to_vec(),
        ] {
            let bytes = collect_sse(vec![bytes], redactor()).await;
            assert!(bytes.as_slice() == ERROR_SSE.as_bytes());
        }
        let mut event = Vec::new();
        for _ in 0..MAX_SSE_EVENT / 2048 + 1 {
            event.extend_from_slice(b": ");
            event.extend(std::iter::repeat_n(b'a', 2048));
            event.push(b'\n');
        }
        event.push(b'\n');
        let output = collect_sse(event.chunks(4096).map(<[u8]>::to_vec).collect(), redactor()).await;
        assert!(output.as_slice() == ERROR_SSE.as_bytes());
        for content in [vec![b'a'; MAX_BODY + 1], b"not-json".to_vec()] {
            let content = Arc::new(Mutex::new(Some(content)));
            let lease = lease(Arc::new(move |_| {
                let body = content.lock().unwrap().take().unwrap();
                Box::pin(async { Ok(reply(StatusCode::OK, "application/json", vec![body])) })
            }))
            .await;
            let response = client().post(&lease.url).header(header::AUTHORIZATION, &lease.authorization).send().await.unwrap();
            assert!(response.status() == StatusCode::BAD_GATEWAY);
            assert!(response.bytes().await.unwrap().as_ref() == ERROR_JSON.as_bytes());
        }
    }

    #[tokio::test]
    async fn oversized_local_request_never_reaches_upstream() {
        let called = Arc::new(AtomicUsize::new(0));
        let seen = called.clone();
        let lease = lease(Arc::new(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(Fault::Transport) })
        }))
        .await;
        let response = client().post(&lease.url).header(header::AUTHORIZATION, &lease.authorization).body(vec![b'a'; MAX_BODY + 1]).send().await.unwrap();
        assert!(response.status() == StatusCode::PAYLOAD_TOO_LARGE);
        assert!(response.bytes().await.unwrap().as_ref() == ERROR_JSON.as_bytes());
        assert!(called.load(Ordering::SeqCst) == 0);
    }

    #[tokio::test]
    async fn unauthorized_callback_is_bounded_and_never_repeated() {
        let called = Arc::new(AtomicUsize::new(0));
        let seen = called.clone();
        let callback: Unauthorized = Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
        });
        let fetch: TestFetch = Arc::new(|_| Box::pin(async { Ok(reply(StatusCode::UNAUTHORIZED, "text/plain", vec![])) }));
        let lease = start_inner(REMOTE, headers(), vec![], callback, Some(fetch)).await.unwrap();
        for _ in 0..2 {
            let response = client().post(&lease.url).header(header::AUTHORIZATION, &lease.authorization).send().await.unwrap();
            assert!(response.status() == StatusCode::UNAUTHORIZED);
            assert!(response.bytes().await.unwrap().as_ref() == AUTH_JSON.as_bytes());
        }
        assert!(called.load(Ordering::SeqCst) == 1);
    }

    struct DropSignal(Arc<Notify>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }

    #[tokio::test]
    async fn drop_cancels_pending_forward_and_closes_listener_without_auth_callback() {
        let entered = Arc::new(Notify::new());
        let cancelled = Arc::new(Notify::new());
        let entered_fetch = entered.clone();
        let cancelled_fetch = cancelled.clone();
        let lease = lease(Arc::new(move |_| {
            let entered = entered_fetch.clone();
            let guard = DropSignal(cancelled_fetch.clone());
            Box::pin(async move {
                let _guard = guard;
                entered.notify_one();
                std::future::pending().await
            })
        }))
        .await;
        let url = lease.url.clone();
        let address = Url::parse(&url).unwrap().socket_addrs(|| None).unwrap()[0];
        let local_authorization = lease.authorization.clone();
        let call = tokio::spawn(async move { client().post(url).header(header::AUTHORIZATION, local_authorization).body("{}").send().await });
        tokio::time::timeout(TEST_WAIT, entered.notified()).await.unwrap();
        drop(lease);
        tokio::time::timeout(TEST_WAIT, cancelled.notified()).await.unwrap();
        let result = tokio::time::timeout(TEST_WAIT, call).await.unwrap().unwrap();
        if let Ok(response) = result {
            assert!(response.status() == StatusCode::SERVICE_UNAVAILABLE);
        }
        assert!(TcpStream::connect(address).await.is_err());
    }

    #[tokio::test]
    async fn drop_stops_sse_and_client_disconnect_drops_upstream_body() {
        for drop_lease in [true, false] {
            let cancelled = Arc::new(Notify::new());
            let cancelled_fetch = cancelled.clone();
            let lease = lease(Arc::new(move |_| {
                let guard = DropSignal(cancelled_fetch.clone());
                Box::pin(async move {
                    let first = futures::stream::once(async { Ok(b"data: {\"ok\":true}\n\n".to_vec()) });
                    let pending = futures::stream::unfold(guard, |guard| async move {
                        let _guard = guard;
                        std::future::pending::<Option<(Result<Vec<u8>, Fault>, DropSignal)>>().await
                    });
                    Ok(UpstreamResponse {
                        status: StatusCode::OK,
                        headers: reply(StatusCode::OK, "text/event-stream", vec![]).headers,
                        body: first.chain(pending).boxed(),
                    })
                })
            }))
            .await;
            let mut response = client().get(&lease.url).header(header::AUTHORIZATION, &lease.authorization).send().await.unwrap();
            let first = response.chunk().await.unwrap().unwrap();
            assert!(String::from_utf8_lossy(&first).contains("data: {\"ok\":true}"));
            if drop_lease {
                drop(lease);
                let _ = tokio::time::timeout(TEST_WAIT, response.bytes()).await.unwrap();
            } else {
                drop(response);
            }
            tokio::time::timeout(TEST_WAIT, cancelled.notified()).await.unwrap();
        }
    }

    #[tokio::test]
    async fn partial_header_connection_is_closed_by_lease_drop() {
        let lease = lease(Arc::new(|_| Box::pin(async { Err(Fault::Transport) }))).await;
        let address = Url::parse(&lease.url).unwrap().socket_addrs(|| None).unwrap()[0];
        let mut socket = TcpStream::connect(address).await.unwrap();
        socket.write_all(b"POST /mcp HTTP/1.1\r\nHost:").await.unwrap();
        tokio::task::yield_now().await;
        drop(lease);
        let mut byte = [0u8; 1];
        let read = tokio::time::timeout(TEST_WAIT, socket.read(&mut byte)).await.unwrap();
        assert!(matches!(read, Ok(0) | Err(_)));
    }

    #[tokio::test]
    async fn drop_cancels_sse_even_when_consumer_stops_reading() {
        let cancelled = Arc::new(Notify::new());
        let produced = Arc::new(AtomicUsize::new(0));
        let progressed = Arc::new(Notify::new());
        let cancelled_fetch = cancelled.clone();
        let produced_fetch = produced.clone();
        let progressed_fetch = progressed.clone();
        let lease = lease(Arc::new(move |_| {
            let guard = DropSignal(cancelled_fetch.clone());
            let produced = produced_fetch.clone();
            let progressed = progressed_fetch.clone();
            let event = format!("data: {{\"text\":\"{}\"}}\n\n", "a".repeat(32 * 1024)).into_bytes();
            Box::pin(async move {
                let body = futures::stream::poll_fn(move |_| {
                    let _keep = &guard;
                    produced.fetch_add(1, Ordering::SeqCst);
                    progressed.notify_one();
                    Poll::Ready(Some(Ok(event.clone())))
                })
                .boxed();
                Ok(UpstreamResponse { status: StatusCode::OK, headers: reply(StatusCode::OK, "text/event-stream", vec![]).headers, body })
            })
        }))
        .await;
        let remote = Url::parse(&lease.url).unwrap();
        let address = remote.socket_addrs(|| None).unwrap()[0];
        let mut socket = TcpStream::connect(address).await.unwrap();
        let request = format!("GET /mcp HTTP/1.1\r\nHost: {address}\r\nAuthorization: {}\r\nAccept: text/event-stream\r\n\r\n", lease.authorization);
        socket.write_all(request.as_bytes()).await.unwrap();
        // Read headers only. Then wait until the producer stops advancing, demonstrating actual
        // socket/body backpressure instead of merely dropping an actively polled response.
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0u8; 1];
            assert!(tokio::time::timeout(TEST_WAIT, socket.read(&mut byte)).await.unwrap().unwrap() == 1);
            head.push(byte[0]);
            assert!(head.len() < 4096);
        }
        tokio::time::timeout(TEST_WAIT, async {
            loop {
                let before = produced.load(Ordering::SeqCst);
                if tokio::time::timeout(Duration::from_millis(100), progressed.notified()).await.is_err() && before == produced.load(Ordering::SeqCst) {
                    assert!(before > 0);
                    break;
                }
            }
        })
        .await
        .unwrap();
        drop(lease);
        let dropped = tokio::time::timeout(TEST_WAIT, cancelled.notified()).await;
        drop(socket);
        assert!(dropped.is_ok());
    }

    #[tokio::test]
    async fn sse_is_pull_driven_and_does_not_spawn_an_eager_reader() {
        let polls = Arc::new(AtomicUsize::new(0));
        let seen = polls.clone();
        let body = futures::stream::poll_fn(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Some(Ok(b"data: {\"ok\":true}\n\n".to_vec())))
        })
        .boxed();
        let (_stop, receiver) = watch::channel(false);
        let permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let mut stream = sse_stream(body, redactor(), receiver, permit);
        assert!(polls.load(Ordering::SeqCst) == 0);
        assert!(stream.next().await.is_some());
        assert!(polls.load(Ordering::SeqCst) == 1);
        tokio::task::yield_now().await;
        assert!(polls.load(Ordering::SeqCst) == 1);
        drop(stream);
    }
}
