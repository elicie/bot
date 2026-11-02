//! Optional MCP Apps data chain. MCP transports stay owned by the source engine; the only
//! operations used here are its session-scoped `x.ai/mcp/read_resource`, `list`, and `call`.
//!
//! An app is data saved on a successful custom tool activity, never a grant attached to arbitrary
//! HTML. App-originated calls get a separate, expiring, one-use host approval. They do not consult
//! the model's `mcp_tool` allow rules. The pinned call proxy returns only a content/text subset:
//! structuredContent and result-level _meta were discarded upstream and are never synthesized.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use agent_client_protocol::{self as acp, Agent as _};
use base64::Engine as _;
use grok_bot_engine::host::EngineHost;
use grok_bot_model::{
    HttpError, HttpResult, MAX_MCP_APP_HTML_BYTES, MAX_MCP_APP_INPUT_BYTES, MAX_MCP_APP_OUTPUT_BYTES, MAX_MCP_APP_TOOL_CHARS, MAX_MCP_APP_URI_CHARS,
    MAX_MCP_APPS_PER_CONVERSATION, McpApp, ToolStatus, new_id,
};
use grok_bot_store::State;
use grok_bot_store::vault::ResolvedMcpServer;
use serde_json::{Value, json};

use super::{ENGINE_SETTLE_LIMIT, ProviderKey, Run, Runtime, custom_mcp::RunMcp, message_mut, text::redact};

const RESOURCE_TIMEOUT: Duration = Duration::from_secs(5);
const CALL_TIMEOUT: Duration = Duration::from_secs(25);
const APPROVAL_TTL: Duration = Duration::from_secs(120);
const MAX_PENDING: usize = 32;
const MAX_PENDING_PER_APP: usize = 8;
const MAX_EXT_RESPONSE_BYTES: usize = 1024 * 1024;
const UI_WARNING: &str = "MCP app unavailable; the ordinary tool result is preserved.";

struct Pending {
    conversation: String,
    activity: String,
    app: McpApp,
    name: String,
    arguments: Value,
    /// Private request-time credentials, used only for fail-closed rotation detection.
    resolved: ResolvedMcpServer,
    created: Instant,
}

#[derive(Default)]
pub(super) struct Requests {
    pending: HashMap<String, Pending>,
}

impl Requests {
    fn prune(&mut self, now: Instant) {
        self.pending.retain(|_, pending| now.saturating_duration_since(pending.created) < APPROVAL_TTL);
    }

    pub(super) fn cancel(&mut self, conversation: &str) {
        self.pending.retain(|_, pending| pending.conversation != conversation);
    }

    pub(super) fn clear(&mut self) {
        self.pending.clear();
    }
}

fn stale() -> HttpError {
    HttpError::conflict("This MCP app approval is no longer valid. Request a fresh approval.")
}

fn unavailable() -> HttpError {
    HttpError::unavailable("The MCP app call could not complete. Its server may be unavailable or its tool is not supported by this client.")
}

fn text_result(text: &str, error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": error})
}

fn valid_uri(uri: &str) -> bool {
    uri.starts_with("ui://")
        && uri.chars().count() <= MAX_MCP_APP_URI_CHARS
        && !uri.chars().any(|c| c.is_control() || c.is_whitespace())
        && url::Url::parse(uri).is_ok_and(|url| url.scheme() == "ui" && url.username().is_empty() && url.password().is_none() && url.fragment().is_none())
}

fn resource_uri(meta: &Value) -> Option<&str> {
    meta.get("ui").and_then(|ui| ui.get("resourceUri")).or_else(|| meta.get("ui/resourceUri")).and_then(Value::as_str)
}

pub(super) fn has_ui_metadata(update: &Value) -> bool {
    update["status"] == "completed"
        && update["rawOutput"]["type"] == "MCP"
        && (update["_meta"]["ui"].get("resourceUri").is_some() || update["_meta"].get("ui/resourceUri").is_some())
}

fn is_html_mime(mime: &str) -> bool {
    let mut parts = mime.split(';').map(str::trim);
    if !parts.next().is_some_and(|base| base.eq_ignore_ascii_case("text/html")) {
        return false;
    }
    let mut profile = false;
    for parameter in parts {
        let Some((name, value)) = parameter.split_once('=') else { return false };
        if name.trim().eq_ignore_ascii_case("profile") {
            if profile || value.trim().trim_matches('"') != "mcp-app" {
                return false;
            }
            profile = true;
        } else if !name.trim().eq_ignore_ascii_case("charset") || !value.trim().eq_ignore_ascii_case("utf-8") {
            return false;
        }
    }
    profile
}

fn resource_html(value: &Value, uri: &str) -> Option<String> {
    let contents = value.get("contents")?.as_array()?;
    if contents.len() != 1 {
        return None;
    }
    let content = &contents[0];
    if content["uri"].as_str()? != uri || !is_html_mime(content["mimeType"].as_str()?) {
        return None;
    }
    let html = match (content.get("text"), content.get("blob")) {
        (Some(Value::String(text)), None | Some(Value::Null)) => text.clone(),
        (None | Some(Value::Null), Some(Value::String(blob))) if blob.len() <= MAX_MCP_APP_HTML_BYTES.div_ceil(3) * 4 => {
            String::from_utf8(base64::engine::general_purpose::STANDARD.decode(blob).ok()?).ok()?
        }
        _ => return None,
    };
    (!html.trim().is_empty() && html.len() <= MAX_MCP_APP_HTML_BYTES && !html.contains('\0')).then_some(html)
}

fn truncate_bytes(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn redact_json(value: &mut Value, keys: &[String]) -> HttpResult<()> {
    match value {
        Value::String(text) => *text = redact(keys, text, None),
        Value::Array(items) => {
            for item in items {
                redact_json(item, keys)?;
            }
        }
        Value::Object(object) => {
            let old = std::mem::take(object);
            for (key, mut item) in old {
                redact_json(&mut item, keys)?;
                if object.insert(redact(keys, &key, None), item).is_some() {
                    return Err(HttpError::bad_request("The MCP app input cannot be represented safely after credential redaction."));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn redact_app(app: &mut McpApp, keys: &[String]) -> HttpResult<()> {
    // Credential-shaped identifiers must not be published or rewritten into another capability.
    if [&app.server, &app.tool, &app.uri].into_iter().any(|value| redact(keys, value, None) != *value) {
        return Err(unavailable());
    }
    let mut input: Value = serde_json::from_str(&app.input).map_err(|_| unavailable())?;
    redact_json(&mut input, keys)?;
    app.input = input.to_string();
    app.html = redact(keys, &app.html, None);
    let output = redact(keys, &app.output, None);
    app.output = truncate_bytes(&output, MAX_MCP_APP_OUTPUT_BYTES).to_owned();
    if !grok_bot_store::mcp_apps::persisted_mcp_app_valid(app) {
        return Err(HttpError::bad_request("The UI resource cannot be saved within this client's validated app format and size limits."));
    }
    Ok(())
}

async fn ext(connection: &acp::ClientSideConnection, method: &'static str, params: Value, timeout: Duration) -> HttpResult<Value> {
    let params = serde_json::value::to_raw_value(&params).map_err(|_| unavailable())?;
    let response = tokio::time::timeout(timeout, connection.ext_method(acp::ExtRequest::new(method, params.into())))
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?;
    if response.0.get().len() > MAX_EXT_RESPONSE_BYTES {
        return Err(HttpError::too_large("The MCP app server response exceeds the supported size limit."));
    }
    let envelope: Value = serde_json::from_str(response.0.get()).map_err(|_| unavailable())?;
    extension_result(envelope)
}

fn extension_result(mut envelope: Value) -> HttpResult<Value> {
    if envelope.get("error").is_some_and(|error| !error.is_null()) {
        return Err(unavailable());
    }
    envelope.get_mut("result").filter(|result| result.is_object()).map(Value::take).ok_or_else(unavailable)
}

async fn host_ext(host: &EngineHost, method: &'static str, params: Value, timeout: Duration) -> HttpResult<Value> {
    tokio::time::timeout(timeout + Duration::from_secs(1), host.call(move |connection| async move { ext(&connection, method, params, timeout).await }))
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?
}

/// Resolve from the stored artifact, not from any frame-supplied server, URL, bot, or session.
fn bound_app(state: &State, conversation_id: &str, activity_id: &str) -> HttpResult<(McpApp, ResolvedMcpServer)> {
    state.assert_writable()?;
    let conversation = state.conversation(conversation_id)?;
    let (message, activity) = conversation
        .messages
        .iter()
        .find_map(|message| message.tools.iter().find(|tool| tool.id == activity_id).map(|tool| (message, tool)))
        .ok_or_else(stale)?;
    let app = activity.mcp_app.as_ref().filter(|_| activity.status == ToolStatus::Completed && activity.local_only != Some(true)).ok_or_else(stale)?;
    let actor = message.speaker_bot_id.as_deref().unwrap_or(&conversation.bot_id);
    if actor != app.bot_id || state.bot(&app.bot_id).is_err() || !valid_uri(&app.uri) {
        return Err(stale());
    }
    let configured = state
        .mcp_servers_for(&app.bot_id)
        .find(|server| server.id == app.server_id && server.name == app.server && server.updated_at == app.server_updated_at && server.enabled)
        .ok_or_else(stale)?;
    let (servers, _) = state.resolve_mcp_servers(&app.bot_id);
    let resolved = servers.into_iter().find(|server| server.name == configured.name).ok_or_else(stale)?;
    Ok((app.clone(), resolved))
}

fn catalog_tool<'a>(catalog: &'a Value, app: &McpApp, name: &str) -> Option<&'a Value> {
    let server = catalog.get("servers")?.as_array()?.iter().find(|server| server["name"] == app.server)?;
    let session = server.get("session")?;
    if session["status"] != "ready" || session["enabled"] != true {
        return None;
    }
    let tools = session.get("tools")?.as_array()?;
    // The saved UI must still be the UI associated with its originating tool. A changed catalog
    // cannot silently turn an old persisted document into a new tool capability.
    let origin = tools.iter().find(|tool| tool["name"] == app.tool && tool["enabled"] != false)?;
    if resource_uri(&origin["_meta"]) != Some(app.uri.as_str()) {
        return None;
    }
    let tool = tools.iter().find(|tool| tool["name"] == name && tool["enabled"] != false)?;
    // Missing visibility defaults to model+app. The pinned catalog omits some app-only tools:
    // those remain unsupported, rather than bypassing this current-catalog admission check.
    if let Some(visibility) = tool["_meta"]["ui"].get("visibility")
        && !visibility.as_array()?.iter().any(|item| item == "app")
    {
        return None;
    }
    Some(tool)
}

fn call_result(value: Value, keys: &[String]) -> HttpResult<Value> {
    let content = value.get("content").and_then(Value::as_array).ok_or_else(unavailable)?;
    if content.len() > 64 {
        return Err(HttpError::too_large("The MCP app result contains too many content blocks."));
    }
    let mut remaining = MAX_MCP_APP_OUTPUT_BYTES;
    let mut texts = Vec::new();
    for block in content {
        if block["type"] != "text" {
            continue;
        }
        let text = block.get("text").and_then(Value::as_str).ok_or_else(unavailable)?;
        let text = redact(keys, text, None);
        if text.len() > remaining {
            return Ok(text_result("The MCP app result exceeds the 64 KiB text limit and was not delivered. The approved call was not retried.", true));
        }
        remaining -= text.len();
        texts.push(json!({"type": "text", "text": text}));
    }
    if texts.is_empty() {
        return Ok(text_result("This MCP result has no supported text content.", true));
    }
    let mut result = json!({"content": texts});
    if let Some(is_error) = value.get("isError").filter(|value| value.is_boolean()) {
        result["isError"] = is_error.clone();
    }
    Ok(result)
}

impl Runtime {
    /// Schedule bounded resource work without stalling the sole normal-update router. Finishing
    /// joins these turn-owned tasks before closing the originating session.
    pub(super) fn queue_mcp_app_capture(&self, run: &Arc<Run>, update: Value) {
        let call_id = update["toolCallId"].as_str().unwrap_or_default().to_owned();
        // Older messages have durable captures; this turn reserves its remaining slots in the task
        // vector. Never acquire the store lock while holding flags (projection does the reverse).
        let prior = self.store.read(|state| {
            state
                .conversation(&run.conversation_id)
                .map(|conversation| {
                    conversation
                        .messages
                        .iter()
                        .filter(|message| message.id != run.message_id)
                        .flat_map(|message| &message.tools)
                        .filter(|tool| tool.mcp_app.is_some())
                        .count()
                })
                .unwrap_or(MAX_MCP_APPS_PER_CONVERSATION)
        });
        let budget = MAX_MCP_APPS_PER_CONVERSATION.saturating_sub(prior);
        let mut flags = run.flags();
        if flags.stopped || flags.finishing || flags.mcp_app_captures_sealed || flags.mcp_app_captures.len() >= budget || !flags.mcp_app_seen.insert(call_id) {
            return;
        }
        let (runtime, run) = (self.arc(), Arc::clone(run));
        flags.mcp_app_captures.push(tokio::spawn(async move { runtime.capture_mcp_app(&run, &update).await }));
    }

    async fn capture_mcp_app(&self, run: &Arc<Run>, update: &Value) {
        let call_id = update["toolCallId"].as_str().unwrap_or_default().to_owned();
        if run.inactive() {
            return;
        }
        let captured = self.capture_app_data(run, update).await;
        let _ = self
            .persist(run, move |state, run| {
                if run.inactive() {
                    return Ok(());
                }
                let Some(public_id) = run.flags().tool_ids.get(&call_id).cloned() else { return Ok(()) };
                let count =
                    state.conversation(&run.conversation_id)?.messages.iter().flat_map(|message| &message.tools).filter(|tool| tool.mcp_app.is_some()).count();
                // The resource await may overlap a vault edit. Re-redact and validate under the
                // same state lock as this commit, including JSON keys/collisions, not only at read time.
                let captured = captured.and_then(|mut app| {
                    let mut keys = state.redaction_keys();
                    keys.extend(run.keys.iter().cloned());
                    redact_app(&mut app, &keys)?;
                    if !state
                        .mcp_servers_for(&run.bot.id)
                        .any(|server| server.id == app.server_id && server.updated_at == app.server_updated_at && server.enabled)
                    {
                        return Err(stale());
                    }
                    Ok(app)
                });
                let message = message_mut(state, run)?;
                let Some(tool) = message.tools.iter_mut().find(|tool| tool.id == public_id) else { return Ok(()) };
                if tool.mcp_app.is_some() {
                    return Ok(());
                }
                match captured {
                    Ok(app) if count < MAX_MCP_APPS_PER_CONVERSATION => tool.mcp_app = Some(app),
                    result => {
                        let output = tool.output.get_or_insert_with(String::new);
                        output.push_str("\n\n");
                        output.push_str(UI_WARNING);
                        if let Err(error) = result {
                            output.push(' ');
                            output.push_str(&error.message);
                        }
                    }
                }
                Ok(())
            })
            .await;
    }

    async fn capture_app_data(&self, run: &Arc<Run>, update: &Value) -> HttpResult<McpApp> {
        let raw = &update["rawOutput"];
        let (Some(server), Some(tool), Some(output), Some(uri)) =
            (raw["server_name"].as_str(), raw["tool_name"].as_str(), raw["output"]["OkayOutput"].as_str(), resource_uri(&update["_meta"]))
        else {
            return Err(unavailable());
        };
        if !run.mcp.has(server) || !valid_uri(uri) || tool.is_empty() || tool.chars().count() > MAX_MCP_APP_TOOL_CHARS || raw["is_error"] == true {
            return Err(unavailable());
        }
        let call_id = update["toolCallId"].as_str().ok_or_else(unavailable)?;
        let (record, mut input, mut keys) = self.store.read(|state| -> HttpResult<_> {
            let record = state.mcp_servers_for(&run.bot.id).find(|record| record.name == server && record.enabled).cloned().ok_or_else(stale)?;
            let conversation = state.conversation(&run.conversation_id)?;
            if conversation.messages.iter().flat_map(|message| &message.tools).filter(|tool| tool.mcp_app.is_some()).count() >= MAX_MCP_APPS_PER_CONVERSATION {
                return Err(HttpError::too_large("This conversation already contains the maximum number of MCP apps."));
            }
            let public_id = run.flags().tool_ids.get(call_id).cloned().ok_or_else(unavailable)?;
            let activity = conversation
                .messages
                .iter()
                .find(|message| message.id == run.message_id)
                .and_then(|message| message.tools.iter().find(|tool| tool.id == public_id))
                .ok_or_else(unavailable)?;
            if activity.local_only == Some(true) {
                return Err(HttpError::forbidden("Local-only tool details cannot be exposed through an MCP app."));
            }
            let input: Value = serde_json::from_str(activity.input.as_deref().ok_or_else(unavailable)?).map_err(|_| unavailable())?;
            let input = input.get("tool_input").or_else(|| input.get("toolInput")).cloned().filter(Value::is_object).ok_or_else(unavailable)?;
            Ok((record, input, state.redaction_keys()))
        })?;
        keys.extend(run.keys.iter().cloned());
        redact_json(&mut input, &keys)?;
        let input = input.to_string();
        if input.len() > MAX_MCP_APP_INPUT_BYTES {
            return Err(HttpError::too_large("The MCP app input is too large."));
        }
        let session = run.flags().session_id.clone().ok_or_else(unavailable)?;
        let engine = self.engine.current().await.ok_or_else(unavailable)?;
        let resource = host_ext(&engine.host, "x.ai/mcp/read_resource", json!({"sessionId": session, "server": server, "uri": uri}), RESOURCE_TIMEOUT).await?;
        let html = resource_html(&resource, uri)
            .ok_or_else(|| HttpError::bad_request("The UI resource must contain one matching ui:// HTML document with the MCP app MIME profile."))?;
        keys.extend(self.store.read(|state| state.redaction_keys()));
        let html = redact(&keys, &html, None);
        let public_uri = redact(&keys, uri, None);
        if html.len() > MAX_MCP_APP_HTML_BYTES || public_uri != uri {
            return Err(unavailable());
        }
        let output = redact(&keys, output, None);
        let app = McpApp {
            bot_id: run.bot.id.clone(),
            server_id: record.id,
            server_updated_at: record.updated_at,
            server: server.to_owned(),
            tool: tool.to_owned(),
            uri: uri.to_owned(),
            html,
            input,
            output: truncate_bytes(&output, MAX_MCP_APP_OUTPUT_BYTES).to_owned(),
        };
        if !grok_bot_store::mcp_apps::persisted_mcp_app_valid(&app) {
            return Err(HttpError::bad_request("The UI resource cannot be saved within this client's validated app format and size limits."));
        }
        // Configuration can be tightened while a turn is running; never save a capability binding
        // to a now-disabled or reconfigured server.
        self.store
            .read(|state| {
                state.mcp_servers_for(&run.bot.id).any(|record| record.id == app.server_id && record.updated_at == app.server_updated_at && record.enabled)
            })
            .then_some(app)
            .ok_or_else(stale)
    }

    /// Create a proposal only. No MCP connection is started and no permission rule is consulted.
    pub fn request_mcp_app_call(&self, conversation: &str, activity: &str, name: &str, arguments: Value) -> HttpResult<String> {
        if self.closing.load(Ordering::SeqCst) {
            return Err(HttpError::unavailable("The server is shutting down."));
        }
        if !arguments.is_object() || arguments.to_string().len() > MAX_MCP_APP_INPUT_BYTES {
            return Err(HttpError::bad_request("MCP app arguments must be a JSON object of at most 32 KiB."));
        }
        if name.is_empty()
            || name.len() > MAX_MCP_APP_TOOL_CHARS
            || name.contains("__")
            || !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte))
        {
            return Err(HttpError::bad_request("Choose a bounded, unqualified MCP tool name."));
        }
        let (app, resolved) = self.store.read(|state| bound_app(state, conversation, activity))?;
        let mut requests = self.mcp_app_requests.lock().unwrap_or_else(|p| p.into_inner());
        requests.prune(Instant::now());
        if requests.pending.len() >= MAX_PENDING
            || requests.pending.values().filter(|request| request.conversation == conversation && request.activity == activity).count() >= MAX_PENDING_PER_APP
        {
            return Err(HttpError::conflict("Too many MCP app requests are awaiting approval. Deny them or let them expire first."));
        }
        let id = new_id();
        requests.pending.insert(
            id.clone(),
            Pending {
                conversation: conversation.to_owned(),
                activity: activity.to_owned(),
                app,
                name: name.to_owned(),
                arguments,
                resolved,
                created: Instant::now(),
            },
        );
        Ok(id)
    }

    /// The host user's explicit allow/deny decision. Consumes the exact proposal once, including on
    /// failure or busy rejection. The queue is acquired only here, never while asking the user.
    pub async fn decide_mcp_app_call(&self, conversation: &str, activity: &str, request_id: &str, allow: bool) -> HttpResult<Value> {
        let pending = {
            let mut requests = self.mcp_app_requests.lock().unwrap_or_else(|p| p.into_inner());
            requests.prune(Instant::now());
            let pending =
                requests.pending.get(request_id).filter(|pending| pending.conversation == conversation && pending.activity == activity).ok_or_else(stale)?;
            if pending.created.elapsed() >= APPROVAL_TTL {
                return Err(stale());
            }
            requests.pending.remove(request_id).ok_or_else(stale)?
        };
        if !allow {
            return Ok(text_result("The user denied this MCP app tool call. It did not run.", true));
        }
        let runtime = self.arc();
        // Keep the queue operation alive if the browser disconnects; its bounded engine job must
        // finish and close before configuration/scheduler mutations can acquire the slot.
        tokio::spawn(async move { runtime.queue.run(runtime.execute_mcp_app_call(pending)).await }).await.map_err(|_| unavailable())?
    }

    async fn execute_mcp_app_call(&self, pending: Pending) -> HttpResult<Value> {
        if self.mcp_app_busy.swap(true, Ordering::SeqCst) {
            return Err(HttpError::conflict("An MCP app call is already running."));
        }
        struct Busy(Arc<Runtime>);
        impl Drop for Busy {
            fn drop(&mut self) {
                self.0.mcp_app_busy.store(false, Ordering::SeqCst);
            }
        }
        let busy = Busy(self.arc());
        if pending.created.elapsed() >= APPROVAL_TTL {
            return Err(stale());
        }
        if self.closing.load(Ordering::SeqCst) {
            return Err(HttpError::unavailable("The server is shutting down."));
        }
        let (resolved, settings) = self.store.read(|state| -> HttpResult<_> {
            state.assert_idle()?;
            let (current, resolved) = bound_app(state, &pending.conversation, &pending.activity)?;
            if current != pending.app || resolved != pending.resolved {
                return Err(stale());
            }
            Ok((resolved, state.settings().clone()))
        })?;
        let servers = RunMcp::new(vec![resolved], Vec::new());
        if servers.servers.len() != 1 {
            return Err(unavailable());
        }
        let parent = grok_bot_files::private_directory(&self.store.data_dir().join("grok").join(&pending.conversation))?;
        let temporary = tempfile::Builder::new().prefix("mcp-app-").tempdir_in(parent).map_err(|_| unavailable())?;
        let cwd = grok_bot_files::private_directory(&temporary.path().join("workspace"))?;
        for directory in [".git/objects", ".git/refs/heads"] {
            grok_bot_files::private_directory(&cwd.join(directory))?;
        }
        grok_bot_files::atomic_private_file(&cwd.join(".git/HEAD"), b"ref: refs/heads/main\n").map_err(|_| unavailable())?;
        grok_bot_files::atomic_private_file(&cwd.join(".git/config"), b"[core]\nrepositoryformatversion = 0\nbare = false\n").map_err(|_| unavailable())?;
        let mut meta = json!({
            "yoloMode": false, "autoMode": false,
            "agentProfile": {
                "name": "grok-bot-mcp-app", "description": "One explicitly approved MCP app action", "promptMode": "extend",
                "injectDefaultTools": false, "agentsMd": false,
                "toolConfig": {"tools": [{"id": "GrokBuild:search_tool"}, {"id": "GrokBuild:use_tool"}]}, "tools": ["search_tool", "use_tool"],
                "disallowedTools": super::DENIED_BUILTIN_TOOLS, "discoverSkills": false, "inheritSkills": false, "permissionMode": "default"
            }
        });
        meta["mcpConfig"] = servers.meta_config().unwrap_or_default();
        let request = acp::NewSessionRequest::new(cwd).mcp_servers(servers.acp_servers()).meta(meta.as_object().cloned());
        let engine = self
            .engine
            .acquire(&self.arc(), &self.options.engine_root, &ProviderKey::from_settings(&settings), self.relay.get())
            .await
            .map_err(|_| unavailable())?;
        let runtime = self.arc();
        let startup = self.options.startup_timeout.min(Duration::from_secs(15));
        engine
            .host
            .call(move |connection| async move {
                // The job, not the HTTP future, owns admission and the private directory until
                // close. Dropping a disconnected HTTP caller must not allow an overlapping turn.
                let _busy = busy;
                let _temporary = temporary;
                let created = tokio::time::timeout(startup, connection.new_session(request))
                    .await
                    .map_err(|_| HttpError::unavailable("The isolated MCP app session did not open before the deadline."))?
                    .map_err(|_| HttpError::unavailable("The isolated MCP app session could not be opened."))?;
                let session = created.session_id;
                let result = async {
                    let deadline = tokio::time::Instant::now() + startup;
                    loop {
                        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                        if left.is_zero() {
                            return Err(unavailable());
                        }
                        let catalog =
                            ext(&connection, "x.ai/mcp/list", json!({"sessionId": session.0.as_ref(), "cache": true}), left.min(Duration::from_secs(2)))
                                .await?;
                        if catalog_tool(&catalog, &pending.app, &pending.name).is_some() {
                            break;
                        }
                        let target = catalog["servers"].as_array().and_then(|servers| servers.iter().find(|server| server["name"] == pending.app.server));
                        let status = target.and_then(|server| server["session"]["status"].as_str());
                        if matches!(status, Some("unavailable" | "setuprequired")) {
                            return Err(unavailable());
                        }
                        let has_tools = target.and_then(|server| server["session"]["tools"].as_array()).is_some_and(|tools| !tools.is_empty());
                        if catalog["sessionMcpResolved"] == true && status == Some("ready") && has_tools {
                            return Err(HttpError::forbidden("The requested MCP tool is not an enabled, app-visible tool in this server's current catalog."));
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    if runtime.closing.load(Ordering::SeqCst) {
                        return Err(stale());
                    }
                    let mut keys = runtime.store.read(|state| -> HttpResult<_> {
                        let (current, resolved) = bound_app(state, &pending.conversation, &pending.activity)?;
                        if current != pending.app || resolved != pending.resolved {
                            return Err(stale());
                        }
                        Ok(state.redaction_keys())
                    })?;
                    let value = ext(
                        &connection,
                        "x.ai/mcp/call",
                        json!({
                            "sessionId": session.0.as_ref(), "server": pending.app.server,
                            "tool": pending.name, "arguments": pending.arguments
                        }),
                        CALL_TIMEOUT,
                    )
                    .await?;
                    keys.extend(runtime.store.read(|state| state.redaction_keys()));
                    call_result(value, &keys)
                }
                .await;
                let _ = tokio::time::timeout(ENGINE_SETTLE_LIMIT, connection.close_session(acp::CloseSessionRequest::new(session))).await;
                result
            })
            .await
            .map_err(|_| unavailable())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_capture_is_exact_bounded_and_html_only() {
        let uri = "ui://fixture/app.html";
        let resource = json!({"contents": [{"uri": uri, "mimeType": "text/html;profile=mcp-app", "text": "<html>ok</html>"}]});
        assert_eq!(resource_html(&resource, uri).as_deref(), Some("<html>ok</html>"));
        assert!(resource_html(&resource, "ui://other/app").is_none());
        assert!(is_html_mime("text/html; profile=\"mcp-app\"; charset=utf-8"));
        for mime in ["text/html", "text/javascript", "text/html;profile=other", "text/html;profile=mcp-app;profile=mcp-app"] {
            assert!(!is_html_mime(mime));
        }
        let mut invalid = resource.clone();
        invalid["contents"][0]["text"] = json!("x".repeat(MAX_MCP_APP_HTML_BYTES + 1));
        assert!(resource_html(&invalid, uri).is_none());
        let encoded = base64::engine::general_purpose::STANDARD.encode("<html>blob</html>");
        let blob = json!({"contents": [{"uri": uri, "mimeType": "text/html;profile=mcp-app", "blob": encoded}]});
        assert_eq!(resource_html(&blob, uri).as_deref(), Some("<html>blob</html>"));
        assert!(!valid_uri("https://fixture/app.html"));
        assert!(!valid_uri("ui://user:password@fixture/app.html"));
        assert!(!valid_uri("ui://fixture/app.html#fragment"));
    }

    #[test]
    fn json_redaction_refuses_key_collisions_and_catches_escaped_values() {
        let secret = "quoted-secret-\"-\\-value";
        let mut input = json!({"nested": [{"value": secret}]});
        redact_json(&mut input, &[secret.into()]).unwrap();
        assert_eq!(input, json!({"nested": [{"value": "[redacted]"}]}));
        let mut collision = json!({"nested": {"secret-token": 1, "[redacted]": 2}});
        assert!(redact_json(&mut collision, &["secret-token".into()]).is_err());
    }

    #[test]
    fn source_engine_extension_envelopes_are_unwrapped_once() {
        assert_eq!(extension_result(json!({"result": {"contents": []}})).unwrap(), json!({"contents": []}));
        assert!(extension_result(json!({"result": null, "error": "server unavailable"})).is_err());
        assert!(extension_result(json!({"result": {"contents": []}, "error": "partial failure"})).is_err());
        assert!(extension_result(json!({"contents": []})).is_err());
    }

    #[test]
    fn requests_expire_and_cancel_without_durable_permission() {
        let app = McpApp {
            bot_id: "bot".into(),
            server_id: "record".into(),
            server_updated_at: "version".into(),
            server: "apps".into(),
            tool: "render".into(),
            uri: "ui://fixture/app".into(),
            html: "<p>ui</p>".into(),
            input: "{}".into(),
            output: "text".into(),
        };
        let resolved = ResolvedMcpServer {
            name: "apps".into(),
            config: grok_bot_model::McpServerConfig::Http { url: "http://127.0.0.1/mcp".into(), headers: Vec::new() },
        };
        let pending = |conversation: &str, created| Pending {
            conversation: conversation.into(),
            activity: "activity".into(),
            app: app.clone(),
            name: "render".into(),
            arguments: json!({}),
            resolved: resolved.clone(),
            created,
        };
        let now = Instant::now();
        let mut requests = Requests::default();
        requests.pending.insert("expired".into(), pending("first", now - APPROVAL_TTL));
        requests.pending.insert("live".into(), pending("first", now));
        requests.pending.insert("other".into(), pending("second", now));
        requests.prune(now);
        assert!(!requests.pending.contains_key("expired"));
        requests.cancel("first");
        assert!(!requests.pending.contains_key("live"));
        assert!(requests.pending.contains_key("other"));
        requests.clear();
        assert!(requests.pending.is_empty());
    }

    #[test]
    fn proxy_exposes_only_redacted_bounded_text_not_reconstructed_metadata() {
        let result = call_result(json!({"content": [{"type": "text", "text": "secret123"}, {"type": "resource", "text": "html"}], "isError": false, "structuredContent": {"secret": true}, "_meta": {"secret": true}}), &["secret123".into()]).unwrap();
        assert_eq!(result, json!({"content": [{"type": "text", "text": "[redacted]"}], "isError": false}));
        let huge = call_result(json!({"content": [{"type": "text", "text": "한".repeat(MAX_MCP_APP_OUTPUT_BYTES)}]}), &[]).unwrap();
        assert_eq!(huge["isError"], true);
        assert!(huge["content"][0]["text"].as_str().unwrap().contains("not delivered"));
    }
}
