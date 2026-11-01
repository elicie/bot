//! HTTP route for choosing the main bot ("메인 Bot 선택", `setMainAgent` in Grok Bot). Merged into
//! `http::router` with one `.merge(...)` line. Rules in `grok_bot_store::main_bot`.
//!
//! - `POST /api/bots/:id/main` `{}`: make the bot the main bot. Returns the state snapshot, whose
//!   `mainBotId` names the chosen main bot (absent: the first bot).

use axum::Router;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::routing::post;

use crate::http::{ApiResult, Ctx, Empty, JsonBody, Remote, state_reply};

pub(crate) fn routes() -> Router<Ctx> {
    Router::new().route("/bots/{id}/main", post(set_main))
}

async fn set_main(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path(id): Path<String>, JsonBody(_): JsonBody<Empty>) -> ApiResult {
    let store = ctx.store.clone();
    ctx.queue.run(async move { store.mutate(|s| s.set_main_bot(&id)).await }).await?;
    Ok(state_reply(&ctx, StatusCode::OK, &remote))
}
