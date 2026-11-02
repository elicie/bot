//! Application core shared by the web server and the Tauri desktop app.
//!
//! Module ownership during the migration:
//! - `queue`: coordinator (done)
//! - `provider`: files agent — port of `checkProvider` in store.ts
//! - `public_state`: files agent — port of `server/public-state.ts`
//! - `runtime`: runtime agent — port of `server/runtime.ts` on top of `grok_bot_engine::host`
//! - `http`, `scheduler`, `app`: coordinator — ports of `server/app.ts` and `RoutineScheduler`

pub mod app;
pub mod chat_features;
pub mod computer;
mod computer_http;
mod groups_http;
pub mod http;
mod http_bots;
mod main_bot;
mod mcp_apps_http;
mod mcp_probe;
pub mod permission_rules;
pub mod provider;
pub mod provider_relay;
pub mod public_state;
pub mod queue;
pub mod runtime;
pub mod scheduler;
mod vault_http;
pub mod webhooks;
