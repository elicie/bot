//! Conversation-attached MCP app resources. The HTML and initial tool data are durable display
//! artifacts; app-initiated requests have their own transient lifecycle, not `Conversation.run`.

use serde::{Deserialize, Serialize};

/// Maximum UTF-8 size of one persisted app resource.
pub const MAX_MCP_APP_HTML_BYTES: usize = 200_000;
/// Maximum UTF-8 size of the JSON object passed to the initial tool.
pub const MAX_MCP_APP_INPUT_BYTES: usize = 32 * 1024;
/// Maximum UTF-8 size of the initial tool's text result.
pub const MAX_MCP_APP_OUTPUT_BYTES: usize = 64 * 1024;
pub const MAX_MCP_APP_URI_CHARS: usize = 2048;
pub const MAX_MCP_APP_TOOL_CHARS: usize = 128;
pub const MAX_MCP_APPS_PER_CONVERSATION: usize = 16;

/// A custom MCP tool's app resource, captured with its owning actor and server version. Historical
/// resources remain renderable after a server is disabled, edited, or removed; those bindings are
/// rechecked by the app-call service before any new tool execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpApp {
    pub bot_id: String,
    pub server_id: String,
    pub server_updated_at: String,
    pub server: String,
    pub tool: String,
    pub uri: String,
    pub html: String,
    /// Complete JSON object of arguments, encoded as a string.
    pub input: String,
    /// Initial tool result, limited to the supported text content subset.
    pub output: String,
}
