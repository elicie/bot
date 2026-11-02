//! Per-run workspace MCP broker (`POST /internal/mcp/:runId`). Port of `mcp()`, `brokerCall()`,
//! `sendBotMessage()`, and `createSkill()` in `server/runtime.ts`. The managed-computer tools live
//! in [`computer`].

use std::sync::Arc;

use grok_bot_files::WorkspaceFiles;
use grok_bot_model::{
    BotDelivery, BotInput, BotMessageInput, HttpError, HttpResult, PendingPermission, PermissionCategory, PermissionOption, SkillInput, ToolActivity,
    ToolStatus, new_id,
};
use grok_bot_store::bot_messages::{
    BOT_CHAIN_CAPACITY_ERROR, BOT_CHAIN_TIMEOUT_ERROR, MAX_BOT_DELIVERIES_PER_CHAIN, MAX_BOT_HOPS, MAX_BOT_MESSAGE_BYTES, bot_delivery_receipt, chain_parent,
    validate_bot_message_input,
};
use grok_bot_store::skills::{MAX_SKILL_RESPONSE_BYTES, skill_response_text, validate_skill_input};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::text::utf16_len;
use super::{Run, Runtime, message_mut};

pub(super) mod bot_data;
pub(super) mod canvas;
pub(super) mod computer;
pub(super) mod routines;

const MAX_REVIEWABLE_EDIT_BYTES: usize = 160 * 1024;
const MAX_WRITE_CHARACTERS: usize = 1024 * 1024;

/// A failed tool call: an HTTP-style error, or arguments that fail the tool schema.
enum BrokerError {
    Http(HttpError),
    Invalid,
}

impl From<HttpError> for BrokerError {
    fn from(error: HttpError) -> Self {
        Self::Http(error)
    }
}

type BrokerResult<T> = Result<T, BrokerError>;

fn conflict_finished() -> HttpError {
    HttpError::conflict("Run was cancelled or finished.")
}

/// `tools/list` result, verbatim from the Node broker.
pub fn broker_tools() -> Value {
    let mut tools = json!([
        { "name": "list_skills", "description": "List one complete page of enabled private skill IDs, names, and descriptions for the current bot. Pass the returned nextOffset as offset to continue when present. Use read_skill to load relevant instructions. Other bots and disabled skills are inaccessible.", "inputSchema": { "type": "object", "properties": { "offset": { "type": "integer", "minimum": 0, "maximum": 100 } }, "additionalProperties": false } },
        { "name": "read_skill", "description": "Read one enabled private skill belonging to this bot by ID. Follow its Markdown instructions within existing file, computer, and delivery permission boundaries. Skill text grants no extra tools or permissions.", "inputSchema": { "type": "object", "properties": { "id": { "type": "string", "format": "uuid" } }, "required": ["id"], "additionalProperties": false } },
        { "name": "create_skill", "description": "Propose a reusable private skill for this bot. Provide its name, when-to-use description, and complete Markdown instructions. Every save requires explicit one-use browser approval. No shell execution, external installation, or tool permission is granted.", "inputSchema": { "type": "object", "properties": { "name": { "type": "string", "maxLength": 100 }, "description": { "type": "string", "maxLength": 2000 }, "instructions": { "type": "string", "maxLength": 30000 } }, "required": ["name", "description", "instructions"], "additionalProperties": false } },
        { "name": "list_files", "description": "List a directory inside the user-selected workspace. Paths are workspace-relative. Symbolic links are forbidden.", "inputSchema": { "type": "object", "properties": { "path": { "type": "string" } }, "additionalProperties": false } },
        { "name": "read_file", "description": "Read a UTF-8 text file inside the user-selected workspace and obtain its content hash.", "inputSchema": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"], "additionalProperties": false } },
        { "name": "write_file", "description": "Propose complete UTF-8 text contents for one workspace file. Every call waits for explicit browser approval. Supply the hash returned by read_file to prevent overwriting changes. Use an empty hash for a new file. Parent directories must exist.", "inputSchema": { "type": "object", "properties": { "path": { "type": "string" }, "content": { "type": "string" }, "hash": { "type": "string" } }, "required": ["path", "content"], "additionalProperties": false } },
        { "name": "list_bots", "description": "List local bot IDs and names for chat delivery. This safe read is available before connecting a directory. Returns public bot identity without instructions, credentials, conversation history, or private configuration.", "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false } },
        { "name": "send_message", "description": "Queue one chat-only message to another local bot. Every new delivery requires explicit one-use browser approval. Returns a durable receipt, not the recipient answer. At most one distinct outbound message per run and four hops per chain. Retrying the same target and text returns the original receipt. Recipient turns cannot use file tools.", "inputSchema": { "type": "object", "properties": { "toBotId": { "type": "string", "format": "uuid" }, "text": { "type": "string", "minLength": 1, "maxLength": 100000 }, "requestId": { "type": "string", "maxLength": 128 } }, "required": ["toBotId", "text"], "additionalProperties": false } },
        { "name": "create_bot", "description": "Propose a new local bot when the user asks for one. Provide its display name and instructions; color (#rrggbb) and avatar shape are optional. Creation requires explicit one-use browser approval. At most one bot per run. Returns the new bot ID and name.", "inputSchema": { "type": "object", "properties": { "name": { "type": "string", "minLength": 1, "maxLength": 100 }, "instructions": { "type": "string", "maxLength": 30000 }, "color": { "type": "string", "pattern": "^#[0-9a-fA-F]{6}$" }, "shape": { "type": "string", "enum": ["blob", "cloud", "square", "spark", "clover", "heart", "flower", "droplet", "pill", "wedge", "house", "star", "hexagon", "pumpkin"] } }, "required": ["name"], "additionalProperties": false } },
    ]);
    if let Some(list) = tools.as_array_mut() {
        list.splice(3..3, computer::tools());
        list.extend(bot_data::tools());
        list.extend(routines::tools());
        list.extend(canvas::tools());
    }
    tools
}

enum ToolCall {
    ListSkills { offset: usize },
    ReadSkill { id: String },
    CreateSkill(SkillInput),
    ListBots,
    SendMessage(BotMessageInput),
    CreateBot(BotInput),
    BotData(bot_data::Call),
    Canvas(canvas::Call),
    Computer(computer::Call),
    Routine(routines::Call),
    ListFiles { path: String },
    ReadFile { path: String },
    WriteFile { path: String, content: String, hash: Option<String> },
}

/// Strict object arguments: every key must be one of `allowed`.
fn strict<'a>(arguments: &'a Map<String, Value>, allowed: &[&str]) -> BrokerResult<&'a Map<String, Value>> {
    if arguments.keys().all(|key| allowed.contains(&key.as_str())) { Ok(arguments) } else { Err(BrokerError::Invalid) }
}

fn string(arguments: &Map<String, Value>, key: &str) -> BrokerResult<Option<String>> {
    match arguments.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(BrokerError::Invalid),
    }
}

fn required(arguments: &Map<String, Value>, key: &str) -> BrokerResult<String> {
    string(arguments, key)?.ok_or(BrokerError::Invalid)
}

fn is_uuid(value: &str) -> bool {
    value.len() == 36 && uuid::Uuid::parse_str(value).is_ok()
}

/// The `toolArguments` discriminated union of the Node broker.
fn parse_call(params: &Value) -> BrokerResult<ToolCall> {
    let name = params.get("name").and_then(Value::as_str).ok_or(BrokerError::Invalid)?;
    let arguments = params.get("arguments").and_then(Value::as_object).ok_or(BrokerError::Invalid)?;
    if let Some(call) = bot_data::parse(name, arguments) {
        return call.map(ToolCall::BotData);
    }
    if let Some(call) = canvas::parse(name, arguments) {
        return call.map(ToolCall::Canvas);
    }
    if let Some(call) = routines::parse(name, arguments) {
        return call.map(ToolCall::Routine);
    }
    if let Some(call) = computer::parse(name, arguments) {
        return call.map(ToolCall::Computer);
    }
    Ok(match name {
        "list_skills" => {
            let arguments = strict(arguments, &["offset"])?;
            let offset = match arguments.get("offset") {
                None => 0,
                Some(value) => {
                    let number = value.as_f64().ok_or(BrokerError::Invalid)?;
                    if number.fract() != 0.0 || !(0.0..=100.0).contains(&number) {
                        return Err(BrokerError::Invalid);
                    }
                    number as usize
                }
            };
            ToolCall::ListSkills { offset }
        }
        "read_skill" => {
            let id = required(strict(arguments, &["id"])?, "id")?;
            if !is_uuid(&id) {
                return Err(BrokerError::Invalid);
            }
            ToolCall::ReadSkill { id }
        }
        "create_skill" => {
            let input: SkillInput = serde_json::from_value(Value::Object(arguments.clone())).map_err(|_| BrokerError::Invalid)?;
            ToolCall::CreateSkill(validate_skill_input(input).map_err(|_| BrokerError::Invalid)?)
        }
        "list_bots" => {
            strict(arguments, &[])?;
            ToolCall::ListBots
        }
        "send_message" => {
            let input: BotMessageInput = serde_json::from_value(Value::Object(arguments.clone())).map_err(|_| BrokerError::Invalid)?;
            ToolCall::SendMessage(validate_bot_message_input(input).map_err(|_| BrokerError::Invalid)?)
        }
        "create_bot" => {
            let arguments = strict(arguments, &["name", "instructions", "color", "shape"])?;
            let input: BotInput = serde_json::from_value(Value::Object(arguments.clone())).map_err(|_| BrokerError::Invalid)?;
            if input.name.trim().is_empty() {
                return Err(BrokerError::Invalid);
            }
            ToolCall::CreateBot(input)
        }
        "list_files" => ToolCall::ListFiles { path: string(strict(arguments, &["path"])?, "path")?.unwrap_or_default() },
        "read_file" => ToolCall::ReadFile { path: required(strict(arguments, &["path"])?, "path")? },
        "write_file" => {
            let arguments = strict(arguments, &["path", "content", "hash"])?;
            let content = required(arguments, "content")?;
            if utf16_len(&content) > MAX_WRITE_CHARACTERS {
                return Err(BrokerError::Invalid);
            }
            ToolCall::WriteFile { path: required(arguments, "path")?, content, hash: string(arguments, "hash")? }
        }
        _ => return Err(BrokerError::Invalid),
    })
}

/// Run blocking workspace I/O off the async workers.
async fn files<T: Send + 'static>(root: String, f: impl FnOnce(&WorkspaceFiles) -> HttpResult<T> + Send + 'static) -> HttpResult<T> {
    tokio::task::spawn_blocking(move || f(&WorkspaceFiles::new(root))).await.unwrap_or_else(|_| Err(HttpError::new(500, "Workspace operation failed.")))
}

fn json_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn approval_options(allow: &str, deny: &str) -> (String, String, Vec<PermissionOption>) {
    let (allow_id, deny_id) = (new_id(), new_id());
    let options = vec![
        PermissionOption { id: allow_id.clone(), label: allow.into(), kind: "allow_once".into() },
        PermissionOption { id: deny_id.clone(), label: deny.into(), kind: "reject_once".into() },
    ];
    (allow_id, deny_id, options)
}

impl Runtime {
    /// `POST /internal/mcp/:runId` JSON-RPC handler. `Ok(None)` means 202 with no body.
    pub async fn mcp(&self, run_id: &str, authorization: Option<&str>, body: Value) -> HttpResult<Option<Value>> {
        let run = self
            .run_by_id(run_id)
            .filter(|run| !run.inactive() && authorization == Some(format!("Bearer {}", run.token).as_str()))
            .ok_or_else(|| HttpError::forbidden("Invalid workspace broker session."))?;
        let invalid = || HttpError::bad_request("Request: Invalid JSON-RPC request.");
        let request = body.as_object().ok_or_else(invalid)?;
        if request.get("jsonrpc") != Some(&json!("2.0")) {
            return Err(invalid());
        }
        let id = match request.get("id") {
            None => None,
            Some(id @ (Value::String(_) | Value::Number(_))) => Some(id.clone()),
            Some(_) => return Err(invalid()),
        };
        let method = request.get("method").and_then(Value::as_str).ok_or_else(invalid)?;
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else { return Ok(None) };
        let result = match method {
            "initialize" => {
                let version = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .ok_or_else(|| HttpError::bad_request("params.protocolVersion: Invalid input: expected string"))?;
                json!({"protocolVersion": version, "capabilities": {"tools": {"listChanged": false}}, "serverInfo": {"name": "grok-bot-workspace", "version": "1.0.0"}})
            }
            "ping" => json!({}),
            "tools/list" => json!({"tools": broker_tools()}),
            "tools/call" => {
                // Tool calls of one run execute in order; finishing waits for the one in progress.
                let _turn = run.broker.lock().await;
                let skill_result = matches!(params.get("name").and_then(Value::as_str), Some("list_skills" | "read_skill" | "read_canvas"))
                    || params.get("name").and_then(Value::as_str).is_some_and(|name| bot_data::NAMES.contains(&name));
                match self.broker_call(&run, &params).await {
                    Ok(output) => {
                        let text = if skill_result { skill_response_text(&output) } else { output.to_string() };
                        json!({"content": [{"type": "text", "text": text}]})
                    }
                    Err(error) => {
                        let message = match error {
                            BrokerError::Http(error) => error.message,
                            BrokerError::Invalid => "Invalid workspace tool arguments.".into(),
                        };
                        json!({"isError": true, "content": [{"type": "text", "text": run.redact(&message)}]})
                    }
                }
            }
            _ => {
                return Ok(Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}})));
            }
        };
        Ok(Some(json!({"jsonrpc": "2.0", "id": id, "result": result})))
    }

    fn is_recipient(&self, run: &Run) -> bool {
        self.store.read(|state| state.disk().bot_deliveries.iter().any(|d| d.recipient_conversation_id == run.conversation_id))
    }

    /// Set the outcome of a tool activity this broker created.
    async fn settle_tool(&self, run: &Arc<Run>, tool_id: &str, status: ToolStatus, output: Option<String>) {
        let tool_id = tool_id.to_owned();
        let _ = self
            .persist(run, move |state, run| {
                if let Some(tool) = message_mut(state, run)?.tools.iter_mut().find(|tool| tool.id == tool_id) {
                    tool.status = status;
                    if output.is_some() {
                        tool.output = output;
                    }
                }
                Ok(())
            })
            .await;
    }

    async fn broker_call(&self, run: &Arc<Run>, params: &Value) -> BrokerResult<Value> {
        if run.inactive() {
            return Err(conflict_finished().into());
        }
        match parse_call(params)? {
            ToolCall::ListSkills { offset } => {
                let all: Vec<_> =
                    if self.is_recipient(run) { Vec::new() } else { run.flags().skills.clone() }.into_iter().filter(|skill| skill.enabled).collect();
                let mut skills: Vec<Value> = Vec::new();
                for skill in all.iter().skip(offset) {
                    let next = json!({"id": skill.id, "name": run.redact_all(&skill.name), "description": run.redact_all(&skill.description)});
                    let mut candidate = skills.clone();
                    candidate.push(next.clone());
                    if skill_response_text(&json!({"skills": candidate, "nextOffset": 100})).len() > MAX_SKILL_RESPONSE_BYTES {
                        break;
                    }
                    skills.push(next);
                }
                if skills.is_empty() && offset < all.len() {
                    return Err(HttpError::too_large("Redacted skill metadata exceeds the complete-response limit. Shorten its name or description.").into());
                }
                let next_offset = offset + skills.len();
                let mut response = json!({"skills": skills});
                if next_offset < all.len() {
                    response["nextOffset"] = json!(next_offset);
                }
                Ok(response)
            }
            ToolCall::ReadSkill { id } => {
                if self.is_recipient(run) {
                    return Err(HttpError::forbidden("Recipient conversations are chat-only and cannot read private skills.").into());
                }
                let skill = run
                    .flags()
                    .skills
                    .iter()
                    .find(|skill| skill.id == id && skill.enabled)
                    .cloned()
                    .ok_or_else(|| HttpError::not_found("Enabled private skill does not exist for this bot."))?;
                let response = json!({
                    "id": skill.id, "name": run.redact(&skill.name), "description": run.redact(&skill.description),
                    "instructions": run.redact_all(&skill.instructions), "revision": skill.revision,
                });
                if skill_response_text(&response).len() > MAX_SKILL_RESPONSE_BYTES {
                    return Err(
                        HttpError::too_large("Redacted skill instructions exceed the complete-response limit. Shorten the skill before loading it.").into()
                    );
                }
                Ok(response)
            }
            ToolCall::CreateSkill(input) => self.create_skill(run, input).await,
            ToolCall::ListBots => {
                let bots = self.store.read(|state| state.disk().bots.iter().map(|bot| json!({"id": bot.id, "name": bot.name})).collect::<Vec<_>>());
                Ok(json!({"bots": bots}))
            }
            ToolCall::SendMessage(input) => self.send_bot_message(run, input).await,
            ToolCall::CreateBot(input) => self.create_bot(run, input).await,
            ToolCall::BotData(call) => self.bot_data_call(run, call).await,
            ToolCall::Canvas(call) => self.canvas_call(run, call).await,
            ToolCall::Routine(call) => self.routine_call(run, call).await,
            ToolCall::Computer(call) => self.computer_call(run, call).await,
            call => self.file_call(run, call).await,
        }
    }

    async fn file_call(&self, run: &Arc<Run>, call: ToolCall) -> BrokerResult<Value> {
        if self.is_recipient(run) {
            return Err(HttpError::forbidden("Bot delivery conversations are chat-only. File tools are unavailable.").into());
        }
        let root = run.settings.workspace.clone();
        if root.is_empty() {
            return Err(HttpError::bad_request("Choose a workspace in Local files settings before using file tools.").into());
        }
        let (filename, content, hash) = match call {
            ToolCall::ListFiles { path } => {
                return Ok(json_value(&files(root, move |files| files.list(&path)).await?));
            }
            ToolCall::ReadFile { path } => {
                return Ok(json_value(&files(root, move |files| files.read(&path)).await?));
            }
            ToolCall::WriteFile { path, content, hash } => (path, content, hash),
            _ => return Err(BrokerError::Invalid),
        };
        if content.len() > MAX_REVIEWABLE_EDIT_BYTES {
            return Err(HttpError::too_large("Write proposals must be at most 160 KiB of UTF-8 text so the complete edit can be reviewed.").into());
        }
        let expected = {
            let filename = filename.clone();
            files(root.clone(), move |files| files.expected(&filename)).await?
        };
        if run.inactive() {
            return Err(conflict_finished().into());
        }
        if hash.as_ref().is_some_and(|hash| *hash != expected) {
            return Err(HttpError::conflict("File changed on disk. Read it again before proposing an edit.").into());
        }
        let public_path = run.redact(&filename);
        let title = run.redact(&format!("Write {filename}"));
        let tool = ToolActivity {
            id: new_id(),
            title: title.clone(),
            kind: "edit".into(),
            status: ToolStatus::Pending,
            input: Some(run.redact_all(&content)),
            output: None,
            paths: Some(vec![public_path.clone()]),
            local_only: None,
            approved_by_rule: None,
        };
        let tool_id = tool.id.clone();
        let (allow, _, options) = approval_options("Allow once", "Deny");
        let verb = if expected.is_empty() { "Create" } else { "Replace" };
        let permission = PendingPermission {
            id: new_id(),
            title,
            kind: "edit".into(),
            description: run.redact_all(&format!("{verb} {filename}\n\n{content}")),
            options,
            paths: vec![public_path],
            local_only: None,
        };
        let decision = self.approve_category(run, PermissionCategory::FileWrite, permission, tool).await?;
        if run.inactive() || decision.as_deref() != Some(allow.as_str()) {
            let message = "The user denied this write or cancelled the run.";
            self.settle_tool(run, &tool_id, ToolStatus::Failed, Some(message.into())).await;
            return Err(HttpError::forbidden(message).into());
        }
        self.settle_tool(run, &tool_id, ToolStatus::InProgress, None).await;
        if run.inactive() {
            return Err(conflict_finished().into());
        }
        let saved = {
            let (filename, content, expected) = (filename.clone(), content, expected);
            files(root, move |files| files.save(&filename, &content, &expected)).await
        };
        match saved {
            Ok(document) => {
                self.settle_tool(run, &tool_id, ToolStatus::Completed, Some(run.redact(&format!("Saved {filename}.")))).await;
                Ok(json!({"path": document.path, "hash": document.hash, "saved": true}))
            }
            Err(error) => {
                self.settle_tool(run, &tool_id, ToolStatus::Failed, Some(run.redact(&error.message))).await;
                Err(error.into())
            }
        }
    }

    async fn send_bot_message(&self, run: &Arc<Run>, input: BotMessageInput) -> BrokerResult<Value> {
        if input.text.len() > MAX_BOT_MESSAGE_BYTES {
            return Err(HttpError::too_large("Bot messages must be at most 160 KiB of UTF-8 text so the complete message can be reviewed.").into());
        }
        // Same rule as `existingBotDelivery`: one distinct message per source message, idempotent.
        let existing: Option<BotDelivery> =
            self.store.read(|state| state.disk().bot_deliveries.iter().find(|d| d.source_message_id == run.message_id).cloned());
        if let Some(existing) = existing {
            if existing.to_bot_id == input.to_bot_id && existing.text == input.text {
                return Ok(bot_delivery_receipt(&existing));
            }
            return Err(HttpError::conflict("A run may send only one distinct bot message. Repeating the same request returns its existing receipt.").into());
        }
        let (recipient_id, recipient_name, parent_hop) = self.store.read(|state| -> HttpResult<_> {
            let recipient = state.bot(&input.to_bot_id)?;
            // A report turn's message continues the reported delivery's chain (see `chain_parent`).
            let parent = chain_parent(&state.disk().bot_deliveries, state.conversation(&run.conversation_id)?, &run.message_id);
            if let Some(parent) = parent {
                if state.disk().bot_deliveries.iter().filter(|delivery| delivery.chain_id == parent.chain_id).count() >= MAX_BOT_DELIVERIES_PER_CHAIN {
                    return Err(HttpError::conflict(BOT_CHAIN_CAPACITY_ERROR));
                }
                if state.expired_bot_chains(state.now()).contains(&parent.chain_id) {
                    return Err(HttpError::conflict(BOT_CHAIN_TIMEOUT_ERROR));
                }
                if state.disk().bot_deliveries.iter().any(|delivery| delivery.id == parent.chain_id && delivery.chain_termination.is_some()) {
                    return Err(HttpError::conflict("This bot message chain was stopped."));
                }
            }
            Ok((recipient.id.clone(), recipient.name.clone(), parent.map(|d| d.hop).unwrap_or(0)))
        })?;
        if recipient_id == run.bot.id {
            return Err(HttpError::bad_request("Choose another bot as the recipient.").into());
        }
        if parent_hop >= MAX_BOT_HOPS {
            return Err(HttpError::conflict("Bot message chains are limited to four hops.").into());
        }
        let tool = ToolActivity {
            id: new_id(),
            title: run.redact(&format!("Message {recipient_name}")),
            kind: "other".into(),
            status: ToolStatus::Pending,
            input: Some(run.redact_all(&input.text)),
            output: None,
            paths: None,
            local_only: None,
            approved_by_rule: None,
        };
        let tool_id = tool.id.clone();
        let (allow, _, options) = approval_options("Send once", "Deny");
        let permission = PendingPermission {
            id: new_id(),
            title: tool.title.clone(),
            kind: "other".into(),
            description: run.redact_all(&format!("Send a chat-only message from {} to {recipient_name} ({recipient_id}).\n\n{}", run.bot.name, input.text)),
            options,
            paths: Vec::new(),
            local_only: None,
        };
        let decision = self.approve_category(run, PermissionCategory::BotMessage, permission, tool).await?;
        if run.inactive() || decision.as_deref() != Some(allow.as_str()) {
            let message = "The user denied this message or cancelled the run.";
            self.settle_tool(run, &tool_id, ToolStatus::Failed, Some(message.into())).await;
            return Err(HttpError::forbidden(message).into());
        }
        let queued = {
            let (store, run) = (Arc::clone(&self.store), Arc::clone(run));
            self.queue
                .run(async move {
                    store
                        .mutate(move |state| {
                            if run.inactive() {
                                return Err(conflict_finished());
                            }
                            state.queue_bot_message(&run.conversation_id, &run.run_id, &run.message_id, input)
                        })
                        .await
                })
                .await
        };
        match queued {
            Ok(delivery) => {
                let output = run.redact(&format!("Queued message {} for {recipient_name}.", delivery.id));
                self.settle_tool(run, &tool_id, ToolStatus::Completed, Some(output)).await;
                Ok(bot_delivery_receipt(&delivery))
            }
            Err(error) => {
                self.settle_tool(run, &tool_id, ToolStatus::Failed, Some(run.redact(&error.message))).await;
                Err(error.into())
            }
        }
    }

    async fn create_skill(&self, run: &Arc<Run>, input: SkillInput) -> BrokerResult<Value> {
        if self.is_recipient(run) {
            return Err(HttpError::forbidden("Recipient conversations are chat-only and cannot save skills.").into());
        }
        let request_key = hex::encode(Sha256::digest(json_value(&input).to_string().as_bytes()));
        {
            let flags = run.flags();
            if let Some(prior) = flags.created_skills.get(&request_key) {
                return Ok(json!({"id": prior, "name": run.redact(&input.name), "saved": true, "existing": true}));
            }
            if !flags.created_skills.is_empty() {
                return Err(HttpError::conflict("Only one private skill can be created per model turn. Ask the user before proposing another.").into());
            }
        }
        let title = run.redact(&format!("Save private skill: {}", input.name));
        let description = run.redact_all(&format!(
            "Save this reusable skill for {}. It will be available to this bot's future chats.\n\nName: {}\nWhen to use: {}\n\n{}",
            run.bot.name, input.name, input.description, input.instructions
        ));
        let tool = ToolActivity {
            id: new_id(),
            title: title.clone(),
            kind: "edit".into(),
            status: ToolStatus::Pending,
            input: Some(description.clone()),
            output: None,
            paths: None,
            local_only: None,
            approved_by_rule: None,
        };
        let tool_id = tool.id.clone();
        let (allow, _, options) = approval_options("Save skill once", "Deny");
        let permission = PendingPermission { id: new_id(), title, kind: "edit".into(), description, options, paths: Vec::new(), local_only: None };
        let choice = self.approve_category(run, PermissionCategory::SkillSave, permission, tool).await?;
        if choice.as_deref() != Some(allow.as_str()) || run.inactive() {
            let message = "Skill save was denied or cancelled.";
            self.settle_tool(run, &tool_id, ToolStatus::Failed, Some(message.into())).await;
            return Err(HttpError::forbidden(message).into());
        }
        let saved = {
            let (store, run) = (Arc::clone(&self.store), Arc::clone(run));
            self.queue
                .run(async move {
                    store
                        .mutate(move |state| {
                            if run.inactive() {
                                return Err(HttpError::conflict("The run ended before the skill could be saved."));
                            }
                            state.add_skill(&run.bot.id, input, true)
                        })
                        .await
                })
                .await
        };
        match saved {
            Ok(skill) => {
                {
                    let mut flags = run.flags();
                    flags.created_skills.insert(request_key, skill.id.clone());
                    flags.skills.push(skill.clone());
                }
                self.settle_tool(run, &tool_id, ToolStatus::Completed, Some(run.redact(&format!("Saved private skill {}.", skill.name)))).await;
                Ok(json!({"id": skill.id, "name": run.redact(&skill.name), "saved": true}))
            }
            Err(error) => {
                self.settle_tool(run, &tool_id, ToolStatus::Failed, Some(run.redact(&error.message))).await;
                Err(error.into())
            }
        }
    }

    async fn create_bot(&self, run: &Arc<Run>, input: BotInput) -> BrokerResult<Value> {
        if self.is_recipient(run) {
            return Err(HttpError::forbidden("Recipient conversations are chat-only and cannot create bots.").into());
        }
        let request_key = hex::encode(Sha256::digest(json_value(&input).to_string().as_bytes()));
        {
            let flags = run.flags();
            if let Some(prior) = flags.created_bots.get(&request_key) {
                return Ok(json!({"id": prior, "name": run.redact(&input.name), "created": true, "existing": true}));
            }
            if !flags.created_bots.is_empty() {
                return Err(HttpError::conflict("Only one bot can be created per model turn. Ask the user before proposing another.").into());
            }
        }
        let shape = input.shape.map(|shape| json_value(&shape).as_str().unwrap_or_default().to_owned());
        let title = run.redact(&format!("Create bot: {}", input.name.trim()));
        let description = run.redact_all(&format!(
            "Create a new local bot requested in {}'s conversation.\n\nName: {}\nColor: {}\nShape: {}\n\nInstructions:\n{}",
            run.bot.name,
            input.name.trim(),
            input.color.as_deref().unwrap_or("default"),
            shape.as_deref().unwrap_or("default"),
            input.instructions.as_deref().filter(|text| !text.is_empty()).unwrap_or("(none)"),
        ));
        let tool = ToolActivity {
            id: new_id(),
            title: title.clone(),
            kind: "edit".into(),
            status: ToolStatus::Pending,
            input: Some(description.clone()),
            output: None,
            paths: None,
            local_only: None,
            approved_by_rule: None,
        };
        let tool_id = tool.id.clone();
        let (allow, _, options) = approval_options("Create bot once", "Deny");
        let permission = PendingPermission { id: new_id(), title, kind: "edit".into(), description, options, paths: Vec::new(), local_only: None };
        let choice = self.approve_category(run, PermissionCategory::BotCreate, permission, tool).await?;
        if choice.as_deref() != Some(allow.as_str()) || run.inactive() {
            let message = "Bot creation was denied or cancelled.";
            self.settle_tool(run, &tool_id, ToolStatus::Failed, Some(message.into())).await;
            return Err(HttpError::forbidden(message).into());
        }
        let created = {
            let (store, run) = (Arc::clone(&self.store), Arc::clone(run));
            self.queue
                .run(async move {
                    store
                        .mutate(move |state| {
                            if run.inactive() {
                                return Err(HttpError::conflict("The run ended before the bot could be created."));
                            }
                            state.add_bot_from_run(input)
                        })
                        .await
                })
                .await
        };
        match created {
            Ok(bot) => {
                run.flags().created_bots.insert(request_key, bot.id.clone());
                self.settle_tool(run, &tool_id, ToolStatus::Completed, Some(run.redact(&format!("Created bot {}.", bot.name)))).await;
                Ok(json!({"id": bot.id, "name": run.redact(&bot.name), "created": true}))
            }
            Err(error) => {
                self.settle_tool(run, &tool_id, ToolStatus::Failed, Some(run.redact(&error.message))).await;
                Err(error.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invalid(params: Value) -> bool {
        matches!(parse_call(&params), Err(BrokerError::Invalid))
    }

    #[test]
    fn managed_computer_tools_are_listed_after_skills_and_strict() {
        let names: Vec<String> = broker_tools().as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect();
        assert_eq!(
            names,
            [
                "list_skills",
                "read_skill",
                "create_skill",
                "computer_observe",
                "computer_act",
                "list_files",
                "read_file",
                "write_file",
                "list_bots",
                "send_message",
                "create_bot",
                "list_memories",
                "save_memory",
                "forget_memory",
                "list_knowledge",
                "read_knowledge",
                "list_routines",
                "save_routine",
                "set_routine_enabled",
                "delete_routine",
                "read_canvas",
                "replace_canvas_range"
            ]
        );
        assert!(matches!(parse_call(&json!({"name": "computer_observe", "arguments": {}})), Ok(ToolCall::Computer(computer::Call::Observe))));
        assert!(invalid(json!({"name": "computer_observe", "arguments": {"x": 1}})));
        assert!(invalid(json!({"name": "computer_act", "arguments": {"epoch": "x", "frameId": null, "action": {"type": "observe"}}})));
        let epoch = new_id();
        assert!(matches!(
            parse_call(&json!({"name": "computer_act", "arguments": {"epoch": epoch, "frameId": null, "action": {"type": "click", "x": 10, "y": 20.5}}})),
            Ok(ToolCall::Computer(computer::Call::Act { action, frame_id: None, .. })) if action == json!({"type": "click", "x": 10, "y": 20.5})
        ));
        for action in [
            json!({"type": "click", "x": 960, "y": 1}),
            json!({"type": "key", "key": "Control+L"}),
            json!({"type": "scroll", "deltaY": 1.5}),
            json!({"type": "type", "text": ""}),
            json!({"type": "observe", "extra": true}),
            json!({"type": "evaluate", "script": "1"}),
        ] {
            assert!(invalid(json!({"name": "computer_act", "arguments": {"epoch": epoch, "frameId": null, "action": action}})), "{action}");
        }
        assert!(invalid(json!({"name": "computer_act", "arguments": {"epoch": epoch, "action": {"type": "observe"}}})));
    }

    #[test]
    fn tool_arguments_follow_the_strict_schemas() {
        assert!(matches!(parse_call(&json!({"name": "list_files", "arguments": {}})), Ok(ToolCall::ListFiles { path }) if path.is_empty()));
        assert!(invalid(json!({"name": "list_files"})));
        assert!(invalid(json!({"name": "list_files", "arguments": {"path": "a", "extra": 1}})));
        assert!(invalid(json!({"name": "read_file", "arguments": {}})));
        assert!(invalid(json!({"name": "read_skill", "arguments": {"id": "not-a-uuid"}})));
        assert!(matches!(parse_call(&json!({"name": "list_skills", "arguments": {"offset": 9.0}})), Ok(ToolCall::ListSkills { offset: 9 })));
        assert!(invalid(json!({"name": "list_skills", "arguments": {"offset": 101}})));
        assert!(invalid(json!({"name": "list_skills", "arguments": {"offset": 1.5}})));
        assert!(invalid(json!({"name": "list_bots", "arguments": {"x": 1}})));
        assert!(invalid(json!({"name": "write_file", "arguments": {"path": "a", "content": 1}})));
        assert!(matches!(
            parse_call(&json!({"name": "write_file", "arguments": {"path": "a", "content": "b", "hash": ""}})),
            Ok(ToolCall::WriteFile { hash: Some(hash), .. }) if hash.is_empty()
        ));
    }
}
