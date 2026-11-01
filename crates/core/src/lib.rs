//! Application core shared by the web server and the Tauri desktop app.
//!
//! Module ownership during the migration:
//! - `queue`: coordinator (done)
//! - `provider`: files agent — port of `checkProvider` in store.ts
//! - `runtime`: runtime agent — port of `server/runtime.ts` on top of `grok_bot_engine::host`
//! - `http`, `scheduler`, `app`: coordinator — ports of `server/app.ts` and `RoutineScheduler`

pub mod provider;
pub mod queue;
pub mod runtime;
