//! Agent runtime. Port of `server/runtime.ts` onto the in-process engine host
//! (`grok_bot_engine::host::EngineHost`). CONTRACT (runtime agent): keep these signatures; the HTTP
//! layer and scheduler call them.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use grok_bot_model::HttpResult;
use grok_bot_store::Store;

use crate::queue::CommandQueue;

#[derive(Clone, Debug)]
pub struct RuntimeOptions {
    /// Directory passed to `grok_bot_engine::host::prepare_process_environment` by the binary.
    pub engine_root: PathBuf,
    pub startup_timeout: Duration,
    /// Backend inactivity deadline while no approval is pending (Node: 30 s).
    pub prompt_idle_timeout: Duration,
    pub cancel_deadline: Duration,
}

pub struct Runtime {
    _private: (),
}

impl Runtime {
    pub fn new(store: Arc<Store>, queue: CommandQueue, options: RuntimeOptions) -> Arc<Self> {
        let _ = (store, queue, options);
        todo!("runtime agent")
    }

    /// Record engine availability (`RuntimeInfo`) in the store.
    pub async fn initialize(&self) {
        todo!("runtime agent")
    }

    /// Origin of this server for the per-run MCP broker URL, e.g. `http://127.0.0.1:3000`.
    pub fn set_origin(&self, origin: String) {
        let _ = origin;
        todo!("runtime agent")
    }

    /// Append the user prompt and an empty assistant message, then run the turn in the background.
    /// Errors match the Node runtime (409 when busy, 503 engine unavailable, 400 no provider).
    pub async fn start(&self, conversation_id: &str, prompt: &str) -> HttpResult<()> {
        let _ = (conversation_id, prompt);
        todo!("runtime agent")
    }

    pub async fn resolve(&self, conversation_id: &str, permission_id: &str, option_id: &str, local_request: bool) -> HttpResult<()> {
        let _ = (conversation_id, permission_id, option_id, local_request);
        todo!("runtime agent")
    }

    /// Cancel an active run and wait for it to settle. No-op for idle conversations (404 if missing).
    pub async fn cancel(&self, conversation_id: &str) -> HttpResult<()> {
        let _ = conversation_id;
        todo!("runtime agent")
    }

    pub async fn shutdown(&self) {
        todo!("runtime agent")
    }

    /// `POST /internal/mcp/:runId` JSON-RPC handler. `Ok(None)` means 202 with no body.
    pub async fn mcp(&self, run_id: &str, authorization: Option<&str>, body: serde_json::Value) -> HttpResult<Option<serde_json::Value>> {
        let _ = (run_id, authorization, body);
        todo!("runtime agent")
    }
}
