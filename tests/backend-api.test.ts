import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtemp, mkdir, writeFile, readFile, symlink, stat, rm, link } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import type { AddressInfo } from 'node:net';
import { createServer, request as httpRequest } from 'node:http';
import { createBackend } from '../server/app.js';
import type { AppState, FileDocument } from '../shared/types.js';

async function fixture(options: { allowedHosts?: string[] } = {}) {
  const root = await mkdtemp(path.join(tmpdir(), 'grok-bot-backend-api-'));
  const workspace = path.join(root, 'workspace');
  const dataDir = path.join(root, 'data');
  await mkdir(workspace);
  await writeFile(path.join(workspace, 'notes.txt'), 'original\n');
  const backend = await createBackend({ dataDir, serveWeb: false, allowedHosts: options.allowedHosts });
  await new Promise<void>(resolve => backend.server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${(backend.server.address() as AddressInfo).port}`;
  async function api(route: string, body?: unknown, method = body === undefined ? 'GET' : 'POST', headers: Record<string, string> = {}) {
    return fetch(`${origin}${route}`, {
      method, headers: { Origin: origin, 'X-Grok-Bot': '1', 'Content-Type': 'application/json', ...headers },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  }
  return { root, dataDir, workspace, backend, origin, api, close: async () => { await backend.close(); await rm(root, { recursive: true, force: true }); } };
}

const settings = (workspace: string) => ({ baseUrl: 'http://127.0.0.1:12345/v1', model: 'test-model', workspace });

test('settings persist privately and public snapshots, SSE, and validation errors never disclose the stored key', async () => {
  const f = await fixture();
  try {
    const initial = await (await f.api('/api/state')).json() as AppState;
    assert.equal(initial.bots.length, 1);
    assert.equal(initial.bots[0].name, 'Grok');
    const key = 'fixture-secret-not-for-the-browser';
    const saved = await f.api('/api/settings', { ...settings(f.workspace), apiKey: key }, 'PUT');
    assert.equal(saved.status, 200);
    const body = await saved.text();
    assert.equal(body.includes(key), false);
    assert.equal(JSON.parse(body).settings.hasApiKey, true);
    const disk = JSON.parse(await readFile(path.join(f.dataDir, 'state.json'), 'utf8'));
    assert.equal(disk.settings.apiKey, key);
    assert.equal((await stat(path.join(f.dataDir, 'state.json'))).mode & 0o777, 0o600);
    assert.equal((await stat(f.dataDir)).mode & 0o777, 0o700);
    const events = await f.api('/api/events');
    const reader = events.body!.getReader();
    const first = new TextDecoder().decode((await reader.read()).value);
    assert.match(first, /event: state/);
    assert.equal(first.includes(key), false);
    await reader.cancel();
    assert.equal((await f.api('/api/settings', { ...settings(f.workspace), baseUrl: 'https://user:password@example.invalid/v1' }, 'PUT')).status, 400);
    assert.equal((await f.api('/api/settings', { ...settings(f.workspace), model: ' ' }, 'PUT')).status, 400);
    await f.backend.close();
    const restarted = await createBackend({ dataDir: f.dataDir, serveWeb: false });
    assert.equal(restarted.store.snapshot().settings.hasApiKey, true);
    assert.equal(restarted.store.snapshot().bots[0].id, initial.bots[0].id);
    await restarted.close();
  } finally { await f.close(); }
});

test('local APIs reject hostile Host, cross origin, missing mutation header, invalid JSON, and oversized bodies', async () => {
  const f = await fixture();
  try {
    const hostileStatus = await new Promise<number>(resolve => {
      const req = httpRequest(f.origin + '/api/state', { headers: { Host: 'attacker.invalid' } }, response => { response.resume(); resolve(response.statusCode!); });
      req.end();
    });
    assert.equal(hostileStatus, 403);
    assert.equal((await f.api('/api/state', undefined, 'GET', { Origin: 'http://evil.invalid' })).status, 403);
    assert.equal((await f.api('/api/bots', { name: 'Unsafe' }, 'POST', { 'X-Grok-Bot': '' })).status, 403);
    assert.equal((await fetch(f.origin + '/api/bots', { method: 'POST', headers: { 'X-Grok-Bot': '1', 'Content-Type': 'application/json' }, body: '{"name":"Unsafe"}' })).status, 403);
    const headers = { Origin: f.origin, 'X-Grok-Bot': '1', 'Content-Type': 'application/json' };
    assert.equal((await fetch(f.origin + '/api/bots', { method: 'POST', headers, body: '{' })).status, 400);
    assert.equal((await fetch(f.origin + '/api/bots', { method: 'POST', headers, body: JSON.stringify({ name: 'x'.repeat(2 * 1024 * 1024) }) })).status, 413);
  } finally { await f.close(); }
});

test('explicit network hosts accept same-origin commands while hostile hosts and origins remain blocked', async () => {
  const f = await fixture({ allowedHosts: ['192.168.10.154', 'devbox', 'Devbox.tailc6b81b.ts.net.'] });
  const port = new URL(f.origin).port;
  const host = `192.168.10.154:${port}`;
  const origin = `http://${host}`;
  const request = (method: string, route: string, requestHost: string, requestOrigin?: string) => new Promise<number>((resolve, reject) => {
    const req = httpRequest(f.origin + route, {
      method,
      headers: { Host: requestHost, ...(requestOrigin ? { Origin: requestOrigin } : {}), 'Content-Type': 'application/json', 'X-Grok-Bot': '1' },
    }, response => { response.resume(); response.on('end', () => resolve(response.statusCode!)); });
    req.on('error', reject);
    req.end(method === 'POST' ? JSON.stringify({ name: 'Network bot' }) : undefined);
  });
  try {
    assert.equal(await request('GET', '/api/state', host), 200);
    assert.equal(await request('POST', '/api/bots', host, origin), 201);
    for (const name of ['devbox', 'DEVBOX', 'devbox.', 'devbox.tailc6b81b.ts.net', 'DEVBOX.tailc6b81b.ts.net.']) {
      const requestHost = `${name}:${port}`;
      assert.equal(await request('GET', '/', requestHost), 404, name);
      assert.equal(await request('GET', '/api/state', requestHost), 200, name);
      const ownOrigin = new URL(`http://${requestHost}`).origin;
      assert.equal(await request('POST', '/api/bots', requestHost, ownOrigin), 201, name);
      assert.equal(await request('POST', '/api/bots', requestHost, 'http://attacker.invalid'), 403, name);
    }
    assert.equal(await request('POST', '/api/bots', host, 'http://attacker.invalid'), 403);
    assert.equal(await request('GET', '/api/state', `attacker.invalid:${port}`), 403);
    assert.equal(await request('GET', '/api/state', `192.168.10.155:${port}`), 403);
    assert.equal(await request('GET', '/api/state', `192.168.10.154:${Number(port) + 1}`), 403);
    assert.equal(await request('GET', '/api/state', `192.168.10.154:${port}@attacker.invalid`), 403);
  } finally { await f.close(); }
});

test('workspace browse/read/save observes hashes and rejects traversal, parent links, hard links, binary, and oversized files', async () => {
  const f = await fixture();
  try {
    assert.equal((await f.api('/api/files')).status, 400);
    assert.equal((await f.api('/api/settings', settings(f.workspace), 'PUT')).status, 200);
    await mkdir(path.join(f.workspace, 'nested'));
    await writeFile(path.join(f.root, 'outside.txt'), 'outside stays unchanged\n');
    await symlink(path.join(f.root, 'outside.txt'), path.join(f.workspace, 'linked.txt'));
    await symlink(f.root, path.join(f.workspace, 'parent-link'));
    await link(path.join(f.root, 'outside.txt'), path.join(f.workspace, 'hard.txt'));
    await writeFile(path.join(f.workspace, 'binary.bin'), Buffer.from([0, 1, 2]));
    await writeFile(path.join(f.workspace, 'large.txt'), 'x'.repeat(1024 * 1024 + 1));
    const listing = await (await f.api('/api/files?path=')).json();
    assert.equal(listing.entries[0].name, 'nested');
    assert.equal(listing.entries.some((entry: { name: string }) => entry.name === 'linked.txt'), false);
    const document = await (await f.api('/api/file?path=notes.txt')).json() as FileDocument;
    assert.equal(document.content, 'original\n');
    assert.match(document.hash, /^[0-9a-f]{64}$/);
    const saved = await f.api('/api/file', { ...document, content: 'saved from browser\n' }, 'PUT');
    assert.equal(saved.status, 200);
    assert.equal(await readFile(path.join(f.workspace, 'notes.txt'), 'utf8'), 'saved from browser\n');
    assert.equal((await f.api('/api/file', { ...document, content: 'stale write' }, 'PUT')).status, 409);
    const badPaths = ['../outside.txt', '/etc/passwd', 'nested/../../outside.txt', 'parent-link/outside.txt', 'linked.txt', 'hard.txt', 'nested/../notes.txt'];
    for (const filename of badPaths) {
      assert.equal((await f.api(`/api/file?path=${encodeURIComponent(filename)}`)).status, 403, filename);
      assert.equal((await f.api('/api/file', { path: filename, content: 'blocked', hash: '' }, 'PUT')).status, 403, filename);
    }
    assert.equal((await f.api('/api/file?path=binary.bin')).status, 415);
    assert.equal((await f.api('/api/file?path=large.txt')).status, 413);
    assert.equal((await f.api('/api/file?path=missing.txt')).status, 404);
    assert.equal((await f.api('/api/file', { path: 'new.txt', content: 'new file\n', hash: '' }, 'PUT')).status, 200);
    assert.equal(await readFile(path.join(f.root, 'outside.txt'), 'utf8'), 'outside stays unchanged\n');
    const alias = path.join(f.root, 'workspace-alias');
    await symlink(f.workspace, alias);
    assert.equal((await f.api('/api/settings', settings(alias), 'PUT')).status, 403);
  } finally { await f.close(); }
});

test('provider test uses the models endpoint, reports HTTP failures, and does not echo provider secrets', async () => {
  const f = await fixture();
  let status = 200;
  const provider = createServer((request, response) => {
    assert.equal(request.url, '/v1/models');
    response.writeHead(status, { 'Content-Type': 'application/json' });
    response.end(JSON.stringify(status === 200 ? { data: [{ id: 'test-model' }, { id: 'other' }] } : { error: 'super-secret-provider-body' }));
  });
  await new Promise<void>(resolve => provider.listen(0, '127.0.0.1', resolve));
  try {
    const baseUrl = `http://127.0.0.1:${(provider.address() as AddressInfo).port}/v1`;
    await f.api('/api/settings', { ...settings(f.workspace), baseUrl }, 'PUT');
    const check = await (await f.api('/api/provider/test', {})).json();
    assert.equal(check.ok, true);
    assert.deepEqual(check.models, ['other', 'test-model']);
    status = 401;
    const failed = await (await f.api('/api/provider/test', {})).json();
    assert.equal(failed.ok, false);
    assert.match(failed.error, /401/);
    assert.equal(JSON.stringify(failed).includes('super-secret-provider-body'), false);
  } finally { provider.closeAllConnections(); await new Promise<void>(resolve => provider.close(() => resolve())); await f.close(); }
});

test('invalid state fails visibly without replacing the file and active startup states become interrupted', async () => {
  const f = await fixture();
  try {
    const bot = f.backend.store.bots[0];
    const conversation = await f.backend.store.addConversation(bot.id);
    conversation.run = { id: 'interrupted-run', status: 'running', startedAt: new Date().toISOString() };
    await f.backend.store.changed();
    await f.backend.close();
    const restarted = await createBackend({ dataDir: f.dataDir, serveWeb: false });
    assert.equal(restarted.store.conversation(conversation.id).run.status, 'interrupted');
    assert.match(restarted.store.conversation(conversation.id).run.error!, /server stopped/);
    await restarted.close();
    await writeFile(path.join(f.dataDir, 'state.json'), '{invalid');
    await assert.rejects(createBackend({ dataDir: f.dataDir, serveWeb: false }), /existing state.json was not replaced/);
    assert.equal(await readFile(path.join(f.dataDir, 'state.json'), 'utf8'), '{invalid');
  } finally { await f.close(); }
});
