//! A disposable MCP client: discovery only, no model hooks and no persisted configuration.
//!
//! rmcp logs peer-controlled messages, including errors and server metadata. Run its tasks on a
//! private current-thread runtime with tracing disabled, rather than risking credential echoes in
//! application logs. Dropping the caller closes the result channel; the worker then kills and
//! reaps its stdio process group before leaving. The worker itself is bounded by a 10-second budget.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{StreamExt, stream::BoxStream};
use grok_bot_model::{HttpError, HttpResult, McpServerConfig};
use grok_bot_store::vault::ResolvedMcpServer;
use reqwest_mcp::header::{HeaderMap, HeaderName, HeaderValue};
use rmcp::model::{ClientCapabilities, ClientConfig, ClientJsonRpcMessage, Implementation, PaginatedRequestParams, ProtocolVersion, ServerJsonRpcMessage};
use rmcp::service::{ClientLifecycleMode, RunningService};
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::transport::streamable_http_client::{StreamableHttpClient, StreamableHttpClientTransportConfig, StreamableHttpError, StreamableHttpPostResponse};
use rmcp::{ClientServiceExt, RoleClient, transport::StreamableHttpClientTransport};
use sse_stream::{Sse, SseStream};
use tokio::process::{Child, Command};

pub(crate) const STOPPED: &str = "MCP 테스트 중단: 서버가 종료 중이에요.";
const BUDGET: Duration = Duration::from_secs(10);
const DISCOVER_BUDGET: Duration = Duration::from_secs(3);
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_TOOL_PAGES: usize = 100;
type Client = RunningService<RoleClient, ClientConfig>;

#[derive(Clone, Copy, Debug)]
#[repr(u8)]
enum Failure {
    Connection = 1,
    Authentication,
    Protocol,
    Tools,
    Timeout,
    Process,
    ResponseLimit,
}

impl Failure {
    fn message(self) -> &'static str {
        match self {
            Self::Connection => "MCP 연결 오류: 서버 주소와 네트워크 상태를 확인해 주세요.",
            Self::Authentication => "MCP 인증 오류: 시크릿과 인증 헤더를 확인해 주세요.",
            Self::Protocol => "MCP 초기화 오류: 지원하는 MCP 서버인지 확인해 주세요.",
            Self::Tools => "MCP 도구 조회 오류: 도구 목록을 가져오지 못했어요.",
            Self::Timeout => "MCP 시간 초과: 10초 안에 연결과 도구 조회를 마치지 못했어요.",
            Self::Process => "MCP 실행 오류: 서버 명령과 실행 환경을 확인해 주세요.",
            Self::ResponseLimit => "MCP 응답 오류: 서버 응답이 테스트 허용 크기를 초과했어요.",
        }
    }

    fn http(self) -> HttpError {
        HttpError::new(if matches!(self, Self::Timeout) { 504 } else { 502 }, self.message())
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}
impl std::error::Error for Failure {}

/// At most one disposable probe is admitted per backend. The slot belongs to the WORKER, not
/// its HTTP future: disconnected callers cannot release admission before child cleanup completes.
/// There is no waiting list; repeated requests get a prompt 429 rather than a hidden 10s queue.
#[derive(Clone)]
pub(crate) struct Probes {
    state: Arc<Mutex<ProbeState>>,
    stopping: tokio::sync::watch::Sender<bool>,
}

#[derive(Default)]
struct ProbeState {
    closed: bool,
    active: Option<tokio::sync::watch::Receiver<bool>>,
}

impl Default for Probes {
    fn default() -> Self {
        Self { state: Arc::default(), stopping: tokio::sync::watch::Sender::new(false) }
    }
}

impl Probes {
    pub(crate) fn admit(&self) -> HttpResult<Admission> {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closed {
            return Err(HttpError::unavailable(STOPPED));
        }
        if state.active.is_some() {
            return Err(HttpError::new(429, "MCP 테스트 사용 중: 진행 중인 연결 테스트가 끝난 뒤 다시 시도해 주세요."));
        }
        let (done, finished) = tokio::sync::watch::channel(false);
        state.active = Some(finished);
        Ok(Admission { probes: self.clone(), done })
    }

    pub(crate) fn stop(&self) {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).closed = true;
        self.stopping.send_replace(true);
    }

    /// Called independently of the app command queue, including when a disconnected HTTP future
    /// has already been dropped. Shutdown must wait for the worker's kill + wait acknowledgement.
    pub(crate) async fn wait(&self) {
        let active = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).active.clone();
        if let Some(mut active) = active {
            let _ = active.wait_for(|done| *done).await;
        }
    }
}

pub(crate) struct Admission {
    probes: Probes,
    done: tokio::sync::watch::Sender<bool>,
}

impl Admission {
    pub(crate) fn check_open(&self) -> HttpResult<()> {
        if self.probes.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).closed { Err(HttpError::unavailable(STOPPED)) } else { Ok(()) }
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.probes.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).active = None;
        self.done.send_replace(true);
    }
}

/// Never move the store, a model client, or the command queue into this worker.
pub(crate) async fn test(server: ResolvedMcpServer, launch: crate::runtime::McpLaunchContext, admission: Admission) -> HttpResult<usize> {
    admission.check_open()?;
    let mut stopping = admission.probes.stopping.subscribe();
    let (mut result, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("mcp-draft-test".into())
        .spawn(move || {
            let _quiet = tracing::subscriber::set_default(tracing::subscriber::NoSubscriber::default());
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
                drop(admission);
                let _ = result.send(Err(Failure::Connection.http()));
                return;
            };
            let outcome = runtime.block_on(async {
                let mut child = None;
                let outcome = tokio::select! {
                    biased;
                    _ = result.closed() => None,
                    // Current-value-aware for late subscribers; acknowledgement follows cleanup.
                    _ = stopping.wait_for(|stopping| *stopping) => Some(Err(HttpError::unavailable(STOPPED))),
                    outcome = tokio::time::timeout(BUDGET, discover(server.config, launch, &mut child)) => {
                        Some(outcome.unwrap_or(Err(Failure::Timeout)).map_err(Failure::http))
                    }
                };
                if let Some(mut child) = child {
                    child.stop().await;
                }
                outcome
            });
            // End SDK transport tasks before releasing admission or acknowledging shutdown.
            drop(runtime);
            drop(admission);
            if let Some(outcome) = outcome {
                let _ = result.send(outcome);
            }
        })
        .map_err(|_| Failure::Connection.http())?;
    receiver.await.unwrap_or_else(|_| Err(Failure::Connection.http()))
}

fn client_info() -> ClientConfig {
    ClientConfig::new(ClientCapabilities::default(), Implementation::new("grok-bot-connection-test", env!("CARGO_PKG_VERSION")))
        // Match the runtime's initialize-era pin, not rmcp's modern default.
        .with_protocol_version(ProtocolVersion::V_2025_11_25)
}

async fn discover(config: McpServerConfig, launch: crate::runtime::McpLaunchContext, child: &mut Option<ProbeChild>) -> Result<usize, Failure> {
    let failure = Arc::new(AtomicU8::new(0));
    let service = match config {
        McpServerConfig::Stdio { command, args, env } => {
            let mut command = Command::new(command);
            launch.apply(&mut command, &env);
            command.args(args).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).kill_on_drop(true);
            #[cfg(unix)]
            command.process_group(0);
            *child = Some(ProbeChild::new(command.spawn().map_err(|_| Failure::Process)?));
            let process = &mut child.as_mut().expect("spawned child").child;
            let stdout = process.stdout.take().ok_or(Failure::Process)?;
            let stdin = process.stdin.take().ok_or(Failure::Process)?;
            // As in runtime startup, never probe server/discover on a non-rebuildable byte stream.
            client_info()
                .serve_with_lifecycle(AsyncRwTransport::new_client(stdout, stdin).with_max_line_length(MAX_RESPONSE_BYTES), ClientLifecycleMode::Initialize)
                .await
                .map_err(|_| Failure::Protocol)?
        }
        McpServerConfig::Http { url, headers } => {
            let mut defaults = HeaderMap::new();
            for pair in headers {
                let name = HeaderName::from_bytes(pair.name.as_bytes()).map_err(|_| Failure::Protocol)?;
                let mut value = HeaderValue::from_str(&pair.value).map_err(|_| Failure::Protocol)?;
                value.set_sensitive(true);
                defaults.insert(name, value);
            }
            let client = ProbeHttp {
                client: reqwest_mcp::Client::builder()
                    .default_headers(defaults)
                    .redirect(reqwest_mcp::redirect::Policy::none())
                    .connect_timeout(Duration::from_secs(3))
                    .timeout(BUDGET)
                    .pool_max_idle_per_host(0)
                    .build()
                    .map_err(|_| Failure::Connection)?,
                failure: failure.clone(),
            };
            let transport = || {
                StreamableHttpClientTransport::with_client(
                    client.clone(),
                    StreamableHttpClientTransportConfig::with_uri(url.as_str())
                        .reinit_on_expired_session(false)
                        .max_concurrent_requests(1)
                        .max_sse_event_size(MAX_RESPONSE_BYTES),
                )
            };
            // Match runtime compatibility: modern HTTP first, then initialize on a FRESH session.
            // A swallowed discovery gets only part of the single total startup/listing budget.
            match tokio::time::timeout(
                DISCOVER_BUDGET,
                client_info().serve_with_lifecycle(transport(), ClientLifecycleMode::Discover { preferred_versions: vec![ProtocolVersion::V_2026_07_28] }),
            )
            .await
            {
                Ok(Ok(service)) => service,
                _ if matches!(http_failure(&failure, Failure::Protocol), Failure::Authentication | Failure::ResponseLimit | Failure::Connection) => {
                    return Err(http_failure(&failure, Failure::Protocol));
                }
                _ => {
                    failure.store(0, Ordering::Relaxed);
                    client_info()
                        .serve_with_lifecycle(transport(), ClientLifecycleMode::Initialize)
                        .await
                        .map_err(|_| http_failure(&failure, Failure::Protocol))?
                }
            }
        }
    };
    count_tools(service, failure).await
}

async fn count_tools(mut service: Client, failure: Arc<AtomicU8>) -> Result<usize, Failure> {
    let result = async {
        // A server can be healthy while exposing only resources/prompts. No tools capability
        // means zero tools, not a failed connection (nor an unsupported tools/list request).
        if service.peer_info().is_some_and(|info| info.capabilities.tools.is_none()) {
            return Ok(0);
        }
        let mut cursor = None;
        let mut seen = HashSet::new();
        let mut count = 0usize;
        for _ in 0..MAX_TOOL_PAGES {
            let page =
                service.list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor))).await.map_err(|_| http_failure(&failure, Failure::Tools))?;
            count = count.checked_add(page.tools.len()).ok_or(Failure::ResponseLimit)?;
            match page.next_cursor {
                None => return Ok(count),
                Some(next) if seen.insert(next.clone()) => cursor = Some(next),
                Some(_) => return Err(Failure::Tools),
            }
        }
        Err(Failure::ResponseLimit)
    }
    .await;
    let _ = service.close_with_timeout(Duration::from_millis(200)).await;
    result
}

struct ProbeChild {
    child: Child,
    #[cfg(unix)]
    group: Option<i32>,
}

impl ProbeChild {
    fn new(child: Child) -> Self {
        Self {
            #[cfg(unix)]
            group: child.id().and_then(|id| i32::try_from(id).ok()),
            child,
        }
    }

    fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group {
            // SAFETY: this is the positive PID captured from our child, started in its own group.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
        let _ = self.child.start_kill();
    }

    async fn stop(&mut self) {
        self.kill();
        let _ = self.child.wait().await;
        #[cfg(unix)]
        {
            self.group = None;
        }
    }
}

impl Drop for ProbeChild {
    fn drop(&mut self) {
        self.kill();
    }
}

/// SDK's stock reqwest backend buffers JSON bodies without a size limit and retains peer text in
/// errors. This small adapter keeps its real transport/session lifecycle, but bounds every JSON
/// or SSE response (including chunked responses) and creates body/URL-free transport errors.
#[derive(Clone)]
struct ProbeHttp {
    client: reqwest_mcp::Client,
    failure: Arc<AtomicU8>,
}

type TransportError = StreamableHttpError<Failure>;

fn http_failure(failure: &AtomicU8, fallback: Failure) -> Failure {
    match failure.load(Ordering::Relaxed) {
        1 => Failure::Connection,
        2 => Failure::Authentication,
        5 => Failure::Timeout,
        7 => Failure::ResponseLimit,
        _ => fallback,
    }
}

impl ProbeHttp {
    fn fail(&self, failure: Failure) -> TransportError {
        self.failure.store(failure as u8, Ordering::Relaxed);
        StreamableHttpError::Client(failure)
    }

    fn request(
        &self,
        method: reqwest_mcp::Method,
        uri: &str,
        session: Option<&str>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> reqwest_mcp::RequestBuilder {
        let mut request = self.client.request(method, uri).header("accept", "application/json, text/event-stream").headers(headers.into_iter().collect());
        if let Some(session) = session {
            request = request.header("mcp-session-id", session);
        }
        if let Some(auth) = auth {
            request = request.bearer_auth(auth);
        }
        request
    }

    async fn send(&self, request: reqwest_mcp::RequestBuilder) -> Result<reqwest_mcp::Response, TransportError> {
        request.send().await.map_err(|error| self.fail(if error.is_timeout() { Failure::Timeout } else { Failure::Connection }))
    }

    fn status(&self, response: &reqwest_mcp::Response) -> Result<(), TransportError> {
        if response.status().is_success() {
            return Ok(());
        }
        Err(self.fail(if matches!(response.status().as_u16(), 401 | 403) {
            Failure::Authentication
        } else if response.status().is_redirection() {
            Failure::Connection
        } else {
            Failure::Protocol
        }))
    }

    fn bytes(&self, response: reqwest_mcp::Response) -> BoxStream<'static, Result<axum::body::Bytes, Failure>> {
        let failure = self.failure.clone();
        response
            .bytes_stream()
            .scan(0usize, move |size, chunk| {
                let chunk = chunk.map_err(|error| if error.is_timeout() { Failure::Timeout } else { Failure::Connection }).and_then(|chunk| {
                    *size = size.saturating_add(chunk.len());
                    if *size > MAX_RESPONSE_BYTES { Err(Failure::ResponseLimit) } else { Ok(chunk) }
                });
                if let Err(error) = &chunk {
                    failure.store(*error as u8, Ordering::Relaxed);
                }
                std::future::ready(Some(chunk))
            })
            .boxed()
    }

    fn events(&self, response: reqwest_mcp::Response) -> BoxStream<'static, Result<Sse, sse_stream::Error>> {
        SseStream::from_bytes_stream(self.bytes(response)).boxed()
    }
}

impl StreamableHttpClient for ProbeHttp {
    type Error = Failure;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, TransportError> {
        let response = self.send(self.request(reqwest_mcp::Method::POST, &uri, session.as_deref(), auth, headers).json(&message)).await?;
        // Legacy HTTP middleware often rejects server/discover with a body-free 400/404/405.
        // Correlate that rejection so the SDK can fall back; never retain its response body.
        if session.is_none()
            && response.status().is_client_error()
            && !matches!(response.status().as_u16(), 401 | 403)
            && let ClientJsonRpcMessage::Request(request) = &message
            && matches!(request.request, rmcp::model::ClientRequest::DiscoverRequest(_))
        {
            return Ok(StreamableHttpPostResponse::Json(
                ServerJsonRpcMessage::error(rmcp::model::ErrorData::method_not_found::<rmcp::model::DiscoverRequestMethod>(), Some(request.id.clone())),
                None,
            ));
        }
        self.status(&response)?;
        if matches!(response.status().as_u16(), 202 | 204) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        let session = response.headers().get("mcp-session-id").and_then(|value| value.to_str().ok()).map(str::to_owned);
        let content_type = response.headers().get("content-type").and_then(|value| value.to_str().ok()).unwrap_or("");
        if content_type.starts_with("text/event-stream") {
            return Ok(StreamableHttpPostResponse::Sse(self.events(response), session));
        }
        let is_request = matches!(message, ClientJsonRpcMessage::Request(_));
        if !is_request && response.content_length() == Some(0) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if !content_type.starts_with("application/json") {
            return Err(self.fail(Failure::Protocol));
        }
        if response.content_length().is_some_and(|size| size > MAX_RESPONSE_BYTES as u64) {
            return Err(self.fail(Failure::ResponseLimit));
        }
        let mut stream = self.bytes(response);
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            body.extend_from_slice(&chunk.map_err(|error| self.fail(error))?);
        }
        match serde_json::from_slice::<ServerJsonRpcMessage>(&body) {
            Ok(message) => Ok(StreamableHttpPostResponse::Json(message, session)),
            Err(_) if !is_request => Ok(StreamableHttpPostResponse::Accepted),
            Err(_) => Err(self.fail(Failure::Protocol)),
        }
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session: Option<Arc<str>>,
        last_event: Option<String>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, sse_stream::Error>>, TransportError> {
        let mut request = self.request(reqwest_mcp::Method::GET, &uri, session.as_deref(), auth, headers);
        if let Some(last_event) = last_event {
            request = request.header("last-event-id", last_event);
        }
        let response = self.send(request).await?;
        if response.status().as_u16() == 405 {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        self.status(&response)?;
        if !response.headers().get("content-type").is_some_and(|value| value.as_bytes().starts_with(b"text/event-stream")) {
            return Err(self.fail(Failure::Protocol));
        }
        Ok(self.events(response))
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session: Arc<str>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), TransportError> {
        let response = self.send(self.request(reqwest_mcp::Method::DELETE, &uri, Some(&session), auth, headers)).await?;
        if response.status().as_u16() == 405 {
            return Ok(());
        }
        self.status(&response)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::Path;
    use tokio::time::{Instant, timeout};

    fn hanging_server(marker: &Path, disconnect: bool) -> ResolvedMcpServer {
        let script = r#"
const fs = require('fs'), cp = require('child_process');
const child = cp.spawn(process.execPath, ['-e', 'setInterval(()=>{},1000)'], {stdio:'ignore'});
fs.writeFileSync(process.argv[1], JSON.stringify({pid:process.pid, child:child.pid}));
if (process.argv[2] === 'disconnect') {
  require('readline').createInterface({input:process.stdin}).on('line', line => {
    const req = JSON.parse(line);
    if (req.method === 'initialize') console.log(JSON.stringify({jsonrpc:'2.0',id:req.id,result:{protocolVersion:'2025-11-25',capabilities:{tools:{}},serverInfo:{name:'peer',version:'1'}}}));
    if (req.method === 'tools/list') process.exit(9);
  });
}
setInterval(()=>{},1000);
"#;
        ResolvedMcpServer {
            name: "cleanup".into(),
            config: McpServerConfig::Stdio {
                command: "node".into(),
                args: vec!["-e".into(), script.into(), marker.to_string_lossy().into_owned(), if disconnect { "disconnect" } else { "hang" }.into()],
                env: Vec::new(),
            },
        }
    }

    async fn admitted_probe(probes: Probes, server: ResolvedMcpServer) -> HttpResult<usize> {
        let admission = probes.admit()?;
        test(server, crate::runtime::McpLaunchContext::capture()?, admission).await
    }

    async fn probe(server: ResolvedMcpServer) -> HttpResult<usize> {
        admitted_probe(Probes::default(), server).await
    }

    async fn process_ids(marker: &Path) -> (i32, i32) {
        timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(bytes) = tokio::fs::read(marker).await
                    && let Ok(record) = serde_json::from_slice::<serde_json::Value>(&bytes)
                {
                    return (record["pid"].as_i64().unwrap() as i32, record["child"].as_i64().unwrap() as i32);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("stdio fixture starts")
    }

    async fn assert_stopped(pid: i32, child: i32) {
        timeout(Duration::from_secs(3), async {
            loop {
                // SAFETY: signal 0 probes only the PID our test fixture recorded.
                let reaped = unsafe { libc::kill(pid, 0) } == -1;
                let descendant = tokio::fs::read_to_string(format!("/proc/{child}/stat")).await.unwrap_or_default();
                if reaped && (descendant.is_empty() || descendant.split_whitespace().nth(2) == Some("Z")) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("direct child is killed and reaped; process-group descendant is not running");
    }

    #[tokio::test]
    async fn cancelled_probe_kills_and_reaps_its_stdio_process_group() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("cancel.json");
        let task = tokio::spawn(probe(hanging_server(&marker, false)));
        let (pid, child) = process_ids(&marker).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_stopped(pid, child).await;
    }

    #[tokio::test]
    async fn shutdown_tracks_a_worker_after_its_http_future_is_dropped() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("dropped.json");
        let probes = Probes::default();
        let task = tokio::spawn(admitted_probe(probes.clone(), hanging_server(&marker, false)));
        let (pid, child) = process_ids(&marker).await;
        assert_eq!(probes.admit().err().unwrap().status, 429);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        probes.stop();
        timeout(Duration::from_secs(3), probes.wait()).await.expect("shutdown tracks cleanup independently of HTTP future");
        // SAFETY: signal 0 probes this fixture's direct child after the tracker acknowledged wait.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_stopped(pid, child).await;
        assert_eq!(probes.admit().err().unwrap().status, 503);
    }

    #[tokio::test]
    async fn timed_out_probe_kills_and_reaps_its_stdio_process_group() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("timeout.json");
        let started = Instant::now();
        let error = timeout(Duration::from_secs(12), probe(hanging_server(&marker, false))).await.expect("bounded whole probe").unwrap_err();
        assert_eq!(error.status, 504);
        assert_eq!(error.message, Failure::Timeout.message());
        assert!(started.elapsed() < Duration::from_secs(12));
        let (pid, child) = process_ids(&marker).await;
        assert_stopped(pid, child).await;
    }

    #[tokio::test]
    async fn shutdown_probe_acknowledges_only_after_child_is_reaped() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("shutdown.json");
        let probes = Probes::default();
        let task = tokio::spawn(admitted_probe(probes.clone(), hanging_server(&marker, false)));
        let (pid, child) = process_ids(&marker).await;
        probes.stop();
        let error = timeout(Duration::from_secs(3), task).await.expect("shutdown interrupts startup").unwrap().unwrap_err();
        assert_eq!(error.status, 503);
        assert_eq!(error.message, STOPPED);
        // This is deliberately immediate: the response must acknowledge completed wait(), not
        // merely that the worker has been asked to stop.
        // SAFETY: signal 0 probes only the PID recorded by this test's own child.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "shutdown response follows child reaping");
        assert_stopped(pid, child).await;
    }

    #[tokio::test]
    async fn late_shutdown_subscriber_never_spawns_the_draft() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("never.json");
        let probes = Probes::default();
        let admission = probes.admit().unwrap();
        probes.stop();
        let error = test(hanging_server(&marker, false), crate::runtime::McpLaunchContext::capture().unwrap(), admission).await.unwrap_err();
        assert_eq!(error.status, 503);
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn disconnected_probe_kills_and_reaps_its_stdio_process_group() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("disconnect.json");
        let error = probe(hanging_server(&marker, true)).await.unwrap_err();
        assert_eq!(error.status, 502);
        assert_eq!(error.message, Failure::Tools.message());
        let (pid, child) = process_ids(&marker).await;
        assert_stopped(pid, child).await;
    }
}
