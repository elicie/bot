//! `/api/computer*` routes. Port of `installComputerRoutes` in `server/computer-api.ts`.
//!
//! Every route requires a loopback socket (forwarded-address headers grant nothing): this server
//! has no user authentication, and the managed browser acts from the server computer. Starts and
//! actions share the command queue with model runs and require an idle application; Stop bypasses
//! the queue so it can interrupt a slow navigation.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use grok_bot_model::HttpError;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

use crate::computer::{ApplicationComputer, is_uuid};
use crate::http::{ApiError, ApiResult, Ctx, JsonBody, Remote, bad_request, is_local, to_value};

pub(crate) fn routes() -> Router<Ctx> {
    Router::new()
        .route("/computer", get(status).post(start).delete(stop))
        .route("/computer/preview", get(preview))
        .route("/computer/actions", axum::routing::post(act))
}

fn local(remote: &std::net::SocketAddr) -> ApiResult<()> {
    if !is_local(remote) {
        return Err(ApiError(HttpError::forbidden(
            "Managed browser control requires a local loopback connection. This network server has no user authentication.",
        )));
    }
    Ok(())
}

fn computer(ctx: &Ctx) -> ApiResult<Arc<ApplicationComputer>> {
    ctx.runtime.computer().ok_or_else(|| ApiError(HttpError::unavailable("Managed browser is unavailable.")))
}

fn uuid(name: &str, value: &str) -> ApiResult<()> {
    if is_uuid(value) { Ok(()) } else { Err(bad_request(format!("{name}: Invalid uuid"))) }
}

/// Strict query: exactly `keys`, each a UUID.
fn query(query: Result<Query<HashMap<String, String>>, axum::extract::rejection::QueryRejection>, keys: &[&str]) -> ApiResult<HashMap<String, String>> {
    let Query(values) = query.map_err(|_| bad_request("Request: invalid query"))?;
    if let Some(extra) = values.keys().find(|key| !keys.contains(&key.as_str())) {
        return Err(bad_request(format!("Unrecognized key: \"{extra}\"")));
    }
    for key in keys {
        let value = values.get(*key).ok_or_else(|| bad_request(format!("{key}: Required")))?;
        uuid(key, value)?;
    }
    Ok(values)
}

fn json_response<T: serde::Serialize>(value: &T) -> Response {
    (StatusCode::OK, axum::Json(to_value(value))).into_response()
}

async fn status(
    State(ctx): State<Ctx>,
    ConnectInfo(remote): Remote,
    input: Result<Query<HashMap<String, String>>, axum::extract::rejection::QueryRejection>,
) -> ApiResult {
    local(&remote)?;
    let input = query(input, &["owner"])?;
    Ok(json_response(&computer(&ctx)?.status(&input["owner"])?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct StartInput {
    owner_id: String,
    request_id: String,
    user_initiated: bool,
    origins: Vec<String>,
}

async fn start(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, body: Result<JsonBody<StartInput>, ApiError>) -> ApiResult {
    local(&remote)?;
    let JsonBody(input) = body?;
    uuid("ownerId", &input.owner_id)?;
    uuid("requestId", &input.request_id)?;
    if !input.user_initiated {
        return Err(bad_request("userInitiated: Invalid input: expected true"));
    }
    if !(1..=16).contains(&input.origins.len()) {
        return Err(bad_request("origins: Select between one and sixteen allowed public origins."));
    }
    let computer = computer(&ctx)?;
    let generation = computer.generation();
    let store = ctx.store.clone();
    let request = json!({ "requestId": input.request_id, "userInitiated": true, "origins": input.origins });
    let status = ctx
        .queue
        .run(async move {
            store.read(|s| s.assert_idle())?;
            computer.start(&input.owner_id, request, generation).await
        })
        .await?;
    Ok(json_response(&status))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Owner {
    owner_id: String,
}

async fn stop(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, body: Result<JsonBody<Owner>, ApiError>) -> ApiResult {
    local(&remote)?;
    let JsonBody(input) = body?;
    uuid("ownerId", &input.owner_id)?;
    Ok(json_response(&computer(&ctx)?.stop(Some(&input.owner_id)).await?))
}

async fn preview(
    State(ctx): State<Ctx>,
    ConnectInfo(remote): Remote,
    input: Result<Query<HashMap<String, String>>, axum::extract::rejection::QueryRejection>,
) -> ApiResult {
    local(&remote)?;
    let input = query(input, &["owner", "epoch", "frame"])?;
    let png = computer(&ctx)?.preview(&input["owner"], &input["epoch"], &input["frame"])?;
    Ok(([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "no-store")], png.as_ref().clone()).into_response())
}

/// A key that must be present and may be `null` (`z.string().uuid().nullable()`).
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ActionInput {
    owner_id: String,
    request_id: String,
    epoch: String,
    #[serde(default, deserialize_with = "present")]
    frame_id: Option<Option<String>>,
    action: Value,
}

async fn act(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, body: Result<JsonBody<ActionInput>, ApiError>) -> ApiResult {
    local(&remote)?;
    let JsonBody(input) = body?;
    uuid("ownerId", &input.owner_id)?;
    uuid("requestId", &input.request_id)?;
    uuid("epoch", &input.epoch)?;
    let frame_id = input.frame_id.ok_or_else(|| bad_request("frameId: Required"))?;
    if let Some(frame_id) = &frame_id {
        uuid("frameId", frame_id)?;
    }
    let computer = computer(&ctx)?;
    let generation = computer.generation();
    let store = ctx.store.clone();
    let request = json!({ "requestId": input.request_id, "epoch": input.epoch, "frameId": frame_id, "action": input.action });
    let observation = ctx
        .queue
        .run(async move {
            store.read(|s| s.assert_idle())?;
            computer.perform(&input.owner_id, request, generation).await
        })
        .await?;
    Ok(json_response(&observation))
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use crate::http::is_local;

    #[test]
    fn computer_locality_follows_the_socket_address() {
        for address in ["127.0.0.1:1", "[::1]:1", "[::ffff:127.0.0.1]:1"] {
            assert!(is_local(&address.parse::<SocketAddr>().unwrap()), "{address}");
        }
        for address in ["10.0.0.2:1", "100.64.0.2:1", "192.168.1.2:1", "127.0.0.2:1", "[::ffff:10.0.0.2]:1"] {
            assert!(!is_local(&address.parse::<SocketAddr>().unwrap()), "{address}");
        }
    }
}
