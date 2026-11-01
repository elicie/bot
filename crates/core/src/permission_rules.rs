//! `/api/permission-rules`: list, add, edit the action of, and delete the user's permission rules
//! (see `grok_bot_store::permissions`).
//!
//! The server has no login, so a LAN client is only trusted as far as the existing approvals let
//! it go: any change that can let actions proceed without asking (adding or switching a rule to
//! allow, deleting an ask rule) requires a loopback client on the server computer. LAN clients
//! may view the rules and tighten them (add ask rules, switch to ask, delete allow rules).

use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::routing::{get, patch};
use axum::Router;
use grok_bot_model::{PermissionRuleInput, PermissionRulePatch};
use serde_json::json;

use crate::http::{ApiResult, Ctx, Empty, JsonBody, Remote, is_local, reply, state_reply, to_value};

pub(crate) fn routes() -> Router<Ctx> {
    Router::new()
        .route("/permission-rules", get(list).post(add))
        .route("/permission-rules/{id}", patch(update).delete(delete))
}

/// `{ rules, canAutoAllow }`: `canAutoAllow` tells the editor whether this client may loosen rules.
async fn list(State(ctx): State<Ctx>, ConnectInfo(remote): Remote) -> ApiResult {
    let rules = ctx.store.read(|s| to_value(&s.permission_rules()));
    Ok(reply(StatusCode::OK, json!({ "rules": rules, "canAutoAllow": is_local(&remote) }), &remote))
}

async fn add(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, JsonBody(input): JsonBody<PermissionRuleInput>) -> ApiResult {
    let (store, local) = (ctx.store.clone(), is_local(&remote));
    ctx.queue.run(async move { store.mutate(|s| s.add_permission_rule(input, local)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::CREATED, &remote))
}

async fn update(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(input): JsonBody<PermissionRulePatch>) -> ApiResult {
    let (store, local) = (ctx.store.clone(), is_local(&remote));
    ctx.queue.run(async move { store.mutate(|s| s.update_permission_rule(&id, input, local)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}

async fn delete(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(_): JsonBody<Empty>) -> ApiResult {
    let (store, local) = (ctx.store.clone(), is_local(&remote));
    ctx.queue.run(async move { store.mutate(|s| s.delete_permission_rule(&id, local)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}
