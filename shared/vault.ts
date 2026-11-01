// Vault secrets ("보관함" / "시크릿") and custom MCP servers ("사용자 지정 MCP").
// Mirrors `crates/model/src/vault.rs`. Secret values are write-only: no response carries them.

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
