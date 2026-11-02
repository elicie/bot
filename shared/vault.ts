// Vault secrets ("보관함" / "시크릿") and custom MCP servers ("사용자 지정 MCP").
// Public storage types mirror `crates/model/src/vault.rs`; the probe body is in core/vault_http.rs.
// Secret values are write-only: no response carries them.

/** A secret's name and scope. `botId` absent: available to every bot. */
export interface PublicSecret {
  id: string;
  name: string;
  botId?: string;
  createdAt: string;
  updatedAt: string;
}
export interface SecretInput {
  name: string;
  value: string;
  botId?: string;
}
export type McpTransportKind = 'stdio' | 'http';
/** A custom MCP server summary (no command, URL, environment, or headers). */
export interface PublicMcpServer {
  id: string;
  botId: string;
  name: string;
  transport: McpTransportKind;
  enabled: boolean;
  /** Names referenced as `${secret:NAME}`. */
  secretRefs?: string[];
  createdAt: string;
  updatedAt: string;
}
/** `GET /api/vault`. */
export interface VaultAccess {
  canEdit: boolean;
}
/** `GET /api/bots/:botId/mcp-servers` (loopback only). */
export interface McpConfigDocument {
  json: string;
}
/** `PUT /api/bots/:botId/mcp-servers`: optional new secrets commit with the document. */
export interface McpConfigInput extends McpConfigDocument {
  secrets?: SecretInput[];
}
/** `POST /api/bots/:botId/mcp-servers/test` (loopback only, never persists). */
export interface McpConnectionTestInput {
  /** Exactly one server in the same JSON format used by configuration storage. */
  json: string;
  /** In-memory bot-scoped credentials for testing before a server is saved. */
  secrets?: SecretInput[];
}
export interface McpConnectionTestResult {
  toolCount: number;
}
