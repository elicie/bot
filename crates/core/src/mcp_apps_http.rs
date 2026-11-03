//! Host-only proposal/decision endpoints for MCP Apps. The normal HTTP Host/Origin/JSON guard
//! applies. A sandboxed app cannot authorize a call: the host renders its own explicit approval.

use axum::Router;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::http::{ApiResult, Ctx, JsonBody, Remote, reply};

pub(crate) fn routes() -> Router<Ctx> {
    Router::new()
        .route("/conversations/{conversation}/mcp-apps/{activity}/requests", post(propose))
        .route("/conversations/{conversation}/mcp-apps/{activity}/requests/{request}", post(decide))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Proposal {
    name: String,
    arguments: serde_json::Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    allow: bool,
}

async fn propose(
    State(ctx): State<Ctx>,
    ConnectInfo(remote): Remote,
    Path((conversation, activity)): Path<(String, String)>,
    JsonBody(input): JsonBody<Proposal>,
) -> ApiResult {
    let request_id = ctx.runtime.request_mcp_app_call(&conversation, &activity, &input.name, Value::Object(input.arguments))?;
    Ok(reply(StatusCode::CREATED, json!({"requestId": request_id}), &remote))
}

async fn decide(
    State(ctx): State<Ctx>,
    ConnectInfo(remote): Remote,
    Path((conversation, activity, request)): Path<(String, String, String)>,
    JsonBody(input): JsonBody<Decision>,
) -> ApiResult {
    // Runtime owns serialization only during execution, not during the human's approval wait.
    let result = ctx.runtime.decide_mcp_app_call(&conversation, &activity, &request, input.allow).await?;
    Ok(reply(StatusCode::OK, result, &remote))
}
