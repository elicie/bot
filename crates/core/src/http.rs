//! HTTP layer. Port of `server/app.ts`: Host/Origin validation, security headers, same-origin JSON
//! mutations, the `/api` routes, SSE state events, the internal MCP broker route, and static files.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, FromRequest, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::{self, Stream, StreamExt};
use grok_bot_model::*;
use grok_bot_store::Store;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::public_state::{project_computer_permissions, project_response};
use crate::queue::CommandQueue;
use crate::runtime::Runtime;
use crate::scheduler::RoutineScheduler;

const MAX_JSON_BYTES: usize = 2 * 1024 * 1024;

pub struct HttpContext {
    pub store: Arc<Store>,
    pub runtime: Arc<Runtime>,
    pub scheduler: Arc<RoutineScheduler>,
    pub queue: CommandQueue,
    pub allowed_hosts: HashSet<String>,
    /// Bound port, set once the listener is open; Host must name this port.
    pub port: AtomicU16,
    pub closing: AtomicBool,
    /// Built frontend (`dist/`). `None` serves the API only.
    pub web_root: Option<PathBuf>,
    /// Flips to `true` on shutdown; open SSE streams end so graceful shutdown can finish.
    pub stopping: tokio::sync::watch::Sender<bool>,
}

type Ctx = Arc<HttpContext>;

pub fn normalize_host(value: &str) -> String {
    value.to_ascii_lowercase().trim_end_matches('.').to_owned()
}

pub fn default_allowed_hosts(extra: &[String]) -> HashSet<String> {
    ["localhost", "127.0.0.1", "[::1]"].iter().map(|h| normalize_host(h)).chain(extra.iter().map(|h| normalize_host(h))).collect()
}

/// Errors returned to clients as `{ "error": message }`.
pub struct ApiError(pub HttpError);

impl From<HttpError> for ApiError {
    fn from(error: HttpError) -> Self {
        Self(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(json!({ "error": self.0.message }))).into_response()
    }
}

type ApiResult<T = Response> = Result<T, ApiError>;

fn bad_request(message: impl Into<String>) -> ApiError {
    ApiError(HttpError::bad_request(message))
}

/// JSON body with the Node limits and messages: 2 MiB (413), syntax errors (400), schema errors (400).
pub struct JsonBody<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for JsonBody<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = match Bytes::from_request(request, state).await {
            Ok(bytes) => bytes,
            Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                return Err(ApiError(HttpError::too_large("JSON request exceeds the 2 MiB limit.")));
            }
            Err(_) => return Err(bad_request("Request body must be valid JSON.")),
        };
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| bad_request("Request body must be valid JSON."))?;
        serde_json::from_value(value).map(JsonBody).map_err(|error| bad_request(format!("Request: {error}").chars().take(1000).collect::<String>()))
    }
}

/// `{}` bodies (`z.object({}).strict()`).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

fn host_error(ctx: &HttpContext, headers: &HeaderMap) -> Option<&'static str> {
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return Some("Host is not allowed.");
    };
    if host.is_empty() || host.chars().any(|c| c.is_whitespace() || matches!(c, '/' | '@' | '?' | '#' | '\\')) {
        return Some("Host is not allowed.");
    }
    let Some((name, port)) = split_host(host) else {
        return Some("Host is not allowed.");
    };
    if !ctx.allowed_hosts.contains(&normalize_host(&name)) {
        return Some("Host is not allowed.");
    }
    let bound = ctx.port.load(Ordering::Relaxed);
    if bound != 0 && port.unwrap_or(80) != bound {
        return Some("Host does not match this server.");
    }
    if let Some(origin) = headers.get(header::ORIGIN) {
        let expected = format!("http://{}", origin_authority(&name, port));
        if origin.to_str().ok() != Some(expected.as_str()) {
            return Some("Cross-origin requests are not accepted.");
        }
    }
    None
}

/// Split `host[:port]` like WHATWG URL parsing does for `http://${host}`; `None` when invalid.
fn split_host(host: &str) -> Option<(String, Option<u16>)> {
    let (name, port) = if let Some(rest) = host.strip_prefix('[') {
        let end = rest.find(']')?;
        let name = format!("[{}]", &rest[..end]);
        let tail = &rest[end + 1..];
        let port = match tail.strip_prefix(':') {
            Some(p) => Some(p),
            None if tail.is_empty() => None,
            None => return None,
        };
        (name, port)
    } else {
        match host.rsplit_once(':') {
            Some((name, port)) => (name.to_owned(), Some(port)),
            None => (host.to_owned(), None),
        }
    };
    if name.is_empty() {
        return None;
    }
    let port = match port {
        None | Some("") => None,
        Some(p) => Some(p.parse::<u16>().ok()?),
    };
    Some((name, port))
}

fn origin_authority(name: &str, port: Option<u16>) -> String {
    let name = name.to_ascii_lowercase();
    match port {
        Some(p) if p != 80 => format!("{name}:{p}"),
        _ => name,
    }
}

fn is_local(remote: &SocketAddr) -> bool {
    match remote.ip() {
        std::net::IpAddr::V4(ip) => ip.is_loopback() && ip == std::net::Ipv4Addr::LOCALHOST,
        std::net::IpAddr::V6(ip) => ip.is_loopback() || ip.to_ipv4_mapped() == Some(std::net::Ipv4Addr::LOCALHOST),
    }
}

async fn guard(State(ctx): State<Ctx>, request: Request, next: Next) -> Response {
    if let Some(error) = host_error(&ctx, request.headers()) {
        return (StatusCode::FORBIDDEN, Json(json!({ "error": error }))).into_response();
    }
    let api = request.uri().path().starts_with("/api");
    if api && !matches!(*request.method(), Method::GET | Method::HEAD) {
        let headers = request.headers();
        let json_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"));
        if headers.get("x-grok-bot").map(|v| v.as_bytes()) != Some(b"1") || headers.get(header::ORIGIN).is_none() || !json_type {
            return secure((StatusCode::FORBIDDEN, Json(json!({ "error": "Mutations require same-origin JSON and X-Grok-Bot: 1." }))).into_response(), api);
        }
        if ctx.closing.load(Ordering::Relaxed) {
            return secure((StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "The server is shutting down." }))).into_response(), api);
        }
    }
    secure(next.run(request).await, api)
}

fn secure(mut response: Response, api: bool) -> Response {
    let headers = response.headers_mut();
    headers.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert("cross-origin-resource-policy", HeaderValue::from_static("same-origin"));
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    if api {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

/// JSON response projected for non-local clients (computer approvals hidden).
fn reply(status: StatusCode, body: Value, remote: &SocketAddr) -> Response {
    (status, Json(project_response(body, is_local(remote)))).into_response()
}

fn state_reply(ctx: &HttpContext, status: StatusCode, remote: &SocketAddr) -> Response {
    reply(status, serde_json::to_value(ctx.store.snapshot()).unwrap_or(Value::Null), remote)
}

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

type Remote = ConnectInfo<SocketAddr>;

pub fn router(ctx: Ctx) -> Router {
    let api = Router::new()
        .route("/state", get(get_state))
        .route("/events", get(events))
        .route("/settings", axum::routing::put(put_settings))
        .route("/provider/test", post(provider_test))
        .route("/bots", post(add_bot))
        .route("/bots/{id}", axum::routing::patch(update_bot))
        .route("/routines", post(add_routine))
        .route("/routines/{id}", axum::routing::put(update_routine).patch(enable_routine).delete(delete_routine))
        .route("/routines/{id}/run", post(run_routine))
        .route("/bots/{bot_id}/skills/import", post(import_skill))
        .route("/bots/{bot_id}/skills", post(add_skill))
        .route("/bots/{bot_id}/skills/{id}", axum::routing::patch(update_skill).delete(delete_skill))
        .route("/conversations", post(add_conversation))
        .route("/conversations/{id}/messages", post(send_message))
        .route("/conversations/{id}/cancel", post(cancel))
        .route("/conversations/{id}/permissions/{permission_id}", post(resolve_permission))
        .route("/files", get(list_files))
        .route("/file", get(read_file).put(save_file))
        .route("/computer", get(computer_status).post(computer_disabled).delete(computer_status))
        .route("/computer/preview", get(computer_disabled))
        .route("/computer/actions", post(computer_disabled))
        .fallback(|| async { (StatusCode::NOT_FOUND, Json(json!({ "error": "API endpoint does not exist." }))) });
    let mut app = Router::new()
        .nest("/api", api)
        .route("/internal/mcp/{run_id}", post(mcp).get(|| async { StatusCode::METHOD_NOT_ALLOWED }));
    app = match ctx.web_root.clone() {
        Some(root) => {
            let index = root.join("index.html");
            let files = tower_http::services::ServeDir::new(&root).append_index_html_on_directories(false).fallback(tower::service_fn(move |request: Request| {
                let index = index.clone();
                async move { Ok::<_, std::convert::Infallible>(spa_fallback(request, index).await) }
            }));
            app.fallback_service(files)
        }
        None => app.fallback(|| async { StatusCode::NOT_FOUND }),
    };
    app.layer(middleware::from_fn_with_state(ctx.clone(), guard))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_JSON_BYTES))
        .with_state(ctx)
}

async fn spa_fallback(request: Request, index: PathBuf) -> Response {
    if request.uri().path().starts_with("/internal") || !matches!(*request.method(), Method::GET | Method::HEAD) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tokio::fs::read(&index).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], bytes).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, Json(json!({ "error": "File or directory does not exist. Build the frontend before production startup." }))).into_response(),
    }
}

async fn get_state(State(ctx): State<Ctx>, ConnectInfo(remote): Remote) -> Response {
    state_reply(&ctx, StatusCode::OK, &remote)
}

async fn events(State(ctx): State<Ctx>, ConnectInfo(remote): Remote) -> impl IntoResponse {
    let local = is_local(&remote);
    let first = ctx.store.snapshot();
    let receiver = ctx.store.subscribe();
    let updates = stream::unfold(receiver, |mut receiver| async move {
        // A lagging client is disconnected, like the Node 4 MiB backpressure cut-off; it reconnects
        // and receives a fresh snapshot.
        receiver.recv().await.ok().map(|state| ((*state).clone(), receiver))
    });
    let mut stopping = ctx.stopping.subscribe();
    let stopped = async move {
        let _ = stopping.wait_for(|stop| *stop).await;
    };
    let stream = stream::once(async move { first }).chain(updates).take_until(stopped).map(move |state| sse_event(state, local));
    let mut response = Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("heartbeat")).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

fn sse_event(state: AppState, local: bool) -> Result<Event, std::convert::Infallible> {
    let projected = project_computer_permissions(state, local);
    Ok(Event::default().event("state").data(serde_json::to_string(&projected).unwrap_or_default()))
}

#[allow(dead_code)]
fn _assert_stream<S: Stream>(_: &S) {}

async fn put_settings(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, JsonBody(input): JsonBody<SettingsInput>) -> ApiResult {
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|s| s.update_settings(input)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProviderTest {
    base_url: Option<String>,
    api_key: Option<String>,
}

async fn provider_test(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, JsonBody(input): JsonBody<ProviderTest>) -> ApiResult {
    if input.base_url.as_ref().is_some_and(|u| u.chars().count() > 2048) {
        return Err(bad_request("baseUrl: String must contain at most 2048 character(s)"));
    }
    if let Some(key) = &input.api_key
        && (key.chars().count() > 16384 || key.contains(['\r', '\n', '\0']))
    {
        return Err(bad_request("apiKey: API key contains invalid characters."));
    }
    let settings = ctx.store.read(|s| s.settings().clone());
    let requested = input.base_url.as_deref().map(str::trim).filter(|u| !u.is_empty()).unwrap_or(&settings.base_url).to_owned();
    let target = grok_bot_store::base_url(&requested)?;
    let same = target == settings.base_url;
    let key = input.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned).unwrap_or_else(|| if same { settings.api_key.clone() } else { String::new() });
    let model = if same { settings.model.clone() } else { String::new() };
    let result = crate::provider::check_provider(&target, &key, &model).await;
    Ok(reply(StatusCode::OK, to_value(&result), &remote))
}

async fn add_bot(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, JsonBody(input): JsonBody<BotInput>) -> ApiResult {
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|s| s.add_bot(input)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::CREATED, &remote))
}

async fn update_bot(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(input): JsonBody<BotPatch>) -> ApiResult {
    if input == BotPatch::default() {
        return Err(bad_request("Provide a bot field to update."));
    }
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|s| s.update_bot(&id, input)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

async fn add_routine(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, JsonBody(input): JsonBody<RoutineInput>) -> ApiResult {
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|s| s.add_routine(input)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::CREATED, &remote))
}

async fn update_routine(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(input): JsonBody<RoutineInput>) -> ApiResult {
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|s| s.update_routine(&id, input)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Enabled {
    enabled: bool,
}

async fn enable_routine(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(input): JsonBody<Enabled>) -> ApiResult {
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|s| s.set_routine_enabled(&id, input.enabled)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

async fn delete_routine(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(_): JsonBody<Empty>) -> ApiResult {
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|s| s.delete_routine(&id)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

async fn run_routine(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(_): JsonBody<Empty>) -> ApiResult {
    let run = ctx.scheduler.run(&id).await?;
    let state = ctx.store.snapshot();
    let run = state.routine_runs.iter().find(|r| r.id == run.id).cloned().unwrap_or(run);
    Ok(reply(StatusCode::ACCEPTED, json!({ "run": run, "state": state }), &remote))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillImport {
    markdown: String,
}

async fn import_skill(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(bot_id): Path<String>, JsonBody(input): JsonBody<SkillImport>) -> ApiResult {
    if input.markdown.chars().count() > 20 * 1024 {
        return Err(bad_request("markdown: String must contain at most 20480 character(s)"));
    }
    let skill = grok_bot_store::skills::import_skill_markdown(&input.markdown)?;
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|s| s.add_skill(&bot_id, skill, false)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::CREATED, &remote))
}

async fn add_skill(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(bot_id): Path<String>, JsonBody(input): JsonBody<SkillInput>) -> ApiResult {
    let input = grok_bot_store::skills::validate_skill_input(input)?;
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|s| s.add_skill(&bot_id, input, false)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::CREATED, &remote))
}

async fn update_skill(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path((bot_id, id)): Path<(String, String)>, JsonBody(input): JsonBody<SkillPatch>) -> ApiResult {
    if input.revision < 1 {
        return Err(bad_request("revision: Number must be greater than or equal to 1"));
    }
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|s| s.update_skill(&bot_id, &id, input)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revision {
    revision: u64,
}

async fn delete_skill(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path((bot_id, id)): Path<(String, String)>, JsonBody(input): JsonBody<Revision>) -> ApiResult {
    if input.revision < 1 {
        return Err(bad_request("revision: Number must be greater than or equal to 1"));
    }
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|s| s.delete_skill(&bot_id, &id, input.revision)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct NewConversation {
    bot_id: String,
}

async fn add_conversation(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, JsonBody(input): JsonBody<NewConversation>) -> ApiResult {
    if uuid::Uuid::parse_str(&input.bot_id).is_err() {
        return Err(bad_request("botId: Invalid uuid"));
    }
    let store = ctx.store.clone();
    let conversation = ctx.queue.run(async move { store.mutate(|s| s.add_conversation(&input.bot_id)).await }).await?;
    Ok(reply(StatusCode::CREATED, json!({ "conversation": conversation, "state": ctx.store.snapshot() }), &remote))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prompt {
    text: String,
}

async fn send_message(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(input): JsonBody<Prompt>) -> ApiResult {
    let text = input.text.trim().to_owned();
    if text.is_empty() {
        return Err(bad_request("text: String must contain at least 1 character(s)"));
    }
    if text.chars().count() > 100000 {
        return Err(bad_request("text: String must contain at most 100000 character(s)"));
    }
    let (store, runtime) = (ctx.store.clone(), ctx.runtime.clone());
    ctx.queue
        .run(async move {
            if store.read(|s| s.is_reserved_conversation(&id)) {
                return Err(HttpError::conflict("This conversation is reserved for queued work or a read-only bot delivery."));
            }
            runtime.start(&id, &text).await
        })
        .await?;
    Ok(state_reply(&ctx, StatusCode::ACCEPTED, &remote))
}

async fn cancel(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(_): JsonBody<Empty>) -> ApiResult {
    let store = ctx.store.clone();
    let queued_id = id.clone();
    let cancelled_queued = ctx.queue.run(async move { store.mutate(|s| s.cancel_queued_work(&queued_id)).await }).await?;
    if !cancelled_queued {
        // Runtime cancellation waits for the run to settle; it is not held inside the command queue
        // (the Node handler awaited `done` outside the queue as well).
        ctx.runtime.cancel(&id).await?;
    }
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Decision {
    option_id: String,
}

async fn resolve_permission(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path((id, permission_id)): Path<(String, String)>, JsonBody(input): JsonBody<Decision>) -> ApiResult {
    let length = input.option_id.chars().count();
    if !(1..=256).contains(&length) {
        return Err(bad_request("optionId: String must contain between 1 and 256 character(s)"));
    }
    let runtime = ctx.runtime.clone();
    let local = is_local(&remote);
    ctx.queue.run(async move { runtime.resolve(&id, &permission_id, &input.option_id, local).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

fn workspace(ctx: &HttpContext) -> HttpResult<grok_bot_files::WorkspaceFiles> {
    let root = ctx.store.read(|s| s.settings().workspace.clone());
    if root.is_empty() {
        return Err(HttpError::bad_request("Choose a workspace in settings first."));
    }
    Ok(grok_bot_files::WorkspaceFiles::new(root))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathQuery {
    #[serde(default)]
    path: Option<String>,
}

async fn list_files(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, query: Result<Query<PathQuery>, axum::extract::rejection::QueryRejection>) -> ApiResult {
    let Query(query) = query.map_err(|_| bad_request("Request: invalid query"))?;
    let files = workspace(&ctx)?;
    let listing = tokio::task::spawn_blocking(move || files.list(query.path.as_deref().unwrap_or(""))).await.map_err(|_| internal())??;
    Ok(reply(StatusCode::OK, to_value(&listing), &remote))
}

async fn read_file(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, query: Result<Query<PathQuery>, axum::extract::rejection::QueryRejection>) -> ApiResult {
    let Query(query) = query.map_err(|_| bad_request("Request: invalid query"))?;
    let path = query.path.ok_or_else(|| bad_request("path: Required"))?;
    let files = workspace(&ctx)?;
    let document = tokio::task::spawn_blocking(move || files.read(&path)).await.map_err(|_| internal())??;
    Ok(reply(StatusCode::OK, to_value(&document), &remote))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveFile {
    path: String,
    content: String,
    hash: String,
}

async fn save_file(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, JsonBody(input): JsonBody<SaveFile>) -> ApiResult {
    if input.path.chars().count() > 4096 {
        return Err(bad_request("path: String must contain at most 4096 character(s)"));
    }
    if !(input.hash.is_empty() || (input.hash.len() == 64 && input.hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))) {
        return Err(bad_request("hash: Invalid"));
    }
    let context = ctx.clone();
    let document = ctx
        .queue
        .run(async move {
            context.store.read(|s| s.assert_idle())?;
            let files = workspace(&context)?;
            tokio::task::spawn_blocking(move || files.save(&input.path, &input.content, &input.hash)).await.map_err(|_| HttpError::new(500, "The operation failed. Check that the workspace and data directory are accessible."))?
        })
        .await?;
    Ok(reply(StatusCode::OK, to_value(&document), &remote))
}

fn internal() -> ApiError {
    ApiError(HttpError::new(500, "The operation failed. Check that the workspace and data directory are accessible."))
}

/// Managed computer control is not ported yet: report a stopped browser (the panel renders its
/// normal idle state) and refuse to start one. Loopback-only, like the Node routes.
async fn computer_status(ConnectInfo(remote): Remote) -> ApiResult {
    if !is_local(&remote) {
        return Err(ApiError(HttpError::forbidden("Managed browser control requires a local loopback connection. This network server has no user authentication.")));
    }
    Ok(Json(json!({
        "state": "stopped", "epoch": uuid::Uuid::new_v4().to_string(), "frameId": null,
        "viewport": { "width": 960, "height": 600 }, "origins": [], "url": null, "busy": false, "sandboxed": false,
    }))
    .into_response())
}

async fn computer_disabled() -> ApiError {
    ApiError(HttpError::unavailable("Managed browser computing is not available in this build yet."))
}

async fn mcp(State(ctx): State<Ctx>, Path(run_id): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let authorization = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return ApiError(HttpError::bad_request("Request body must be valid JSON.")).into_response();
    };
    match ctx.runtime.mcp(&run_id, authorization.as_deref(), body).await {
        Ok(Some(result)) => Json(result).into_response(),
        Ok(None) => StatusCode::ACCEPTED.into_response(),
        Err(error) => ApiError(error).into_response(),
    }
}

#[allow(dead_code)]
fn _body(_: Body) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_parsing_matches_url_semantics() {
        assert_eq!(split_host("127.0.0.1:3000"), Some(("127.0.0.1".into(), Some(3000))));
        assert_eq!(split_host("localhost"), Some(("localhost".into(), None)));
        assert_eq!(split_host("[::1]:8080"), Some(("[::1]".into(), Some(8080))));
        assert_eq!(split_host("host:99999"), None);
        assert_eq!(origin_authority("LocalHost", Some(80)), "localhost");
        assert_eq!(origin_authority("localhost", Some(3000)), "localhost:3000");
        assert_eq!(normalize_host("Devbox.Example.NET."), "devbox.example.net");
    }
}
