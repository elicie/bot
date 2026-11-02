//! Conversation-owned canvas mutations. Browser edits stay available during model runs; the
//! store's optimistic revision checks, rather than the workspace-file idle guard, prevent loss.

use axum::Router;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::routing::{post, put};
use grok_bot_model::{CanvasInput, CanvasSource};
use serde::Deserialize;

use crate::http::{ApiResult, Ctx, JsonBody, Remote, state_reply};

pub(crate) fn routes() -> Router<Ctx> {
    Router::new().route("/conversations/{id}/canvas", put(save_canvas)).route("/conversations/{id}/canvas/restore", post(restore_canvas))
}

async fn save_canvas(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(input): JsonBody<CanvasInput>) -> ApiResult {
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|state| state.save_canvas(&id, input, CanvasSource::User)).await }).await?;
    // Do not return the raw mutation result: canvas content and history must use snapshot redaction.
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RestoreCanvas {
    revision: u64,
    target_revision: u64,
}

async fn restore_canvas(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(input): JsonBody<RestoreCanvas>) -> ApiResult {
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|state| state.restore_canvas(&id, input.revision, input.target_revision)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}
