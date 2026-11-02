import test from 'node:test';
import assert from 'node:assert/strict';
import { appendMcpDraft, availableMcpSecrets, buildMcpDraft, emptyMcpDraft, mcpDraftProblems, type McpCredential } from '../web/mcp-form';
import type { PublicMcpServer, PublicSecret } from '../shared/vault';

const secret = (name: string, botId?: string): PublicSecret => ({ id: `${name}-${botId ?? 'all'}`, name, botId, createdAt: '', updatedAt: '' });
const pair = (fields: Partial<McpCredential> = {}): McpCredential => ({ id: '1', name: 'Authorization', source: 'vault', secret: 'TOKEN', value: '', prefix: 'Bearer ', ...fields });
const remote = () => ({ ...emptyMcpDraft(), name: 'remote', transport: 'http' as const, url: 'https://example.com/mcp' });
const server = (name: string): PublicMcpServer => ({ id: name, botId: 'bot', name, transport: 'http', enabled: true, createdAt: '', updatedAt: '' });

test('scoped vault choices mirror runtime precedence and exclude other bots', () => {
  const all = secret('TOKEN');
  const own = secret('TOKEN', 'bot');
  assert.deepEqual(availableMcpSecrets([own, all, secret('OTHER', 'elsewhere'), secret('GLOBAL')], 'bot'), [secret('GLOBAL'), own]);
});

test('HTTP draft references the vault and keeps direct values out of JSON', () => {
  const draft = { ...remote(), headers: [pair(), pair({ id: '2', name: 'X-Key', source: 'new', secret: 'NEW_TOKEN', value: '  write-only-value  ', prefix: '' })] };
  assert.deepEqual(mcpDraftProblems(draft, [], [secret('TOKEN')]), {});
  const input = buildMcpDraft(draft, 'bot');
  assert.equal(JSON.parse(input.json).mcpServers.remote.headers.Authorization, 'Bearer ${secret:TOKEN}');
  assert.equal(JSON.parse(input.json).mcpServers.remote.headers['X-Key'], '${secret:NEW_TOKEN}');
  assert.deepEqual(input.secrets, [{ name: 'NEW_TOKEN', value: 'write-only-value', botId: 'bot' }]);
  assert.ok(!input.json.includes('write-only-value'));
});

test('stdio arguments are literal argv entries, including spaces, quotes and empty strings', () => {
  const draft = { ...emptyMcpDraft(), name: 'local', command: ' node ', args: ['a b', '"quoted"', ''], env: [pair({ name: 'API_TOKEN', prefix: 'ignored' })] };
  assert.deepEqual(mcpDraftProblems(draft, [], [secret('TOKEN')]), {});
  assert.deepEqual(JSON.parse(buildMcpDraft(draft, 'bot').json), { mcpServers: { local: { command: 'node', args: ['a b', '"quoted"', ''], env: { API_TOKEN: '${secret:TOKEN}' } } } });
});

test('name, URL, command, argument and per-bot limits give field errors', () => {
  for (const name of ['', 'Upper', '_first', 'a__b', 'workspace', 'workspace-other', 'a'.repeat(33)]) assert.ok(mcpDraftProblems({ ...remote(), name }, [], []).name, name);
  assert.ok(mcpDraftProblems(remote(), [server('remote')], []).name);
  assert.ok(mcpDraftProblems(remote(), Array.from({ length: 10 }, (_, i) => server(`server${i}`)), []).name);
  for (const url of ['ftp://example.com', 'https://user:pass@example.com', 'https://example.com/#', 'https://example.com/#x', 'not a url', 'https://example.com/${secret:TOKEN}']) assert.ok(mcpDraftProblems({ ...remote(), url }, [], []).url, url);
  for (const command of ['', './server', 'path/server', 'no\nde', '${secret:TOKEN}']) assert.ok(mcpDraftProblems({ ...emptyMcpDraft(), name: 'local', command }, [], []).command, command);
  assert.ok(mcpDraftProblems({ ...emptyMcpDraft(), name: 'local', command: 'node', args: ['${secret:TOKEN}'] }, [], []).args);
  assert.ok(mcpDraftProblems({ ...emptyMcpDraft(), name: 'local', command: 'node', args: Array(65).fill('') }, [], []).args);
});

test('credential validation rejects missing, duplicate, invalid, short and control values', () => {
  const check = (headers: McpCredential[], secrets = [secret('TOKEN')]) => mcpDraftProblems({ ...remote(), headers }, [], secrets);
  assert.ok(check([pair({ secret: 'MISSING' })])['pair-1']);
  assert.ok(check([pair(), pair({ id: '2', name: 'authorization' })])['pair-2']);
  assert.ok(check([pair({ name: 'bad header' })])['pair-1']);
  assert.ok(check([pair({ source: 'new', secret: 'TOKEN', value: 'long-value' })])['pair-1']);
  const short = check([pair({ source: 'new', secret: 'NEW', value: 'short' })]);
  assert.ok(short['pair-1']);
  assert.ok(short['pair-1-value'], 'the password field, not the valid header name, receives the error');
  assert.equal(short['pair-1-name'], undefined);
  assert.ok(check([pair({ source: 'new', secret: 'NEW', value: 'bad\nvalue' })])['pair-1']);
  assert.ok(check([pair({ source: 'new', secret: 'lower', value: 'long-value' })])['pair-1']);
  assert.ok(check([pair({ source: 'new', secret: 'NEW', value: 'long-value' }), pair({ id: '2', name: 'X-Other', source: 'new', secret: 'NEW', value: 'other-value' })])['pair-2']);
  assert.ok(check([pair({ prefix: 'Bearer\r\n' })])['pair-1']);
  assert.ok(check([pair({ prefix: '${secret:OTHER}' })])['pair-1']);
  const env = { ...emptyMcpDraft(), name: 'local', command: 'node', env: [pair({ name: 'TOKEN' }), pair({ id: '2', name: 'token' })] };
  assert.deepEqual(mcpDraftProblems(env, [], [secret('TOKEN')]), {}, 'environment variable names are case sensitive');
});

test('append uses the fresh JSON document and refuses collisions, capacity or oversize', () => {
  const input = buildMcpDraft(remote(), 'bot');
  const existing = { mcpServers: { existing: { url: 'https://other.example/mcp', headers: { Key: '${secret:TOKEN}' } } } };
  assert.deepEqual(JSON.parse(appendMcpDraft(JSON.stringify(existing), input)), { mcpServers: { ...existing.mcpServers, remote: input.config } });
  assert.throws(() => appendMcpDraft(input.json, input), /같은 이름/);
  assert.throws(() => appendMcpDraft(JSON.stringify({ mcpServers: Object.fromEntries(Array.from({ length: 10 }, (_, i) => [`s${i}`, {}])) }), input), /10개/);
  assert.throws(() => appendMcpDraft(JSON.stringify({ mcpServers: { other: { command: 'node', args: ['x'.repeat(65536)] } } }), input), /64 KiB/);
  assert.throws(() => appendMcpDraft('{}', input), /JSON/);
});
