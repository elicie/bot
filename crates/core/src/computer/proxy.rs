//! Authenticated loopback forward proxy of the managed computer. Port of `ComputerProxy` in
//! `server/computer-network.ts`.
//!
//! Chrome is launched with this proxy as its only network path (`--host-resolver-rules` makes every
//! other name unresolvable). Each request needs the per-session Basic credentials, is limited to
//! read-only methods without a body, and resolves its destination through the policy; the proxy
//! connects to the validated address, so a later DNS change cannot redirect the connection.
//! Plain HTTP drops credentials, cookies, and downloads; HTTPS is a pinned CONNECT tunnel.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use base64::Engine as _;
use grok_bot_model::{HttpError, HttpResult};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::AbortHandle;

use super::network::Destination;

const MAX_HEAD_BYTES: usize = 64 * 1024;
const MAX_HEADERS: usize = 100;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const UPSTREAM_IDLE: Duration = Duration::from_secs(12);
const TUNNEL_IDLE: Duration = Duration::from_secs(30);
pub const DENIED_BODY: &str = "Network access was denied by the managed computer policy.";
const HOP_HEADERS: [&str; 11] = ["connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "proxy-connection", "te", "trailer", "transfer-encoding", "upgrade", "authorization", "cookie"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyConfiguration {
    pub server: String,
    pub username: String,
    pub password: String,
}

#[derive(Default)]
struct ProxyState {
    port: Option<u16>,
    stopped: bool,
    tasks: Vec<AbortHandle>,
}

pub struct ComputerProxy {
    destination: Arc<dyn Destination>,
    credentials: String,
    probe: String,
    state: Mutex<ProxyState>,
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).expect("the operating system provides randomness");
    hex::encode(buffer)
}

fn stopped_error() -> HttpError {
    HttpError::conflict("The computer proxy is stopped.")
}

struct Head {
    method: String,
    target: String,
    headers: Vec<(String, Vec<u8>)>,
    rest: Vec<u8>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_slice())
    }
}

/// Read until the end of an HTTP head; `None` when the peer closes or the head is too large.
async fn read_head<S: AsyncRead + Unpin>(stream: &mut S) -> Option<(Vec<u8>, usize)> {
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            return Some((buffer, end + 4));
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return None;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<Head> {
    let (buffer, end) = tokio::time::timeout(HEAD_TIMEOUT, read_head(stream)).await.ok()??;
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut request = httparse::Request::new(&mut headers);
    if !matches!(request.parse(&buffer[..end]), Ok(httparse::Status::Complete(_))) {
        return None;
    }
    Some(Head {
        method: request.method?.to_owned(),
        target: request.path?.to_owned(),
        headers: request.headers.iter().map(|header| (header.name.to_owned(), header.value.to_vec())).collect(),
        rest: buffer[end..].to_vec(),
    })
}

/// Headers forwarded across the proxy: hop-by-hop, credential, and cookie headers are removed,
/// including every header named by `Connection`.
fn safe_headers<'a>(headers: &'a [(String, Vec<u8>)], keep: &[&str]) -> Vec<&'a (String, Vec<u8>)> {
    let named: Vec<String> = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("connection"))
        .flat_map(|(_, value)| String::from_utf8_lossy(value).split(',').map(|item| item.trim().to_ascii_lowercase()).collect::<Vec<_>>())
        .collect();
    headers
        .iter()
        .filter(|(name, _)| {
            let lower = name.to_ascii_lowercase();
            keep.contains(&lower.as_str()) || (!HOP_HEADERS.contains(&lower.as_str()) && !named.contains(&lower))
        })
        .collect()
}

async fn respond(stream: &mut TcpStream, status: u16, reason: &str, extra: &str, body: &str) {
    let content = if body.is_empty() { String::new() } else { "Content-Type: text/plain; charset=utf-8\r\n".to_owned() };
    let response = format!("HTTP/1.1 {status} {reason}\r\n{extra}{content}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn reason(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        403 => "Forbidden",
        407 => "Proxy Authentication Required",
        409 => "Conflict",
        502 => "Bad Gateway",
        _ => "Error",
    }
}

/// Copy both directions until either side closes or nothing moves for `idle`.
async fn pipe_both<A, B>(mut client: A, mut upstream: B, idle: Duration)
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let mut from_client = [0u8; 16 * 1024];
    let mut from_upstream = [0u8; 16 * 1024];
    loop {
        tokio::select! {
            read = client.read(&mut from_client) => match read {
                Ok(0) | Err(_) => break,
                Ok(n) => if upstream.write_all(&from_client[..n]).await.is_err() { break },
            },
            read = upstream.read(&mut from_upstream) => match read {
                Ok(0) | Err(_) => break,
                Ok(n) => if client.write_all(&from_upstream[..n]).await.is_err() { break },
            },
            _ = tokio::time::sleep(idle) => break,
        }
    }
}

impl ComputerProxy {
    pub fn new(destination: Arc<dyn Destination>) -> Arc<Self> {
        Arc::new(Self { destination, credentials: random_hex(24), probe: random_hex(24), state: Mutex::new(ProxyState::default()) })
    }

    fn state(&self) -> MutexGuard<'_, ProxyState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn stopped(&self) -> bool {
        self.state().stopped
    }

    pub fn configuration(&self) -> HttpResult<ProxyConfiguration> {
        let state = self.state();
        match state.port {
            Some(port) if !state.stopped => Ok(ProxyConfiguration { server: format!("http://127.0.0.1:{port}"), username: "computer".into(), password: self.credentials.clone() }),
            _ => Err(stopped_error()),
        }
    }

    pub fn probe_url(&self) -> HttpResult<String> {
        Ok(format!("{}/proxy-check/{}", self.configuration()?.server, self.probe))
    }

    pub fn probe_body(&self) -> String {
        format!("managed-computer-proxy-{}", self.probe)
    }

    fn track(&self, handle: AbortHandle) {
        let mut state = self.state();
        state.tasks.retain(|task| !task.is_finished());
        if state.stopped {
            handle.abort();
        } else {
            state.tasks.push(handle);
        }
    }

    pub async fn start(self: &Arc<Self>) -> HttpResult<()> {
        if self.stopped() {
            return Err(stopped_error());
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|_| HttpError::unavailable("The computer proxy could not start."))?;
        let port = listener.local_addr().map_err(|_| HttpError::unavailable("The computer proxy could not start."))?.port();
        let proxy = Arc::clone(self);
        let accept = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let connection = Arc::clone(&proxy);
                let task = tokio::spawn(async move { connection.connection(socket).await });
                proxy.track(task.abort_handle());
            }
        });
        {
            let mut state = self.state();
            if state.stopped {
                accept.abort();
                return Err(stopped_error());
            }
            state.port = Some(port);
            state.tasks.push(accept.abort_handle());
        }
        Ok(())
    }

    /// Close the listener and every client and upstream connection.
    pub fn stop(&self) {
        let tasks = {
            let mut state = self.state();
            state.stopped = true;
            std::mem::take(&mut state.tasks)
        };
        for task in tasks {
            task.abort();
        }
    }

    fn authenticated(&self, head: &Head) -> bool {
        let expected = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("computer:{}", self.credentials)));
        let mut values = head.headers.iter().filter(|(name, _)| name.eq_ignore_ascii_case("proxy-authorization"));
        matches!((values.next(), values.next()), (Some((_, value)), None) if value.as_slice() == expected.as_bytes())
    }

    async fn connection(self: Arc<Self>, mut socket: TcpStream) {
        let Some(head) = read_request(&mut socket).await else { return };
        if head.method == "CONNECT" {
            self.tunnel(head, socket).await;
        } else {
            self.forward(head, socket).await;
        }
    }

    async fn forward(&self, head: Head, mut socket: TcpStream) {
        if !self.authenticated(&head) {
            respond(&mut socket, 407, reason(407), "Proxy-Authenticate: Basic realm=\"managed-computer\"\r\n", "").await;
            return;
        }
        match self.forward_checked(&head, &mut socket).await {
            Ok(()) => {}
            Err(Some(error)) => respond(&mut socket, error.status, reason(error.status), "", DENIED_BODY).await,
            Err(None) => {}
        }
    }

    /// `Err(Some)` answers with the denial page; `Err(None)` means the response already started.
    async fn forward_checked(&self, head: &Head, socket: &mut TcpStream) -> Result<(), Option<HttpError>> {
        if self.stopped() {
            return Err(Some(stopped_error()));
        }
        let length = head.header("content-length");
        if !matches!(head.method.as_str(), "GET" | "HEAD" | "OPTIONS") || head.header("upgrade").is_some() || length.is_some_and(|value| value != b"0") || head.header("transfer-encoding").is_some() {
            return Err(Some(HttpError::forbidden("Only public read-only browsing is allowed.")));
        }
        if head.method == "GET" && self.probe_url().is_ok_and(|probe| probe == head.target) {
            let body = self.probe_body();
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
            return Ok(());
        }
        let target = self.destination.resolve(&head.target).await.map_err(Some)?;
        if self.stopped() {
            return Err(Some(stopped_error()));
        }
        if target.url.scheme() != "http" {
            return Err(Some(HttpError::forbidden("HTTPS requires a pinned CONNECT tunnel.")));
        }
        let failed = || Some(HttpError::new(502, "The upstream request failed."));
        let mut upstream = match tokio::time::timeout(UPSTREAM_IDLE, TcpStream::connect((target.address, target.port))).await {
            Ok(Ok(upstream)) => upstream,
            _ => return Err(failed()),
        };
        let host = match target.url.port() {
            Some(port) => format!("{}:{port}", target.url.host_str().unwrap_or_default()),
            None => target.url.host_str().unwrap_or_default().to_owned(),
        };
        let path = match target.url.query() {
            Some(query) => format!("{}?{query}", target.url.path()),
            None => target.url.path().to_owned(),
        };
        let mut request = format!("{} {path} HTTP/1.1\r\n", head.method).into_bytes();
        for (name, value) in safe_headers(&head.headers, &[]) {
            if name.eq_ignore_ascii_case("host") {
                continue;
            }
            request.extend_from_slice(name.as_bytes());
            request.extend_from_slice(b": ");
            request.extend_from_slice(value);
            request.extend_from_slice(b"\r\n");
        }
        request.extend_from_slice(format!("host: {host}\r\nconnection: close\r\n\r\n").as_bytes());
        if upstream.write_all(&request).await.is_err() {
            return Err(failed());
        }
        let Ok(Some((buffer, end))) = tokio::time::timeout(UPSTREAM_IDLE, read_head(&mut upstream)).await else {
            return Err(failed());
        };
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut response = httparse::Response::new(&mut headers);
        if !matches!(response.parse(&buffer[..end]), Ok(httparse::Status::Complete(_))) {
            return Err(failed());
        }
        let status = response.code.unwrap_or(502);
        let reason_phrase: String = response.reason.unwrap_or("").chars().filter(|c| !c.is_control()).collect();
        let remote: Vec<(String, Vec<u8>)> = response.headers.iter().map(|header| (header.name.to_owned(), header.value.to_vec())).collect();
        let attachment = remote.iter().any(|(name, value)| name.eq_ignore_ascii_case("content-disposition") && String::from_utf8_lossy(value).to_ascii_lowercase().contains("attachment"));
        if attachment {
            drop(upstream);
            respond(socket, 403, reason(403), "", "Downloads are not allowed.").await;
            return Err(None);
        }
        // The body is relayed byte for byte, so its framing headers stay; the connection closes after it.
        let mut out = format!("HTTP/1.1 {status} {reason_phrase}\r\n").into_bytes();
        for (name, value) in safe_headers(&remote, &["transfer-encoding"]) {
            if name.eq_ignore_ascii_case("set-cookie") {
                continue;
            }
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(value);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"connection: close\r\n\r\n");
        out.extend_from_slice(&buffer[end..]);
        if socket.write_all(&out).await.is_err() {
            return Err(None);
        }
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match tokio::time::timeout(UPSTREAM_IDLE, upstream.read(&mut chunk)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    if socket.write_all(&chunk[..n]).await.is_err() {
                        return Err(None);
                    }
                }
                _ => return Err(None),
            }
        }
        let _ = socket.shutdown().await;
        Ok(())
    }

    async fn tunnel(&self, head: Head, mut socket: TcpStream) {
        if !self.authenticated(&head) {
            respond(&mut socket, 407, reason(407), "Proxy-Authenticate: Basic realm=\"managed-computer\"\r\n", "").await;
            return;
        }
        let forbidden = b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n";
        if !valid_authority(&head.target) {
            let _ = socket.write_all(forbidden).await;
            return;
        }
        let Ok(target) = self.destination.resolve(&format!("https://{}/", head.target)).await else {
            let _ = socket.write_all(forbidden).await;
            return;
        };
        if self.stopped() {
            return;
        }
        let Ok(Ok(mut upstream)) = tokio::time::timeout(TUNNEL_IDLE, TcpStream::connect((target.address, target.port))).await else {
            return;
        };
        if self.stopped() || socket.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.is_err() {
            return;
        }
        if !head.rest.is_empty() && upstream.write_all(&head.rest).await.is_err() {
            return;
        }
        pipe_both(socket, upstream, TUNNEL_IDLE).await;
    }
}

/// `/^(?:\[[0-9a-f:]+\]|[a-z0-9.-]+):[0-9]+$/i`
fn valid_authority(authority: &str) -> bool {
    let Some((host, port)) = authority.rsplit_once(':') else { return false };
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    match host.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
        Some(inner) => !inner.is_empty() && inner.bytes().all(|b| b.is_ascii_hexdigit() || b == b':'),
        None => !host.is_empty() && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'),
    }
}

#[cfg(test)]
pub(super) fn authority_is_valid(authority: &str) -> bool {
    valid_authority(authority)
}
