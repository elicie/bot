//! PLACEHOLDER (coordinator tree only): the files agent owns the real port of
//! `server/public-state.ts`. Replaced at merge.
use grok_bot_model::AppState;
pub fn project_computer_permissions(state: AppState, local: bool) -> AppState {
    let _ = local;
    state
}
pub fn project_response(body: serde_json::Value, local: bool) -> serde_json::Value {
    let _ = local;
    body
}
