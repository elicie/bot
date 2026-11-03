//! One linked integration-test executable. The shared harness re-executes this binary for each
//! case so upstream's process-global engine state remains isolated while libtest runs in parallel.

mod attachments;
mod canvas_http;
mod common;
mod computer_http;
mod groups_threads;
mod http_api;
mod http_bots;
mod link_preview_api;
mod main_bot;
mod maintenance;
mod mcp_apps_http;
mod mcp_initial_search;
mod mcp_probe_shutdown;
mod per_bot_concurrency;
mod permissions_webhooks;
mod provider_relay;
mod runtime;
mod scheduler;
mod vault_mcp;
mod web;
