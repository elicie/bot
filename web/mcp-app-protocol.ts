// The opt-in MCP app bridge. This parser grants no authority: the server binds each one-use
// request to the stored app, and only a separate host approval can execute it.
export const MCP_APP_PROTOCOL_VERSION = '2026-01-26';
export const MAX_APP_REQUEST_BYTES = 32 * 1024;
export type RpcId = string | number;
export interface AppRequest { jsonrpc: '2.0'; method: string; id?: RpcId; params: Record<string, unknown> }
export function record(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === 'object' && !Array.isArray(value);
}
export function readAppRequest(value: unknown): AppRequest | null {
  if (!record(value) || value.jsonrpc !== '2.0' || typeof value.method !== 'string' || value.method.length > 100) return null;
  if (value.id !== undefined && !(typeof value.id === 'string' && value.id.length > 0 && value.id.length <= 128)
    && !(typeof value.id === 'number' && Number.isSafeInteger(value.id))) return null;
  if (value.params !== undefined && !record(value.params)) return null;
  try { if (new TextEncoder().encode(JSON.stringify(value)).length > MAX_APP_REQUEST_BYTES) return null; } catch { return null; }
  return { jsonrpc: '2.0', method: value.method, ...(value.id !== undefined ? { id: value.id as RpcId } : {}), params: value.params ?? {} };
}
export const rpcResult = (id: RpcId, result: unknown) => ({ jsonrpc: '2.0', id, result });
export const rpcError = (id: RpcId, code: number, message: string) => ({ jsonrpc: '2.0', id, error: { code, message } });
