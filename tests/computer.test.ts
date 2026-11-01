import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { once } from 'node:events';
import { createServer, request as httpRequest } from 'node:http';
import type { Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import { test } from 'node:test';
import type { ComputerAction, ComputerActionRequest } from '../shared/computer.js';
import { ManagedComputer } from '../server/computer.js';
import {
  ComputerNetworkPolicy, ComputerProxy, isGlobalComputerAddress,
} from '../server/computer-network.js';
import { HttpError } from '../server/files.js';

const denied = (status: number) => (error: unknown) => error instanceof HttpError && error.status === status;
const start = (computer: ManagedComputer, origins: string[]) => computer.start({ requestId: randomUUID(), userInitiated: true, origins });

function command(computer: ManagedComputer, action: ComputerAction): ComputerActionRequest {
  const status = computer.status();
  return { requestId: randomUUID(), epoch: status.epoch, frameId: status.frameId, action };
}

async function listen(server: Server): Promise<string> {
  await new Promise<void>(resolve => server.listen(0, '127.0.0.1', resolve));
  return `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
}

async function close(server: Server): Promise<void> {
  server.closeAllConnections();
  await new Promise<void>(resolve => server.close(() => resolve()));
}

async function fixture() {
  const hits: string[] = [];
  const forbidden: string[] = [];
  const other = createServer((request, response) => { forbidden.push(request.url!); response.end('private target'); });
  const privateOrigin = await listen(other);
  let signalHang!: () => void;
  const hanging = new Promise<void>(resolve => { signalHang = resolve; });
  const site = createServer((request, response) => {
    hits.push(`${request.method} ${request.url}`);
    if (request.url === '/hang') { signalHang(); return; }
    if (request.url === '/redirect') { response.writeHead(302, { Location: privateOrigin + '/redirected' }); response.end(); return; }
    if (request.url === '/download') { response.writeHead(200, { 'Content-Disposition': 'attachment; filename="denied.txt"' }); response.end('not a file'); return; }
    if (request.url === '/worker.js') { response.writeHead(200, { 'Content-Type': 'application/javascript' }); response.end('self.addEventListener("install",()=>{});'); return; }
    if (request.url === '/popup') { response.end('popup must not load'); return; }
    response.writeHead(200, { 'Content-Type': 'text/html', 'Set-Cookie': 'fixture=private; SameSite=Lax' });
    response.end(`<!doctype html><html><head><title>Managed computer fixture</title></head>
      <body style="margin:24px;font:16px sans-serif"><h1>Public fixture</h1>
      <label>Query <input id="query" aria-label="Query" type="text"></label>
      <button id="count" onclick="this.textContent='Count '+(++window.count)">Count 0</button>
      <button onclick="this.textContent=document.querySelector('#query').value">Show query</button>
      <button onclick="window.open('/popup')">Popup</button>
      <a href="/download" download>Download</a>
      <input aria-label="Password" type="password"><input aria-label="Upload" type="file">
      <a href="${privateOrigin}/clicked">Private destination</a>
      <div style="height:1200px"></div><button>Bottom</button>
      <script>
      window.count=0;
      const attempts = [];
      try { const peer=new RTCPeerConnection(); peer.close(); attempts.push('rtc open'); } catch { attempts.push('rtc blocked'); }
      try { new WebSocket(location.origin.replace('http:', 'ws:')+'/ws'); attempts.push('ws open'); } catch { attempts.push('ws blocked'); }
      const state=document.createElement('button'); state.textContent=attempts.join(', '); document.body.prepend(state);
      fetch('${privateOrigin}/fetch').catch(()=>{});
      const image=new Image(); image.src='${privateOrigin}/image'; document.body.append(image);
      </script></body></html>`);
  });
  const origin = await listen(site);
  const computer = new ManagedComputer({ allowTestOrigins: [origin], testBrowser: { channel: 'chrome', chromiumSandbox: true } });
  return {
    computer, origin, privateOrigin, hits, forbidden, hanging,
    close: async () => { await computer.stop(); await Promise.all([close(site), close(other)]); },
  };
}

async function clickLabel(computer: ManagedComputer, label: string) {
  const observation = await computer.perform(command(computer, { type: 'observe' }));
  const control = observation.frame.controls.find(control => control.label === label);
  assert.ok(control, `Expected visible control ${label}`);
  return computer.perform(command(computer, { type: 'click', x: control.bounds.x + control.bounds.width / 2, y: control.bounds.y + control.bounds.height / 2 }));
}

test('network policy rejects non-global IPv4, IPv6, mapped IPv6, and ambiguous addresses', () => {
  for (const address of [
    '0.0.0.0', '10.1.2.3', '100.64.0.1', '127.0.0.1', '169.254.169.254', '172.16.1.1',
    '192.168.1.1', '192.0.2.1', '198.18.0.1', '198.51.100.1', '203.0.113.1', '224.0.0.1', '255.255.255.255',
    '::', '::1', '::ffff:127.0.0.1', '::ffff:7f00:1', '::ffff:10.0.0.1', '::ffff:8.8.8.8',
    'fc00::1', 'fe80::1', 'fe80::1%lo', 'ff02::1', '64:ff9b::7f00:1', '2001:db8::1', '2002:7f00:1::1', '3fff::1',
    '127.1', '2130706433', 'not-an-address',
  ]) assert.equal(isGlobalComputerAddress(address), false, address);
  for (const address of ['1.1.1.1', '8.8.8.8', '93.184.216.34', '2606:4700:4700::1111', '2001:4860:4860::8888']) {
    assert.equal(isGlobalComputerAddress(address), true, address);
  }
});

test('DNS validates every answer and revalidates a hostname instead of trusting an earlier lookup', async () => {
  let answers = [{ address: '93.184.216.34', family: 4 }];
  let lookups = 0;
  const policy = new ComputerNetworkPolicy(['https://public.example'], { lookup: async () => { lookups++; return answers; } });
  assert.equal((await policy.resolve('https://public.example/path')).address, '93.184.216.34');
  answers = [{ address: '93.184.216.34', family: 4 }, { address: '127.0.0.1', family: 4 }];
  await assert.rejects(policy.resolve('https://public.example/path'), denied(403));
  answers = [{ address: '10.0.0.1', family: 4 }];
  await assert.rejects(policy.resolve('https://public.example/path'), denied(403));
  answers = [{ address: '::ffff:127.0.0.1', family: 6 }];
  await assert.rejects(policy.resolve('https://public.example/path'), denied(403));
  answers = [];
  await assert.rejects(policy.resolve('https://public.example/path'), denied(403));
  assert.equal(lookups, 5);
  await assert.rejects(policy.resolve('http://public.example/path'), denied(403));
  await assert.rejects(policy.resolve('https://user:secret@public.example/path'), denied(403));
  await assert.rejects(policy.resolve('file:///etc/passwd'), denied(403));
  await assert.rejects(new ComputerNetworkPolicy(['http://127.1']).resolve('http://127.1/'), denied(403));
});

test('fixture exceptions are loopback-only, origin-specific, and never an action input', async () => {
  assert.throws(() => new ComputerNetworkPolicy(['https://example.com/path']), denied(400));
  assert.throws(() => new ComputerNetworkPolicy(['http://fixture.example'], { allowTestOrigins: ['http://fixture.example'] }), denied(400));
  const policy = new ComputerNetworkPolicy(['http://127.0.0.1:12345', 'http://127.0.0.1:12346'], { allowTestOrigins: ['http://127.0.0.1:12345'] });
  assert.equal((await policy.resolve('http://127.0.0.1:12345/path')).address, '127.0.0.1');
  await assert.rejects(policy.resolve('http://127.0.0.1:12346/path'), denied(403));
});

test('the loopback proxy authenticates callers, pins the validated address, blocks rebinding and closes sockets', async () => {
  const received: { host?: string; cookie?: string; authorization?: string }[] = [];
  const site = createServer((request, response) => { received.push(request.headers); response.end('pinned fixture'); });
  const siteOrigin = await listen(site);
  const port = new URL(siteOrigin).port;
  const policy = new ComputerNetworkPolicy([siteOrigin], { allowTestOrigins: [siteOrigin] });
  const original = policy.resolve.bind(policy);
  let rebinding = false;
  policy.resolve = async input => {
    if (rebinding) throw new HttpError(403, 'Rebinding blocked.');
    const target = await original(input.replace('public.example', '127.0.0.1'));
    return { ...target, url: new URL(input) };
  };
  const proxy = new ComputerProxy(policy);
  await proxy.start();
  const configuration = proxy.configuration;
  async function request(authenticated: boolean, method = 'GET') {
    return new Promise<{ status: number; body: string }>((resolve, reject) => {
      const request = httpRequest(configuration.server, {
        method, path: `http://public.example:${port}/pinned`,
        headers: {
          ...(authenticated ? { 'Proxy-Authorization': `Basic ${Buffer.from(`${configuration.username}:${configuration.password}`).toString('base64')}` } : {}),
          Cookie: 'secret=cookie', Authorization: 'Bearer secret',
        },
      }, response => {
        let body = '';
        response.setEncoding('utf8');
        response.on('data', chunk => { body += chunk; });
        response.on('end', () => resolve({ status: response.statusCode!, body }));
      });
      request.on('error', reject);
      request.end();
    });
  }
  try {
    assert.equal((await request(false)).status, 407);
    assert.deepEqual(await request(true), { status: 200, body: 'pinned fixture' });
    assert.equal(received[0].host, `public.example:${port}`);
    assert.equal(received[0].cookie, undefined);
    assert.equal(received[0].authorization, undefined);
    assert.equal((await request(true, 'POST')).status, 403);
    rebinding = true;
    assert.equal((await request(true)).status, 403);
    assert.equal(received.length, 1);
    await proxy.stop();
    await assert.rejects(request(true));
  } finally { await proxy.stop(); await close(site); }
});

test('the computer starts stopped and requires an explicit user operation with public origins', async () => {
  const computer = new ManagedComputer();
  assert.equal(computer.status().state, 'stopped');
  assert.equal(computer.status().frameId, null);
  await assert.rejects(computer.start({ requestId: randomUUID(), userInitiated: false, origins: ['https://example.com'] } as unknown as Parameters<ManagedComputer['start']>[0]), denied(400));
  await assert.rejects(start(computer, ['http://127.0.0.1:12345']), denied(403));
  assert.equal(computer.status().state, 'stopped');
  await assert.rejects(computer.screenshot({ requestId: randomUUID(), epoch: computer.status().epoch, frameId: null }), denied(409));
  assert.equal((await computer.stop()).state, 'stopped');
});

test('real sandboxed Chrome provides PNG frames, controls, typed actions, stale guards, and at-most-once execution', { timeout: 45_000 }, async () => {
  const f = await fixture();
  try {
    const ready = await start(f.computer, [f.origin]);
    assert.equal(ready.state, 'running');
    assert.equal(ready.sandboxed, true);
    assert.deepEqual(ready.viewport, { width: 960, height: 600 });
    const navigated = await f.computer.perform(command(f.computer, { type: 'navigate', url: f.origin }));
    assert.equal(navigated.frame.title, 'Managed computer fixture');
    assert.ok(navigated.frame.controls.some(control => control.label === 'rtc blocked, ws blocked'));
    const screenshotRequest = command(f.computer, { type: 'observe' });
    const shot = await f.computer.screenshot({ requestId: screenshotRequest.requestId, epoch: screenshotRequest.epoch, frameId: screenshotRequest.frameId });
    assert.equal(shot.screenshotBuffer.subarray(0, 8).toString('hex'), '89504e470d0a1a0a');
    assert.equal(shot.screenshotBuffer.readUInt32BE(16), 960);
    assert.equal(shot.screenshotBuffer.readUInt32BE(20), 600);
    assert.equal(JSON.stringify(shot.frame).includes('fixture=private'), false);
    const stale = { ...command(f.computer, { type: 'observe' }), frameId: navigated.frame.id };
    await assert.rejects(f.computer.perform(stale), denied(409));
    await clickLabel(f.computer, 'Query');
    await f.computer.perform(command(f.computer, { type: 'type', text: 'typed fixture text' }));
    const shown = await clickLabel(f.computer, 'Show query');
    assert.ok(shown.frame.controls.some(control => control.label === 'typed fixture text'));
    const count = shown.frame.controls.find(control => control.label === 'Count 0')!;
    const click = command(f.computer, { type: 'click', x: count.bounds.x + count.bounds.width / 2, y: count.bounds.y + count.bounds.height / 2 });
    const first = f.computer.perform(click);
    const duplicate = assert.rejects(f.computer.perform(click), denied(409));
    const raced = assert.rejects(f.computer.perform({ ...click, requestId: randomUUID() }), denied(409));
    const clicked = await first;
    await Promise.all([duplicate, raced]);
    assert.ok(clicked.frame.controls.some(control => control.label === 'Count 1'));
    await assert.rejects(f.computer.perform({ ...click, requestId: randomUUID() }), denied(409));
    await f.computer.perform(command(f.computer, { type: 'key', key: 'Tab' }));
    await f.computer.perform(command(f.computer, { type: 'scroll', deltaY: 700 }));
    assert.deepEqual(f.forbidden, []);
    const oldEpoch = f.computer.status().epoch;
    await f.computer.stop();
    assert.notEqual(f.computer.status().epoch, oldEpoch);
    await start(f.computer, [f.origin]);
    await assert.rejects(f.computer.perform({ ...command(f.computer, { type: 'observe' }), epoch: oldEpoch }), denied(409));
    await assert.rejects(f.computer.perform(click), denied(409));
  } finally { await f.close(); }
});

test('real Chrome blocks private redirects, subresources, sensitive controls, downloads, uploads, and unsafe actions', { timeout: 40_000 }, async () => {
  const f = await fixture();
  try {
    await start(f.computer, [f.origin]);
    const loaded = await f.computer.perform(command(f.computer, { type: 'navigate', url: f.origin }));
    for (const label of ['Password', 'Upload', 'Download', 'Private destination']) {
      const control = loaded.frame.controls.find(control => control.label === label);
      assert.ok(control?.blocked, label);
    }
    const password = loaded.frame.controls.find(control => control.label === 'Password')!;
    await assert.rejects(f.computer.perform(command(f.computer, { type: 'click', x: password.bounds.x + 2, y: password.bounds.y + 2 })), denied(403));
    assert.equal(f.computer.status().frameId, null);
    await f.computer.perform(command(f.computer, { type: 'observe' }));
    await clickLabel(f.computer, 'Popup');
    assert.equal(f.hits.some(hit => hit.includes('/popup')), false);
    const invalidKey = command(f.computer, { type: 'key', key: 'Control+L' } as unknown as ComputerAction);
    await assert.rejects(f.computer.perform(invalidKey), denied(400));
    await assert.rejects(f.computer.perform(command(f.computer, { type: 'navigate', url: 'file:///etc/passwd' })), denied(403));
    await f.computer.perform(command(f.computer, { type: 'observe' }));
    await assert.rejects(f.computer.perform(command(f.computer, { type: 'navigate', url: f.privateOrigin })), denied(403));
    await f.computer.perform(command(f.computer, { type: 'observe' }));
    await assert.rejects(f.computer.perform(command(f.computer, { type: 'navigate', url: f.origin + '/redirect' })));
    assert.deepEqual(f.forbidden, []);
  } finally { await f.close(); }
});

test('stop aborts an awaiting navigation and queued actions and destroys the upstream connection', { timeout: 30_000 }, async () => {
  const f = await fixture();
  try {
    await start(f.computer, [f.origin]);
    const navigationRequest = command(f.computer, { type: 'navigate', url: f.origin + '/hang' });
    const pending = assert.rejects(f.computer.perform(navigationRequest), denied(409));
    const queued = assert.rejects(f.computer.perform({ ...navigationRequest, requestId: randomUUID(), action: { type: 'observe' } }), denied(409));
    await f.hanging;
    const stopped = await f.computer.stop();
    assert.equal(stopped.state, 'stopped');
    assert.equal(stopped.busy, false);
    assert.equal(stopped.frameId, null);
    await Promise.all([pending, queued]);
    assert.equal((await f.computer.stop()).state, 'stopped');
  } finally { await f.close(); }
});

test('Chrome launch failures are explicit and never fall back to an unsandboxed browser', { timeout: 15_000 }, async () => {
  const f = await fixture();
  const computer = new ManagedComputer({ allowTestOrigins: [f.origin], testBrowser: { executablePath: '/nonexistent/managed-computer-test-chrome' } });
  try {
    await assert.rejects(start(computer, [f.origin]), denied(503));
    assert.equal(computer.status().state, 'stopped');
    assert.equal(computer.status().sandboxed, false);
    assert.match(computer.status().error!, /sandbox/);
  } finally { await computer.stop(); await f.close(); }
});
