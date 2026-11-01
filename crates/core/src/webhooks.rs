//! Routine webhooks. Key management lives under `/api` (same-origin browser mutations); the trigger
//! endpoint `POST /hooks/routines/:id` lives outside `/api` because external senders cannot send
//! `Origin` or `X-Grok-Bot`. It authenticates with `Authorization: Bearer <key>` (the header the
//! original app documents), compares hashes in constant time, and claims the run through the
//! scheduler's admission path (FIFO, single global slot, one queued or active run per routine).
//!
//! Host validation still applies to the trigger: a LAN sender needs `HOST=0.0.0.0` and its Host in
//! `GROK_BOT_ALLOWED_HOSTS`. Repeated failed authentication for a routine is throttled.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use grok_bot_model::HttpError;
use grok_bot_store::webhooks::{MAX_WEBHOOK_PAYLOAD_BYTES, webhook_key_hash, webhook_key_hint};
use serde_json::json;

use crate::http::{ApiError, ApiResult, Ctx, Empty, JsonBody, Remote, reply, state_reply};

/// Failed authentications allowed per routine within [`FAILURE_WINDOW`] before 429.
const MAX_FAILURES: u32 = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
const MAX_TRACKED_ROUTINES: usize = 4096;
const KEY_PREFIX: &str = "gbw_";

/// `/api` routes: create or rotate (`POST`) and revoke (`DELETE`) a routine's webhook key.
pub(crate) fn api_routes() -> Router<Ctx> {
    Router::new().route("/routines/{id}/webhook", post(create_key).delete(revoke_key))
}

/// The external trigger route, merged outside `/api`.
pub(crate) fn hook_routes() -> Router<Ctx> {
    Router::new().route("/hooks/routines/{id}", post(trigger).fallback(|| async { (StatusCode::METHOD_NOT_ALLOWED, Json(json!({ "error": "Send webhooks with POST." }))) }))
}

fn new_key() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the operating system provides randomness");
    format!("{KEY_PREFIX}{}", hex::encode(bytes))
}

/// Generate a new key (replacing any previous one). The key is returned once and never stored.
async fn create_key(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(_): JsonBody<Empty>) -> ApiResult {
    let key = new_key();
    let (hash, hint) = (webhook_key_hash(&key), webhook_key_hint(&key));
    let store = ctx.store.clone();
    let webhook = ctx.queue.run(async move { store.mutate(|s| s.set_routine_webhook(&id, hash, hint)).await }).await?;
    let state = ctx.store.snapshot();
    Ok(reply(
        StatusCode::CREATED,
        json!({ "key": key, "keyHint": webhook.key_hint, "createdAt": webhook.created_at, "state": state }),
        &remote,
    ))
}

async fn revoke_key(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(_): JsonBody<Empty>) -> ApiResult {
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|s| s.clear_routine_webhook(&id)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

struct Failures {
    count: u32,
    since: Instant,
}

static FAILURES: LazyLock<Mutex<HashMap<String, Failures>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn failures() -> std::sync::MutexGuard<'static, HashMap<String, Failures>> {
    FAILURES.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Seconds until `routine` may try again, when it is throttled.
fn throttled(routine: &str) -> Option<u64> {
    let mut failures = failures();
    let now = Instant::now();
    failures.retain(|_, entry| now.duration_since(entry.since) < FAILURE_WINDOW);
    let entry = failures.get(routine)?;
    (entry.count >= MAX_FAILURES).then(|| (FAILURE_WINDOW - now.duration_since(entry.since)).as_secs().max(1))
}

/// Count a failed authentication. Tracking is bounded so requests for made-up routine IDs cannot
/// grow it without limit; entries expire after [`FAILURE_WINDOW`].
fn record_failure(routine: &str) {
    let mut failures = failures();
    if routine.len() > 64 || (!failures.contains_key(routine) && failures.len() >= MAX_TRACKED_ROUTINES) {
        return;
    }
    let entry = failures.entry(routine.to_owned()).or_insert(Failures { count: 0, since: Instant::now() });
    entry.count += 1;
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, key) = value.split_once(' ')?;
    let key = key.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !key.is_empty() && key.len() <= 256).then_some(key)
}

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn error(error: HttpError) -> Response {
    let unauthorized = error.status == 401;
    let mut response = ApiError(error).into_response();
    if unauthorized {
        response.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    no_store(response)
}

/// `POST /hooks/routines/:id`: 202 with a minimal receipt (never the application state).
async fn trigger(State(ctx): State<Ctx>, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    if ctx.closing.load(std::sync::atomic::Ordering::Relaxed) {
        return error(HttpError::unavailable("The server is shutting down."));
    }
    if let Some(retry) = throttled(&id) {
        let mut response = error(HttpError::new(429, "Too many failed webhook attempts. Try again later."));
        if let Ok(value) = HeaderValue::from_str(&retry.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }
    let Some(key) = bearer(&headers).map(str::to_owned) else {
        record_failure(&id);
        return error(HttpError::new(401, grok_bot_store::webhooks::INVALID_WEBHOOK_KEY));
    };
    if body.len() > MAX_WEBHOOK_PAYLOAD_BYTES {
        return error(HttpError::too_large("Webhook payloads must be at most 16 KiB of UTF-8 text."));
    }
    let Ok(payload) = std::str::from_utf8(&body) else {
        return error(HttpError::bad_request("Webhook payloads must be UTF-8 text."));
    };
    let payload = (!payload.trim().is_empty()).then_some(payload);
    match ctx.scheduler.run_webhook(&id, &key, payload).await {
        Ok(run) => no_store(
            (
                StatusCode::ACCEPTED,
                Json(json!({ "run": { "id": run.id, "routineId": run.routine_id, "trigger": run.trigger, "createdAt": run.created_at } })),
            )
                .into_response(),
        ),
        Err(failure) => {
            if failure.status == 401 {
                record_failure(&id);
            }
            error(failure)
        }
    }
}
