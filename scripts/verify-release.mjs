// Black-box check: only the binary is copied to a fresh directory. No Node executable or
// companion dist/engine exists on the server's PATH. The test driver still needs Node.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { copyFile, mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { createServer, request as httpRequest } from 'node:http';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';

const source = path.resolve(process.argv[2] ?? 'target/release/grok-bot');
const root = await mkdtemp(path.join(tmpdir(), 'grok-bot-release-'));
const binary = path.join(root, 'grok-bot');
let child;
let closed;
let logs = '';
const provider = createServer(async (request, response) => {
  if (request.url === '/v1/models') {
    response.setHeader('Content-Type', 'application/json');
    response.end(JSON.stringify({ data: [{ id: 'release-test-model' }] }));
    return;
  }
  if (request.url !== '/v1/chat/completions') { response.writeHead(404).end(); return; }
  let body = '';
  for await (const chunk of request) body += chunk;
  const input = JSON.parse(body);
  const text = 'Standalone release reply.';
  const completion = { id: 'release-smoke', created: 1, model: input.model };
  if (input.stream) {
    response.writeHead(200, { 'Content-Type': 'text/event-stream' });
    for (const [delta, finish_reason] of [[{ role: 'assistant', content: text }, null], [{}, 'stop']]) {
      response.write(`data: ${JSON.stringify({ ...completion, object: 'chat.completion.chunk', choices: [{ index: 0, delta, finish_reason }] })}\n\n`);
    }
    response.end('data: [DONE]\n\n');
  } else {
    response.setHeader('Content-Type', 'application/json');
    response.end(JSON.stringify({ ...completion, object: 'chat.completion', choices: [{ index: 0, message: { role: 'assistant', content: text }, finish_reason: 'stop' }] }));
  }
});

try {
  await copyFile(source, binary);
  const env = { ...process.env, HOME: root, PATH: path.join(root, 'no-executables'), GROK_BOT_WEB_ROOT: '', GROK_BOT_DATA_DIR: path.join(root, 'data') };
  delete env.GROK_BOT_WEB_ROOT;
  for (const [flag, expected] of [['--help', 'GROK_BOT_WEB_ROOT'], ['--version', 'grok-bot '], ['--licenses', 'Apache License']]) {
    const result = spawnSync(binary, [flag], { cwd: root, env, encoding: 'utf8', timeout: 10_000 });
    assert.equal(result.status, 0, result.stderr || result.error?.message);
    assert.ok(result.stdout.includes(expected), `${flag}: missing ${expected}`);
  }
  assert.notEqual(spawnSync(binary, ['--unknown'], { cwd: root, env, timeout: 10_000 }).status, 0);
  const invalid = spawnSync(binary, [], { cwd: root, env: { ...env, GROK_BOT_WEB_ROOT: path.join(root, 'missing') }, encoding: 'utf8', timeout: 10_000 });
  assert.notEqual(invalid.status, 0);
  assert.match(invalid.stderr, /GROK_BOT_WEB_ROOT must contain index.html/);

  const reservation = createServer();
  await new Promise(resolve => reservation.listen(0, '127.0.0.1', resolve));
  const port = reservation.address().port;
  await new Promise(resolve => reservation.close(resolve));
  const origin = `http://127.0.0.1:${port}`;
  child = spawn(binary, [], { cwd: root, env: { ...env, HOST: '127.0.0.1', PORT: String(port) }, stdio: ['ignore', 'pipe', 'pipe'] });
  closed = new Promise((resolve, reject) => { child.once('error', reject); child.once('close', (code, signal) => resolve({ code, signal })); });
  child.stdout.on('data', chunk => { logs += chunk; });
  child.stderr.on('data', chunk => { logs += chunk; });
  const request = (route, options) => fetch(`${origin}${route}`, { ...options, signal: AbortSignal.timeout(5000) });
  let ready = false;
  for (let attempt = 0; attempt < 150; attempt++) {
    if (child.exitCode !== null) throw new Error(`Release exited before startup: ${logs}`);
    try { ready = (await request('/api/state')).ok; } catch {}
    if (ready) break;
    await delay(200);
  }
  assert.ok(ready, `Release did not start: ${logs}`);
  const index = await request('/');
  assert.equal(index.status, 200);
  assert.match(index.headers.get('content-type'), /text\/html/);
  assert.equal(index.headers.get('x-content-type-options'), 'nosniff');
  const html = await index.text();
  const assets = [...html.matchAll(/(?:src|href)="(\/assets\/[^"?]+)"/g)].map(match => match[1]);
  assert.ok(assets.some(asset => asset.endsWith('.js')));
  assert.ok(assets.some(asset => asset.endsWith('.css')));
  for (const asset of assets) {
    const response = await request(asset);
    assert.equal(response.status, 200, asset);
    assert.match(response.headers.get('content-type'), asset.endsWith('.js') ? /javascript/ : /css/);
    assert.ok((await response.arrayBuffer()).byteLength > 0);
  }
  const head = await request(assets[0], { method: 'HEAD' });
  assert.equal(head.status, 200);
  assert.equal((await head.arrayBuffer()).byteLength, 0);
  assert.equal(await (await request('/chat?bot=example')).text(), html);
  assert.equal((await request('/assets/missing.js')).status, 404);
  assert.equal((await request('/internal/missing')).status, 404);
  // Node's fetch may replace Host; use HTTP directly to exercise the actual wire header.
  const foreignHostStatus = await new Promise((resolve, reject) => {
    const probe = httpRequest(origin, { headers: { Host: `untrusted.example:${port}` } }, response => { response.resume(); resolve(response.statusCode); });
    probe.on('error', reject);
    probe.setTimeout(5000, () => probe.destroy(new Error('Host probe timed out')));
    probe.end();
  });
  assert.equal(foreignHostStatus, 403);
  assert.equal((await request('/api/missing')).status, 404);

  await new Promise(resolve => provider.listen(0, '127.0.0.1', resolve));
  const api = async (method, route, body) => {
    const response = await request(route, { method, headers: { Origin: origin, 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify(body) });
    const text = await response.text();
    assert.ok(response.ok, `${route}: ${response.status} ${text}`);
    return JSON.parse(text);
  };
  const state = await api('PUT', '/api/settings', { baseUrl: `http://127.0.0.1:${provider.address().port}/v1`, model: 'release-test-model', workspace: '' });
  assert.equal(state.runtime.available, true);
  const { conversation } = await api('POST', '/api/conversations', { botId: state.bots[0].id });
  await api('POST', `/api/conversations/${conversation.id}/messages`, { text: 'Reply to this standalone release smoke test.' });
  let completed;
  for (let attempt = 0; attempt < 150; attempt++) {
    const snapshot = await (await request('/api/state')).json();
    completed = snapshot.conversations.find(item => item.id === conversation.id);
    if (['completed', 'error', 'cancelled', 'interrupted'].includes(completed.run.status)) break;
    await delay(200);
  }
  assert.equal(completed.run.status, 'completed', JSON.stringify(completed));
  assert.ok(completed.messages.some(message => message.role === 'assistant' && message.text.includes('Standalone release reply.')));

  child.kill('SIGTERM');
  const result = await Promise.race([closed, delay(10_000).then(() => { throw new Error('Release did not shut down after SIGTERM'); })]);
  assert.equal(result.code, 0, `Release shutdown failed: ${logs}`);
  child = undefined;

  // The explicit disk override must still win over the embedded frontend.
  const override = path.join(root, 'override');
  await mkdir(override);
  await writeFile(path.join(override, 'index.html'), '<!doctype html><title>Override frontend</title>');
  child = spawn(binary, [], { cwd: root, env: { ...env, HOST: '127.0.0.1', PORT: String(port), GROK_BOT_WEB_ROOT: override }, stdio: ['ignore', 'pipe', 'pipe'] });
  closed = new Promise((resolve, reject) => { child.once('error', reject); child.once('close', (code, signal) => resolve({ code, signal })); });
  let overrideReady = false;
  for (let attempt = 0; attempt < 150; attempt++) {
    if (child.exitCode !== null) throw new Error('Release exited before override startup');
    try { overrideReady = (await (await request('/')).text()).includes('Override frontend'); } catch {}
    if (overrideReady) break;
    await delay(200);
  }
  assert.ok(overrideReady, 'Explicit GROK_BOT_WEB_ROOT was not served');
  console.log('Standalone release passed: copied binary, no Node/companion assets, CLI/notices, embedded HTML/JS/CSS, HEAD/SPA, Host/API guards, real engine chat, SIGTERM, explicit frontend override.');
} finally {
  if (child) {
    child.kill('SIGTERM');
    const timeout = setTimeout(() => child?.kill('SIGKILL'), 5000);
    await closed.catch(() => {});
    clearTimeout(timeout);
  }
  provider.closeAllConnections();
  if (provider.listening) await new Promise(resolve => provider.close(resolve));
  // root is the exact directory created by mkdtemp above, never a computed user target.
  assert.ok(root.startsWith(path.join(tmpdir(), 'grok-bot-release-')));
  await rm(root, { recursive: true, force: true });
}
