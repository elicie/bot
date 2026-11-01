//! In-process Grok Build engine host.
//!
//! `MvpAgent` is `!Send` (it is built on `Rc`), so it lives on a dedicated OS thread that runs a
//! current-thread Tokio runtime with a `LocalSet`. The agent and an ACP client talk over in-memory
//! duplex pipes, which mirrors upstream's own in-process harness (`agent/testkit/e2e.rs`) and keeps
//! ACP, upstream's public contract, as the only boundary. Callers on any thread submit closures that
//! run against the client connection and receive their results through oneshot channels.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::thread::JoinHandle;

use agent_client_protocol::{self as acp, Agent as _};
use anyhow::{Context, Result, anyhow};
use futures::future::LocalBoxFuture;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use xai_acp_lib::{AcpAgentGatewayReceiver, AcpAgentGatewaySender, LineBufferedRead};
use xai_grok_shell::agent::config::{AgentMode, Config, RuntimeResolutionContext};
use xai_grok_shell::agent::mvp_agent::MvpAgent;

const DUPLEX_BUFFER_BYTES: usize = 16 * 1024 * 1024;

/// One OpenAI-compatible model the engine may select. `id` is the engine-side alias.
#[derive(Clone, Debug)]
pub struct ModelSpec {
    pub id: String,
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
}

#[derive(Clone, Debug)]
pub struct EngineSettings {
    /// Engine `GROK_HOME`. Must match the value passed to [`prepare_process_environment`].
    pub grok_home: PathBuf,
    pub models: Vec<ModelSpec>,
    pub default_model: String,
    /// Additional permission allow rules, e.g. `MCPTool(workspace__*)`.
    pub allow: Vec<String>,
    pub inference_idle_timeout_secs: u64,
}

/// Events the engine sends to the application, in arrival order.
pub enum EngineEvent {
    Update(acp::SessionNotification),
    Permission {
        request: acp::RequestPermissionRequest,
        respond: oneshot::Sender<acp::RequestPermissionResponse>,
    },
}

type Job = Box<dyn FnOnce(Rc<acp::ClientSideConnection>) -> LocalBoxFuture<'static, ()> + Send>;

pub struct EngineHost {
    jobs: Option<mpsc::UnboundedSender<Job>>,
    thread: Option<JoinHandle<()>>,
    pub initialize: acp::InitializeResponse,
}

/// Engine-wide environment. Upstream reads these from the process environment, so they must be set
/// once, before the application starts other threads.
///
/// # Safety
/// Mutates the process environment; call before spawning threads that read it concurrently.
pub unsafe fn prepare_process_environment(engine_root: &Path) {
    let home = engine_root.join("home");
    let vars: [(&str, PathBuf); 6] = [
        ("HOME", home.clone()),
        ("GROK_HOME", home.join(".grok")),
        ("XDG_CONFIG_HOME", home.join(".config")),
        ("XDG_DATA_HOME", home.join(".local/share")),
        ("XDG_CACHE_HOME", home.join(".cache")),
        ("TMPDIR", home.join("tmp")),
    ];
    for (key, value) in vars {
        let _ = std::fs::create_dir_all(&value);
        unsafe { std::env::set_var(key, value) };
    }
    for (key, value) in [
        ("GROK_DISABLE_AUTOUPDATER", "1"),
        ("GROK_REMOTE_FETCH", "0"),
        ("GROK_CAMPAIGNS", "0"),
        ("GROK_TURN_TRANSIENT_RETRY", "0"),
        ("GROK_MAX_RETRIES", "0"),
        ("GROK_TELEMETRY_ENABLED", "0"),
        ("GROK_TELEMETRY_TRACE_UPLOAD", "0"),
        ("GROK_EXTERNAL_OTEL", "0"),
        ("GROK_SUBAGENTS", "0"),
        ("GROK_WORKFLOWS", "0"),
        ("GROK_DEFAULT_SELECTED_PERMISSION", "reject"),
        ("GROK_CURSOR_MCPS_ENABLED", "0"),
        ("GROK_CLAUDE_MCPS_ENABLED", "0"),
        ("GROK_CURSOR_HOOKS_ENABLED", "0"),
        ("GROK_CLAUDE_HOOKS_ENABLED", "0"),
    ] {
        unsafe { std::env::set_var(key, value) };
    }
}

fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

/// Application-owned engine configuration: custom chat-completions models only, every ambient
/// integration disabled. Mirrors the configuration the Node runtime used to generate.
pub fn config_toml(settings: &EngineSettings) -> String {
    let ids: Vec<String> = settings.models.iter().map(|m| toml_string(&m.id)).collect();
    let mut out = format!(
        "[models]\ndefault = {default}\nsession_summary = {default}\nallowed_models = [{ids}]\n",
        default = toml_string(&settings.default_model),
        ids = ids.join(", "),
    );
    for model in &settings.models {
        out.push_str(&format!(
            "[model.{id}]\nmodel = {model}\nbase_url = {base}\napi_backend = \"chat_completions\"\napi_key = {key}\nmax_retries = 0\ninference_idle_timeout_secs = {idle}\n",
            id = toml_string(&model.id),
            model = toml_string(&model.model),
            base = toml_string(&model.base_url),
            // Keyless local servers still need a bearer value for the core's startup auth.
            key = toml_string(model.api_key.as_deref().unwrap_or("grok-bot-loopback-no-auth")),
            idle = settings.inference_idle_timeout_secs,
        ));
    }
    let allow: Vec<String> = settings.allow.iter().map(|rule| toml_string(rule)).collect();
    out.push_str(&format!(
        r#"[auth]
preferred_method = "api_key"
[cli]
use_leader = false
auto_update = false
session_registry = false
[ui]
permission_mode = "ask"
remember_tool_approvals = false
[permission]
allow = [{allow}]
deny = ["Bash", "Read(/**)", "Read(~*)", "Read(~*/**)", "Edit", "Grep", "WebSearch", "WebFetch"]
[features]
turn_transient_retry = false
remote_fetch = false
managed_config = false
campaigns = false
telemetry = false
backend_tools = false
web_fetch = false
ask_user_question = false
write_file = false
image_gen = false
video_gen = false
lsp_tools = false
file_acceleration = false
codebase_indexing = false
mcp_recursive_config_watch = false
remember_mode = false
subagent_bundle = false
session_recap = false
title_refresh = false
turn_summary = false
[telemetry]
trace_upload = false
otel_enabled = false
[managed_mcps]
enabled = false
gateway_tools_enabled = false
[subagents]
enabled = false
[workflows]
enabled = false
[relay]
enabled = false
[session]
load_envrc = false
[toolset.bash]
login_shell_capture = false
[shell_environment_policy]
inherit = "none"
ignore_default_excludes = false
[compat.claude]
agents = false
hooks = false
mcps = false
rules = false
skills = false
[compat.cursor]
agents = false
hooks = false
mcps = false
rules = false
skills = false
[compat.codex]
hooks = false
skills = false
"#,
        allow = allow.join(", "),
    ));
    out
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

struct AppClient {
    events: mpsc::UnboundedSender<EngineEvent>,
}

#[async_trait::async_trait(?Send)]
impl acp::Client for AppClient {
    async fn request_permission(
        &self,
        request: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let (respond, decision) = oneshot::channel();
        if self.events.send(EngineEvent::Permission { request, respond }).is_err() {
            return Ok(acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Cancelled));
        }
        Ok(decision.await.unwrap_or_else(|_| {
            acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Cancelled)
        }))
    }

    async fn session_notification(&self, notification: acp::SessionNotification) -> acp::Result<()> {
        let _ = self.events.send(EngineEvent::Update(notification));
        Ok(())
    }
}

impl EngineHost {
    /// Start the engine thread, initialize the in-process agent, and return once ACP `initialize`
    /// succeeded. Events are delivered on the returned receiver.
    pub async fn start(settings: EngineSettings) -> Result<(Self, mpsc::UnboundedReceiver<EngineEvent>)> {
        let input = config_toml(&settings);
        write_private(&settings.grok_home.join("config.toml"), &input)
            .context("Cannot write the engine configuration")?;
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (jobs_tx, jobs_rx) = mpsc::unbounded_channel::<Job>();
        let (ready_tx, ready_rx) = oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("grok-engine".into())
            .spawn(move || engine_thread(input, events_tx, jobs_rx, ready_tx))
            .context("Cannot start the engine thread")?;
        let initialize = match ready_rx.await {
            Ok(Ok(initialize)) => initialize,
            Ok(Err(error)) => {
                let _ = thread.join();
                return Err(error);
            }
            Err(_) => {
                let _ = thread.join();
                return Err(anyhow!("The engine thread stopped during startup"));
            }
        };
        Ok((Self { jobs: Some(jobs_tx), thread: Some(thread), initialize }, events_rx))
    }

    /// Run `job` on the engine thread with the ACP client connection and return its output.
    pub async fn call<F, Fut, T>(&self, job: F) -> Result<T>
    where
        F: FnOnce(Rc<acp::ClientSideConnection>) -> Fut + Send + 'static,
        Fut: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let job: Job = Box::new(move |connection| {
            Box::pin(async move {
                let _ = tx.send(job(connection).await);
            })
        });
        self.jobs
            .as_ref()
            .context("The engine is shut down")?
            .send(job)
            .map_err(|_| anyhow!("The engine thread is not running"))?;
        rx.await.map_err(|_| anyhow!("The engine thread stopped before answering"))
    }

    pub async fn new_session(&self, request: acp::NewSessionRequest) -> Result<acp::NewSessionResponse> {
        Ok(self.call(move |c| async move { c.new_session(request).await }).await??)
    }

    pub async fn load_session(&self, request: acp::LoadSessionRequest) -> Result<acp::LoadSessionResponse> {
        Ok(self.call(move |c| async move { c.load_session(request).await }).await??)
    }

    pub async fn set_session_model(
        &self,
        request: acp::SetSessionModelRequest,
    ) -> Result<acp::SetSessionModelResponse> {
        Ok(self.call(move |c| async move { c.set_session_model(request).await }).await??)
    }

    pub async fn prompt(&self, request: acp::PromptRequest) -> Result<acp::PromptResponse> {
        Ok(self.call(move |c| async move { c.prompt(request).await }).await??)
    }

    pub async fn cancel(&self, session_id: acp::SessionId) -> Result<()> {
        Ok(self
            .call(move |c| async move { c.cancel(acp::CancelNotification::new(session_id)).await })
            .await??)
    }

    /// Stop the engine thread. In-flight calls fail.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        self.jobs.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for EngineHost {
    fn drop(&mut self) {
        self.stop();
    }
}

fn engine_thread(
    input: String,
    events: mpsc::UnboundedSender<EngineEvent>,
    mut jobs: mpsc::UnboundedReceiver<Job>,
    ready: oneshot::Sender<Result<acp::InitializeResponse>>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(error.into()));
            return;
        }
    };
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async move {
        xai_fsnotify::set_runtime_handle(tokio::runtime::Handle::current());
        let cancel = tokio_util::sync::CancellationToken::new();
        let _cancel_on_exit = cancel.clone().drop_guard();
        let connection = match start_agent(&input, events, &cancel).await {
            Ok(started) => started,
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        };
        let (connection, initialize) = connection;
        if ready.send(Ok(initialize)).is_err() {
            return;
        }
        while let Some(job) = jobs.recv().await {
            tokio::task::spawn_local(job(Rc::clone(&connection)));
        }
    });
}

async fn start_agent(
    input: &str,
    events: mpsc::UnboundedSender<EngineEvent>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(Rc<acp::ClientSideConnection>, acp::InitializeResponse)> {
    let document: toml::Value = toml::from_str(input).context("Invalid engine configuration")?;
    let mut config = Config::new_from_toml_cfg(&document).map_err(anyhow::Error::msg)?;
    config.resolve_runtime_fields(&RuntimeResolutionContext {
        raw_config: &document,
        remote_settings: None,
        is_headless: false,
        cli_subagents: Some(false),
        cli_web_search_model: None,
        cli_session_summary_model: None,
        memory_enabled_override: Some(false),
        disable_web_search: true,
        todo_gate: false,
        laziness_debug_log: None,
        storage_mode: None,
    });
    config.mode = AgentMode::Stdio;
    config.client_version = Some(format!("grok-bot-q {}", env!("CARGO_PKG_VERSION")));
    config.default_yolo_mode = false;
    config.default_auto_mode = false;
    xai_grok_shell::agent::app::suppress_otel();

    let auth_manager = Arc::new(config.create_auth_manager());
    let boot = xai_grok_shell::agent::init::resolve_boot_startup_settings(
        &mut config,
        cancel,
        true,
        auth_manager.current(),
    )
    .await
    .map_err(|error| anyhow!("Engine bootstrap failed: {error}"))?;
    let (gateway_tx, gateway_rx) = mpsc::unbounded_channel();
    let agent = MvpAgent::new(AcpAgentGatewaySender::new(gateway_tx), &config, auth_manager, None, Some(boot))
        .map_err(|error| anyhow!("Engine configuration rejected: {error}"))?;
    let agent = Rc::new(agent);

    let (client_to_agent, agent_from_client) = tokio::io::duplex(DUPLEX_BUFFER_BYTES);
    let (agent_to_client, client_from_agent) = tokio::io::duplex(DUPLEX_BUFFER_BYTES);
    let agent_incoming = LineBufferedRead::spawn_local(agent_from_client.compat());
    let (agent_connection, agent_io) = acp::AgentSideConnection::new(
        Rc::clone(&agent),
        agent_to_client.compat_write(),
        agent_incoming,
        |future| {
            tokio::task::spawn_local(future);
        },
    );
    tokio::task::spawn_local(AcpAgentGatewayReceiver::new(gateway_rx, agent_connection).run());
    tokio::task::spawn_local(async move {
        // Keep the agent alive for the lifetime of its I/O loop.
        let _agent = agent;
        let _ = agent_io.await;
    });

    let client_incoming = LineBufferedRead::spawn_local(client_from_agent.compat());
    let (client, client_io) = acp::ClientSideConnection::new(
        AppClient { events },
        client_to_agent.compat_write(),
        client_incoming,
        |future| {
            tokio::task::spawn_local(future);
        },
    );
    tokio::task::spawn_local(async move {
        let _ = client_io.await;
    });

    let initialize = client
        .initialize(
            acp::InitializeRequest::new(acp::ProtocolVersion::V1)
                .client_capabilities(acp::ClientCapabilities::new().fs(acp::FileSystemCapabilities::new()).terminal(false))
                .client_info(acp::Implementation::new("grok-bot-q", env!("CARGO_PKG_VERSION"))),
        )
        .await
        .map_err(|error| anyhow!("Engine initialize failed: {error}"))?;
    Ok((Rc::new(client), initialize))
}
