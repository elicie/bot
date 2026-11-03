import assert from 'node:assert/strict';
import { test } from 'node:test';
import { MAX_APP_REQUEST_BYTES, readAppRequest, rpcError, rpcResult } from '../web/mcp-app-protocol';

test('MCP app request parser accepts bounded JSON-RPC objects and typed IDs only', () => {
  const call = { jsonrpc: '2.0', id: 1, method: 'tools/call', params: { name: 'lookup', arguments: { query: 'demo' } } };
  assert.deepEqual(readAppRequest(call), call);
  assert.deepEqual(readAppRequest({ jsonrpc: '2.0', method: 'ui/notifications/initialized' }), { jsonrpc: '2.0', method: 'ui/notifications/initialized', params: {} });
  for (const data of [null, [], 'tools/call', { ...call, jsonrpc: '1.0' }, { ...call, id: null }, { ...call, id: {} }, { ...call, id: Infinity }, { ...call, id: 1.5 }, { ...call, id: 'x'.repeat(129) }, { ...call, params: [] }, { ...call, params: { data: '한'.repeat(MAX_APP_REQUEST_BYTES / 3) } }]) assert.equal(readAppRequest(data), null);
  const cycle: Record<string, unknown> = { ...call }; cycle.params = cycle;
  assert.equal(readAppRequest(cycle), null);
});

test('bridge results and errors preserve request correlation without interpreting payload', () => {
  assert.deepEqual(rpcResult('a', { content: [] }), { jsonrpc: '2.0', id: 'a', result: { content: [] } });
  assert.deepEqual(rpcError(7, -32601, 'unsupported'), { jsonrpc: '2.0', id: 7, error: { code: -32601, message: 'unsupported' } });
});
