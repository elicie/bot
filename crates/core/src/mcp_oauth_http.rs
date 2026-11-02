//! Local-only OAuth commands. Consent URLs never enter shared snapshots/SSE or persisted state.

use axum::Router;
use axum::extract::{ConnectInfo, FromRequest, Path, Request, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use grok_bot_model::{HttpError, McpServerRecord};
use serde_json::json;

use crate::http::{ApiResult, Ctx, Empty, JsonBody, Remote, is_local, reply, to_value};
use crate::mcp_oauth::service::{LoginInput, OAuthService};

const LOCAL_REQUIRED: &str = "MCP OAuth 로그인·로그아웃은 서버 컴퓨터의 localhost 연결에서만 할 수 있어요.";
const UNAVAILABLE: &str = "OAuth 암호화 보관함을 사용할 수 없어요. 서버의 보관함 파일과 권한을 확인해 주세요.";

pub(crate) fn routes() -> Router<Ctx> {
    Router::new()
        .route("/bots/{bot_id}/mcp-servers/{name}/oauth", get(status))
        .route("/bots/{bot_id}/mcp-servers/{name}/oauth/login", post(login))
        .route("/bots/{bot_id}/mcp-servers/{name}/oauth/logout", post(logout))
}

fn record(ctx: &Ctx, bot_id: &str, name: &str) -> Result<McpServerRecord, HttpError> {
    ctx.store.read(|state| {
        state.bot(bot_id)?;
        state.mcp_servers_for(bot_id).find(|record| record.name == name).cloned().ok_or_else(|| HttpError::not_found("MCP 서버를 찾을 수 없어요."))
    })
}

fn admit(ctx: &Ctx) -> Result<(), HttpError> {
    if ctx.closing.load(std::sync::atomic::Ordering::Acquire) || ctx.runtime.is_paused() {
        return Err(HttpError::unavailable("서버 종료 또는 업데이트 중에는 OAuth 계정을 변경할 수 없어요."));
    }
    ctx.store.read(|state| state.assert_idle())
}

fn service(ctx: &Ctx) -> Result<std::sync::Arc<OAuthService>, HttpError> {
    ctx.runtime.oauth().ok_or_else(|| HttpError::unavailable(UNAVAILABLE))
}

async fn status(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path((bot_id, name)): Path<(String, String)>) -> ApiResult {
    let record = record(&ctx, &bot_id, &name)?;
    let mut body = match ctx.runtime.oauth() {
        Some(service) => to_value(&service.status(&record).await),
        None => json!({"status": "unavailable", "canLogin": false, "message": UNAVAILABLE}),
    };
    // This is capability metadata, never inferred from a supplied Host/forwarded address.
    if !is_local(&remote) || ctx.runtime.is_paused() || ctx.closing.load(std::sync::atomic::Ordering::Acquire) {
        body["canLogin"] = json!(false);
    }
    Ok(reply(StatusCode::OK, body, &remote))
}

async fn login(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path((bot_id, name)): Path<(String, String)>, request: Request) -> ApiResult {
    if !is_local(&remote) {
        return Err(HttpError::forbidden(LOCAL_REQUIRED).into());
    }
    let JsonBody(input) = JsonBody::<LoginInput>::from_request(request, &ctx)
        .await
        .map_err(|_| crate::http::ApiError(HttpError::bad_request("OAuth 로그인 요청에는 선택적인 clientId와 resource만 넣어 주세요.")))?;
    let service = service(&ctx)?;
    let queued = ctx.clone();
    // Discovery/registration is bounded; waiting for browser consent happens in the service's
    // cancellable callback task after this queue operation returns.
    let response = ctx
        .queue
        .run(async move {
            admit(&queued)?;
            let record = record(&queued, &bot_id, &name)?;
            service.login(record, input).await
        })
        .await?;
    Ok(reply(StatusCode::OK, to_value(&response), &remote))
}

async fn logout(State(ctx): State<Ctx>, ConnectInfo(remote): Remote, Path((bot_id, name)): Path<(String, String)>, request: Request) -> ApiResult {
    if !is_local(&remote) {
        return Err(HttpError::forbidden(LOCAL_REQUIRED).into());
    }
    let JsonBody(_) = JsonBody::<Empty>::from_request(request, &ctx).await?;
    let service = service(&ctx)?;
    let queued = ctx.clone();
    let response = ctx
        .queue
        .run(async move {
            admit(&queued)?;
            let record = record(&queued, &bot_id, &name)?;
            service.logout(&record).await
        })
        .await?;
    Ok(reply(StatusCode::OK, to_value(&response), &remote))
}
