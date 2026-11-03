//! Validation of persisted conversation-attached MCP apps. Server bindings describe the captured
//! resource, not a live permission: edits, disabling, and deletion must not invalidate old history.

use grok_bot_model::{
    Conversation, MAX_MCP_APP_HTML_BYTES, MAX_MCP_APP_INPUT_BYTES, MAX_MCP_APP_OUTPUT_BYTES, MAX_MCP_APP_TOOL_CHARS, MAX_MCP_APP_URI_CHARS,
    MAX_MCP_APPS_PER_CONVERSATION, McpApp,
};

use crate::validate::{is_timestamp, is_uuid, js_len};
use crate::vault::server_name_valid;

/// Formats and bounds for one durable app resource. Credential redaction is performed before
/// capture by the runtime, and again when the store constructs public snapshots.
pub fn persisted_mcp_app_valid(app: &McpApp) -> bool {
    is_uuid(&app.bot_id)
        && is_uuid(&app.server_id)
        && is_timestamp(&app.server_updated_at)
        && server_name_valid(&app.server)
        && (1..=MAX_MCP_APP_TOOL_CHARS).contains(&js_len(&app.tool))
        && !app.tool.chars().any(char::is_control)
        && app.uri.starts_with("ui://")
        && app.uri.len() > "ui://".len()
        && js_len(&app.uri) <= MAX_MCP_APP_URI_CHARS
        && !app.uri.chars().any(char::is_control)
        && !app.html.trim().is_empty()
        && app.html.len() <= MAX_MCP_APP_HTML_BYTES
        && app.input.len() <= MAX_MCP_APP_INPUT_BYTES
        && serde_json::from_str::<serde_json::Value>(&app.input).is_ok_and(|input| input.is_object())
        && app.output.len() <= MAX_MCP_APP_OUTPUT_BYTES
}

/// App resources belong to the conversation's bot, or to the speaking member of a group message.
/// No live server/configuration reference is required to retain the captured HTML in history.
pub fn conversation_mcp_apps_valid(conversation: &Conversation) -> bool {
    let mut count = 0;
    conversation.messages.iter().all(|message| {
        let actor = if conversation.group_id.is_some() { message.speaker_bot_id.as_deref() } else { Some(conversation.bot_id.as_str()) };
        message.tools.iter().all(|tool| match &tool.mcp_app {
            None => true,
            Some(app) => {
                count += 1;
                count <= MAX_MCP_APPS_PER_CONVERSATION && actor == Some(app.bot_id.as_str()) && persisted_mcp_app_valid(app)
            }
        })
    })
}
