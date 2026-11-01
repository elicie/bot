//! `/api` routes for vault secrets ("보관함") and custom MCP servers ("사용자 지정 MCP"); see
//! `grok_bot_store::vault`.
//!
//! The server has no login, so every secret change and every change that can make a server run
//! (saving the MCP JSON, enabling a server) requires a loopback client on the server computer.
//! LAN clients see secret names and server summaries in the snapshot and may disable servers.
//! The MCP JSON itself (commands, URLs, literal env/header values) is served to loopback clients
//! only.

use axum::Router;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post, put};
use grok_bot_model::{HttpError, McpConfigInput, McpEnabledInput, SecretInput, SecretValueInput};
use serde_json::json;

use crate::http::{ApiResult, Ctx, Empty, JsonBody, Remote, is_local, reply, state_reply};

pub(crate) fn routes() -> Router<Ctx> {
    Router::new()
        .route("/vault", get(access))
        .route("/secrets", post(add_secret))
        .route("/secrets/{id}", put(replace_secret).delete(delete_secret))
        .route("/bots/{bot_id}/mcp-servers", get(read_mcp).put(save_mcp))
        .route("/bots/{bot_id}/mcp-servers/{name}", patch(toggle_mcp))
}

/// `{ canEdit }`: whether this client may change secrets and custom MCP servers.
async fn access(ConnectInfo(remote): Remote) -> ApiResult {
    Ok(reply(StatusCode::OK, json!({ "canEdit": is_local(&remote) }), &remote))
}

async fn add_secret(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, JsonBody(input): JsonBody<SecretInput>) -> ApiResult {
    let (store, local) = (ctx.store.clone(), is_local(&remote));
    ctx.queue.run(async move { store.mutate(move |s| s.add_secret(input, local)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::CREATED, &remote))
}

async fn replace_secret(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(input): JsonBody<SecretValueInput>) -> ApiResult {
    let (store, local) = (ctx.store.clone(), is_local(&remote));
    ctx.queue.run(async move { store.mutate(move |s| s.replace_secret(&id, &input.value, local)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

async fn delete_secret(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(_): JsonBody<Empty>) -> ApiResult {
    let (store, local) = (ctx.store.clone(), is_local(&remote));
    ctx.queue.run(async move { store.mutate(move |s| s.delete_secret(&id, local)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

/// The bot's "MCP 서버 JSON" (loopback only: it can hold literal credentials).
async fn read_mcp(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(bot_id): Path<String>) -> ApiResult {
    if !is_local(&remote) {
        return Err(HttpError::forbidden(grok_bot_store::vault::MCP_REQUIRES_LOOPBACK).into());
    }
    let json = ctx.store.read(|s| s.mcp_json(&bot_id))?;
    Ok(reply(StatusCode::OK, json!({ "json": json }), &remote))
}

async fn save_mcp(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(bot_id): Path<String>, JsonBody(input): JsonBody<McpConfigInput>) -> ApiResult {
    let (store, local) = (ctx.store.clone(), is_local(&remote));
    ctx.queue.run(async move { store.mutate(move |s| s.set_mcp_servers(&bot_id, &input.json, local)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

async fn toggle_mcp(
    State(ctx): State<Ctx>,
    ConnectInfo(remote): Remote,
    Path((bot_id, name)): Path<(String, String)>,
    JsonBody(input): JsonBody<McpEnabledInput>,
) -> ApiResult {
    let (store, local) = (ctx.store.clone(), is_local(&remote));
    ctx.queue.run(async move { store.mutate(move |s| s.set_mcp_server_enabled(&bot_id, &name, input.enabled, local)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}
