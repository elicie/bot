//! Bot vault secrets ("보관함" / "시크릿") and custom MCP servers ("사용자 지정 MCP"). Mirrors
//! `shared/vault.ts`. Both live in new optional `state.json` fields, so older state files load
//! unchanged.
//!
//! Secret values are write-only from the browser: [`VaultSecret::value`] is persisted in
//! `state.json` (mode 0600, like the provider API key) and never appears in a public snapshot,
//! which carries [`PublicSecret`] instead.

use serde::{Deserialize, Serialize};

/// A named credential. `bot_id` absent: available to every bot (a bot's own secret of the same
/// name wins).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VaultSecret {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_id: Option<String>,
    pub value: String,
    pub created_at: String,
    pub updated_at: String,
}

/// What every client sees of a secret: its name and scope, never the value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicSecret {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<&VaultSecret> for PublicSecret {
    fn from(secret: &VaultSecret) -> Self {
        Self {
            id: secret.id.clone(),
            name: secret.name.clone(),
            bot_id: secret.bot_id.clone(),
            created_at: secret.created_at.clone(),
            updated_at: secret.updated_at.clone(),
        }
    }
}

/// `POST /api/secrets` body ("새 시크릿 추가").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretInput {
    pub name: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_id: Option<String>,
}

/// `PUT /api/secrets/:id` body (replace the value).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretValueInput {
    pub value: String,
}

/// One `NAME: value` pair of a stdio server's environment or an HTTP server's headers. The value
/// may reference vault secrets as `${secret:NAME}`; references are resolved only when a session
/// starts and the resolved value never leaves the server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpPair {
    pub name: String,
    pub value: String,
}

/// Transport of a custom MCP server, in the shape of the common `mcpServers` JSON entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "lowercase", deny_unknown_fields)]
pub enum McpServerConfig {
    /// A local command the engine starts as the server user (arbitrary command execution).
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env: Vec<McpPair>,
    },
    /// A streamable HTTP MCP endpoint.
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        headers: Vec<McpPair>,
    },
}

/// A bot's custom MCP server. New and changed servers start disabled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpServerRecord {
    pub id: String,
    pub bot_id: String,
    pub name: String,
    pub enabled: bool,
    pub config: McpServerConfig,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransportKind {
    Stdio,
    Http,
}

/// What every client sees of a custom server: no command, URL, environment, or headers (those can
/// carry literal credentials and are read through the loopback-only config route).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicMcpServer {
    pub id: String,
    pub bot_id: String,
    pub name: String,
    pub transport: McpTransportKind,
    pub enabled: bool,
    /// Names referenced as `${secret:NAME}`, in first-use order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret_refs: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// `PUT /api/bots/:botId/mcp-servers` body: the "MCP 서버 JSON" document
/// (`{"mcpServers": {"name": {...}}}`) that replaces the bot's server set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpConfigInput {
    pub json: String,
    /// Optional new bot-scoped credentials, committed atomically with the server document.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretInput>,
}

/// `PATCH /api/bots/:botId/mcp-servers/:name` body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpEnabledInput {
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_configs_are_tagged_and_strict() {
        let stdio: McpServerConfig = serde_json::from_str(r#"{"transport":"stdio","command":"node","args":["s.js"]}"#).unwrap();
        assert_eq!(stdio, McpServerConfig::Stdio { command: "node".into(), args: vec!["s.js".into()], env: vec![] });
        assert_eq!(serde_json::to_string(&stdio).unwrap(), r#"{"transport":"stdio","command":"node","args":["s.js"]}"#);
        assert!(serde_json::from_str::<McpServerConfig>(r#"{"transport":"http","url":"http://x","extra":1}"#).is_err());
        assert!(serde_json::from_str::<McpServerConfig>(r#"{"transport":"sse","url":"http://x"}"#).is_err());
    }
}
