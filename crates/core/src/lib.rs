//! Application core shared by the web server and the desktop host.
//!
//! HTTP/SSE, serialized commands, runtime admission, scheduling, managed browsing, and provider
//! integration live here. The in-process engine host is supplied by `grok_bot_engine::host`.

pub mod app;
mod canvas_http;
pub mod chat_features;
pub mod computer;
mod computer_http;
pub mod desktop_auth;
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
pub mod web;
pub mod webhooks;
