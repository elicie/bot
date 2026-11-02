//! The one shared in-process engine. It starts lazily on the first run and restarts when the
//! provider settings change while no run is active. Engine events from its single receiver are
//! routed to runs by session ID.

use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::Duration;

use agent_client_protocol::{self as acp, Agent as _};
use anyhow::{Result, anyhow};
use grok_bot_engine::host::{EngineEvent, EngineHost, EngineSettings, ModelSpec};
use grok_bot_model::PrivateSettings;
use tokio::sync::{Mutex, mpsc, oneshot};

use super::Runtime;
use crate::provider_relay::RelayEndpoint;

const MCP_STATE_ERROR: &str = "Could not confirm MCP initialization for this session.";

/// Parse only the readiness predicate. Never surface the catalog or engine errors: MCP
/// declarations may contain credentials, and this response is internal bookkeeping only.
fn mcp_resolved(response: &serde_json::Value) -> Result<bool> {
    if !response["error"].is_null() {
        return Err(anyhow!(MCP_STATE_ERROR));
    }
    response["result"]["sessionMcpResolved"].as_bool().ok_or_else(|| anyhow!(MCP_STATE_ERROR))
}

/// Engine-side alias of the single configured provider model.
pub const MODEL_ALIAS: &str = "local";

/// The provider fields an engine instance was configured with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderKey {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

impl ProviderKey {
    pub fn from_settings(settings: &PrivateSettings) -> Self {
        Self { base_url: settings.base_url.clone(), model: settings.model.clone(), api_key: settings.api_key.clone() }
    }
}

/// Engine configuration for one provider, matching the configuration the Node runtime generated
/// per process (single `local` model, broker MCP tools pre-allowed, 120 s inference idle timeout).
/// With a relay the engine talks to it (and holds only the relay token, not the provider key).
pub fn engine_settings(engine_root: &Path, key: &ProviderKey, relay: Option<&RelayEndpoint>) -> EngineSettings {
    let (base_url, api_key) = match relay {
        Some(relay) => (relay.base_url.clone(), Some(relay.token.clone())),
        None => (key.base_url.clone(), (!key.api_key.is_empty()).then(|| key.api_key.clone())),
    };
    EngineSettings {
        grok_home: engine_root.join("home").join(".grok"),
        models: vec![ModelSpec { id: MODEL_ALIAS.into(), model: key.model.clone(), base_url, api_key }],
        default_model: MODEL_ALIAS.into(),
        allow: vec!["MCPTool(workspace__*)".into()],
        inference_idle_timeout_secs: 120,
    }
}

/// A running engine and the barrier channel of its event router.
#[derive(Clone)]
pub struct Engine {
    pub host: Arc<EngineHost>,
    barrier: mpsc::UnboundedSender<oneshot::Sender<()>>,
}

impl Engine {
    /// Await the session's searchable MCP snapshot, including settled failures. A server's
    /// `ready` status alone is too early: upstream publishes the final tool index afterwards.
    /// Called inside open_session's startup deadline/cancellation select, before prompt/idle time.
    /// Keep the loop here, not inside host.call: engine jobs outlive dropped caller futures.
    pub async fn wait_for_mcp(&self, session_id: &str) -> Result<()> {
        loop {
            let params = serde_json::value::to_raw_value(&serde_json::json!({"sessionId":session_id,"cache":true}))?;
            let response = self
                .host
                .call(move |connection| async move { connection.ext_method(acp::ExtRequest::new("x.ai/mcp/list", params.into())).await })
                .await?
                .map_err(|_| anyhow!(MCP_STATE_ERROR))?;
            let response = serde_json::from_str(response.0.get()).map_err(|_| anyhow!(MCP_STATE_ERROR))?;
            if mcp_resolved(&response)? {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Wait until every event the engine emitted before this call has been projected.
    ///
    /// ACP dispatches each notification on its own engine-thread task, so a response can overtake
    /// the notifications that preceded it on the wire. A no-op job queued behind those tasks lets
    /// them reach the event channel; the router then drains the channel before acknowledging.
    pub async fn drain(&self, limit: Duration) {
        let settle = async {
            let _ = self
                .host
                .call(|_| async {
                    for _ in 0..8 {
                        tokio::task::yield_now().await;
                    }
                })
                .await;
            let (tx, rx) = oneshot::channel();
            if self.barrier.send(tx).is_ok() {
                let _ = rx.await;
            }
        };
        let _ = tokio::time::timeout(limit, settle).await;
    }
}

struct Slot {
    engine: Engine,
    key: ProviderKey,
}

#[derive(Default)]
pub struct EngineManager {
    slot: Mutex<Option<Slot>>,
}

impl EngineManager {
    /// The engine for `key`, starting or restarting it as needed. The caller guarantees that no
    /// other run is active (the store admits one active run at a time).
    pub async fn acquire(&self, runtime: &Arc<Runtime>, engine_root: &Path, key: &ProviderKey, relay: Option<&RelayEndpoint>) -> Result<Engine> {
        let mut slot = self.slot.lock().await;
        if let Some(current) = slot.as_ref()
            && current.key == *key
        {
            return Ok(current.engine.clone());
        }
        if let Some(previous) = slot.take() {
            stop(previous.engine).await;
        }
        let (host, events) =
            EngineHost::start(engine_settings(engine_root, key, relay)).await.map_err(|error| anyhow!("Could not start the application engine. {error:#}"))?;
        let (barrier, barriers) = mpsc::unbounded_channel();
        tokio::spawn(route(Arc::downgrade(runtime), events, barriers));
        let engine = Engine { host: Arc::new(host), barrier };
        *slot = Some(Slot { engine: engine.clone(), key: key.clone() });
        Ok(engine)
    }

    pub async fn shutdown(&self) {
        if let Some(previous) = self.slot.lock().await.take() {
            stop(previous.engine).await;
        }
    }
}

/// Stop an engine thread. Dropping the last host handle joins the thread, so do it off the async
/// workers. Handles still held by abandoned calls keep it alive until they are dropped.
async fn stop(engine: Engine) {
    let Engine { host, barrier } = engine;
    drop(barrier);
    let _ = tokio::task::spawn_blocking(move || drop(host)).await;
}

/// Route engine events in arrival order. Updates are projected one at a time so stored text keeps
/// the stream order; permission requests wait for the browser on their own task.
async fn route(runtime: Weak<Runtime>, mut events: mpsc::UnboundedReceiver<EngineEvent>, mut barriers: mpsc::UnboundedReceiver<oneshot::Sender<()>>) {
    loop {
        tokio::select! {
            biased;
            event = events.recv() => {
                let Some(event) = event else { break };
                let Some(runtime) = runtime.upgrade() else { break };
                dispatch(&runtime, event).await;
            }
            Some(ack) = barriers.recv() => {
                while let Ok(event) = events.try_recv() {
                    let Some(runtime) = runtime.upgrade() else { return };
                    dispatch(&runtime, event).await;
                }
                let _ = ack.send(());
            }
        }
    }
    // The engine stopped: answer any remaining barrier waiters.
    barriers.close();
    while let Ok(ack) = barriers.try_recv() {
        let _ = ack.send(());
    }
}

async fn dispatch(runtime: &Arc<Runtime>, event: EngineEvent) {
    match event {
        EngineEvent::Update(notification) => runtime.project(notification).await,
        EngineEvent::Permission { request, respond } => {
            let runtime = Arc::clone(runtime);
            tokio::spawn(async move {
                let response = runtime.request_permission(request).await;
                let _ = respond.send(response);
            });
        }
        EngineEvent::Extension { method, params } => runtime.extension(&method, params).await,
    }
}

pub fn cancelled_permission() -> acp::RequestPermissionResponse {
    acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn readiness_requires_the_session_completion_flag_not_tool_counts() {
        assert!(!mcp_resolved(&json!({"result":{"sessionMcpResolved":false,"servers":[{"session":{"status":"ready","tools":[{"name":"search"}]}}]}})).unwrap());
        // Failed servers can settle with no tools: do not wait forever for a healthy connector.
        assert!(mcp_resolved(&json!({"result":{"sessionMcpResolved":true,"servers":[]}})).unwrap());
    }

    #[test]
    fn missing_malformed_or_failed_readiness_is_not_accepted_and_does_not_echo_details() {
        for response in [
            json!(null),
            json!({"result":null,"error":"private-server-credential"}),
            json!({"result":{"sessionMcpResolved":true},"error":{"message":"private-server-credential"}}),
            json!({"result":{"servers":[]}}),
            json!({"result":{"sessionMcpResolved":"true"}}),
            json!({"sessionMcpResolved":true}),
        ] {
            assert_eq!(mcp_resolved(&response).unwrap_err().to_string(), MCP_STATE_ERROR);
        }
    }
}
