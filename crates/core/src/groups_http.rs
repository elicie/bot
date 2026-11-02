//! `/api` routes for group chats and threads. Group messages, cancellation, and approvals use the
//! ordinary conversation routes; the runtime and dispatcher sequence the member turns.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{patch, post};
use grok_bot_model::{GroupInput, GroupPatch};
use serde::Deserialize;
use serde_json::json;

use crate::http::{ApiResult, Ctx, Empty, JsonBody, Remote, bad_request, reply, state_reply};

pub(crate) fn routes() -> Router<Ctx> {
    Router::new()
        .route("/groups", post(add_group))
        .route("/groups/{id}", patch(update_group).delete(delete_group))
        .route("/groups/{id}/conversations", post(add_group_conversation))
        .route("/conversations/{id}/threads", post(create_thread))
}

async fn add_group(State(ctx): State<Ctx>, axum::extract::ConnectInfo(remote): Remote, JsonBody(input): JsonBody<GroupInput>) -> ApiResult {
    let store = ctx.store.clone();
    let group = ctx.queue.run(async move { store.mutate(|s| s.add_group(input)).await }).await?;
    Ok(reply(StatusCode::CREATED, json!({ "group": group, "state": ctx.store.snapshot() }), &remote))
}

async fn update_group(State(ctx): State<Ctx>, axum::extract::ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(input): JsonBody<GroupPatch>) -> ApiResult {
    if input == GroupPatch::default() {
        return Err(bad_request("Provide a group field to update."));
    }
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|s| s.update_group(&id, input)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

async fn delete_group(State(ctx): State<Ctx>, axum::extract::ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(_): JsonBody<Empty>) -> ApiResult {
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|s| s.delete_group(&id)).await }).await?;
    crate::chat_features::prune_generated_images(&ctx.store).await;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

async fn add_group_conversation(State(ctx): State<Ctx>, axum::extract::ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(_): JsonBody<Empty>) -> ApiResult {
    let store = ctx.store.clone();
    let conversation = ctx.queue.run(async move { store.mutate(|s| s.add_group_conversation(&id)).await }).await?;
    Ok(reply(StatusCode::CREATED, json!({ "conversation": conversation, "state": ctx.store.snapshot() }), &remote))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct NewThread {
    message_id: String,
}

async fn create_thread(State(ctx): State<Ctx>, axum::extract::ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(input): JsonBody<NewThread>) -> ApiResult {
    let store = ctx.store.clone();
    let existed = ctx.store.read(|s| s.thread_for(&input.message_id).is_some());
    let conversation = ctx.queue.run(async move { store.mutate(|s| s.create_thread(&id, &input.message_id)).await }).await?;
    let status = if existed { StatusCode::OK } else { StatusCode::CREATED };
    Ok(reply(status, json!({ "conversation": conversation, "state": ctx.store.snapshot() }), &remote))
}
