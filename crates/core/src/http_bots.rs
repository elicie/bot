//! `/api` routes for per-bot memories, knowledge files, and bot templates. Merged into
//! [`crate::http::router`]; every mutation runs in the command queue like the other routes.

use axum::Router;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use grok_bot_model::{HttpError, KnowledgeInput, MemoryInput, MemorySource, TemplateImport};
use grok_bot_store::memory::preview_memory_import;
use serde::Deserialize;
use serde_json::json;

use crate::http::{ApiResult, Ctx, Empty, JsonBody, Remote, reply, state_reply, to_value};

pub(crate) fn router() -> Router<Ctx> {
    Router::new()
        .route("/bots/{bot_id}/memories", post(add_memories))
        .route("/bots/{bot_id}/memories/preview", post(preview_memories))
        .route("/bots/{bot_id}/memories/{id}", delete(delete_memory))
        .route("/bots/{bot_id}/knowledge", post(add_knowledge))
        .route("/bots/{bot_id}/knowledge/{id}", get(read_knowledge).delete(delete_knowledge))
        .route("/bots/{bot_id}/template", get(export_template))
        .route("/bot-templates", post(import_template))
}

async fn add_memories(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(bot_id): Path<String>, JsonBody(input): JsonBody<MemoryInput>) -> ApiResult {
    let source = if input.imported == Some(true) { MemorySource::Import } else { MemorySource::User };
    let store = ctx.store.clone();
    let result = ctx.queue.run(async move { store.mutate(move |s| s.add_memories(&bot_id, input.texts, source, false)).await }).await?;
    let status = if result.added.is_empty() { StatusCode::OK } else { StatusCode::CREATED };
    Ok(reply(status, json!({ "added": result.added, "existing": result.existing, "state": ctx.store.snapshot() }), &remote))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Paste {
    text: String,
}

async fn preview_memories(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(bot_id): Path<String>, JsonBody(input): JsonBody<Paste>) -> ApiResult {
    let memories = ctx.store.read(|s| -> Result<_, HttpError> { preview_memory_import(&input.text, s.memories(&bot_id)?) })?;
    Ok(reply(StatusCode::OK, json!({ "memories": memories }), &remote))
}

async fn delete_memory(
    State(ctx): State<Ctx>,
    ConnectInfo(remote): Remote,
    Path((bot_id, id)): Path<(String, String)>,
    JsonBody(_): JsonBody<Empty>,
) -> ApiResult {
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(move |s| s.delete_memory(&bot_id, &id, false)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

async fn add_knowledge(
    State(ctx): State<Ctx>,
    ConnectInfo(remote): Remote,
    Path(bot_id): Path<String>,
    JsonBody(input): JsonBody<KnowledgeInput>,
) -> ApiResult {
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.add_knowledge(&bot_id, input).await }).await?;
    Ok(state_reply(&ctx, StatusCode::CREATED, &remote))
}

async fn read_knowledge(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path((bot_id, id)): Path<(String, String)>) -> ApiResult {
    let (mut file, content) = ctx.store.read_knowledge(&bot_id, &id).await?;
    let content = ctx.store.read(|s| {
        file.name = s.redact(&file.name);
        s.redact(&content)
    });
    Ok(reply(StatusCode::OK, json!({ "file": file, "content": content }), &remote))
}

async fn delete_knowledge(
    State(ctx): State<Ctx>,
    ConnectInfo(remote): Remote,
    Path((bot_id, id)): Path<(String, String)>,
    JsonBody(_): JsonBody<Empty>,
) -> ApiResult {
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.delete_knowledge(&bot_id, &id).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportQuery {
    #[serde(default)]
    memories: Option<bool>,
}

async fn export_template(
    State(ctx): State<Ctx>,
    ConnectInfo(remote): Remote,
    Path(bot_id): Path<String>,
    query: Result<Query<ExportQuery>, axum::extract::rejection::QueryRejection>,
) -> ApiResult {
    let Query(query) = query.map_err(|_| crate::http::bad_request("Request: invalid query"))?;
    let template = ctx.store.read(|s| s.export_template(&bot_id, query.memories == Some(true)))?;
    Ok(reply(StatusCode::OK, to_value(&template), &remote))
}

async fn import_template(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, JsonBody(input): JsonBody<TemplateImport>) -> ApiResult {
    let store = ctx.store.clone();
    let bot = ctx.queue.run(async move { store.mutate(move |s| s.add_bot_from_template(input.template)).await }).await?;
    let state = ctx.store.snapshot();
    let bot = state.bots.iter().find(|item| item.id == bot.id).cloned().unwrap_or(bot);
    Ok(reply(StatusCode::CREATED, json!({ "bot": bot, "state": state }), &remote))
}
