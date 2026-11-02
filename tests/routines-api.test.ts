import assert from 'node:assert/strict';
import test from 'node:test';
import { randomUUID } from 'node:crypto';
import { mkdtemp, mkdir, readFile, rename, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { AddressInfo } from 'node:net';
import { request } from 'node:http';
import { createBackend } from '../server/app.js';
import { stateSummary } from './state-summary.js';
import type { AppState, RoutineInput, RoutineRunResult } from '../shared/types.js';

const fakeAgent = `#!/usr/bin/env node
import { createInterface } from 'node:readline';
import { randomUUID } from 'node:crypto';
if (process.argv.includes('--version')) { console.log('grok-bot-engine 0.1.0 (2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8)'); process.exit(0); }
let sessionId;
const send = value => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', ...value }) + '\\n');
createInterface({ input: process.stdin }).on('line', line => {
  const { id, method, params } = JSON.parse(line);
  if (method === 'initialize') send({ id, result: { protocolVersion: 1, agentCapabilities: { mcpCapabilities: { http: true } }, agentInfo: { name: 'routine-fixture', version: '1' } } });
  if (method === 'session/new') { sessionId = randomUUID(); send({ id, result: { sessionId } }); }
  if (method === 'session/prompt' && params.prompt[0].text !== 'hold') {
    send({ method: 'session/update', params: { sessionId, update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'Routine completed locally.' } } } });
    send({ id, result: { stopReason: 'end_turn' } });
  }
});
`;

async function fixture(options: { autoStart?: boolean; pollIntervalMs?: number } = {}) {
  const root = await mkdtemp(join(tmpdir(), 'grok-routines-api-'));
  const workspace = join(root, 'workspace');
  const command = join(root, 'fake-agent.mjs');
  await mkdir(workspace);
  await writeFile(command, fakeAgent, { mode: 0o700 });
  let current = new Date('2026-01-01T00:00:00Z');
  const now = () => new Date(current);
  const backend = await createBackend({
    dataDir: join(root, 'data'), serveWeb: false, allowedHosts: ['192.168.25.10'],
    runtime: { command, args: [], cancelDeadlineMs: 25, startupTimeoutMs: 3000 },
    routines: { now, autoStart: options.autoStart ?? false, pollIntervalMs: options.pollIntervalMs },
  });
  const listen = () => new Promise<void>(resolve => backend.server.listen(0, '127.0.0.1', resolve));
  await listen();
  const origin = `http://127.0.0.1:${(backend.server.address() as AddressInfo).port}`;
  const api = (route: string, body?: unknown, method = body === undefined ? 'GET' : 'POST', headers: Record<string, string> = {}) => fetch(origin + route, {
    method, headers: { Origin: origin, 'X-Grok-Bot': '1', 'Content-Type': 'application/json', ...headers },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const input = (extra: Partial<RoutineInput> = {}): RoutineInput => ({
    botId: backend.store.bots[0].id, name: 'HTTP routine', prompt: 'Fixture prompt', enabled: true,
    schedule: { kind: 'interval', minutes: 5 }, ...extra,
  });
  const configure = async (apiKey = '') => {
    const response = await api('/api/settings', { baseUrl: 'http://127.0.0.1:12345/v1', model: 'fixture', workspace, apiKey }, 'PUT');
    assert.equal(response.status, 200);
  };
  const wait = async (predicate: () => boolean) => {
    const deadline = Date.now() + 5000;
    while (!predicate()) {
      if (Date.now() >= deadline) assert.fail(stateSummary(backend.store.snapshot()));
      await new Promise(resolve => setTimeout(resolve, 10));
    }
  };
  return {
    root, workspace, backend, origin, api, input, configure, wait,
    setTime: (value: string) => { current = new Date(value); },
    close: async () => { await backend.close(); await rm(root, { recursive: true, force: true }); },
  };
}

test('successful routine waits do not build snapshot diagnostics', async (context) => {
  const f = await fixture();
  const snapshot = context.mock.method(f.backend.store, 'snapshot');
  try {
    let polls = 0;
    await f.wait(() => ++polls > 1);
    assert.equal(polls, 2);
    assert.equal(snapshot.mock.callCount(), 0);
  } finally { snapshot.mock.restore(); await f.close(); }
});

test('routine HTTP routes implement typed CRUD, toggle, manual result, validation, and history retention', async () => {
  const f = await fixture();
  try {
    const created = await f.api('/api/routines', f.input());
    assert.equal(created.status, 201);
    const state = await created.json() as AppState;
    const routine = state.routines[0];
    assert.equal(routine.nextRunAt, '2026-01-01T00:05:00.000Z');
    assert.deepEqual(state.routineRuns, []);
    assert.equal((await f.api(`/api/routines/${routine.id}`, f.input({ name: 'Edited' }), 'PUT')).status, 200);
    const paused = await f.api(`/api/routines/${routine.id}`, { enabled: false }, 'PATCH');
    assert.equal(paused.status, 200);
    assert.equal(((await paused.json()) as AppState).routines[0].nextRunAt, undefined);
    const resumed = await f.api(`/api/routines/${routine.id}`, { enabled: true }, 'PATCH');
    assert.equal(resumed.status, 200);
    assert.equal((await f.api('/api/routines', f.input({ schedule: { kind: 'once', at: '2025-01-01T00:00:00Z' } }))).status, 400);
    assert.equal((await f.api('/api/routines', { ...f.input(), schedule: { kind: 'daily', time: '12:00', timezone: 'Unknown/Timezone' } })).status, 400);
    assert.equal((await f.api('/api/routines', { ...f.input(), unexpected: true })).status, 400);
    assert.equal((await f.api(`/api/routines/${routine.id}`, { enabled: 'yes' }, 'PATCH')).status, 400);
    assert.equal((await f.api(`/api/routines/${routine.id}/run`, { text: 'Not accepted' })).status, 400);
    assert.equal((await f.api(`/api/routines/${routine.id}`, { force: true }, 'DELETE')).status, 400);
    assert.equal((await f.api(`/api/routines/${randomUUID()}`, {}, 'DELETE')).status, 404);
    const executed = await f.api(`/api/routines/${routine.id}/run`, {});
    assert.equal(executed.status, 202);
    const result = await executed.json() as RoutineRunResult;
    assert.equal(result.run.routineName, 'Edited');
    assert.equal(result.run.trigger, 'manual');
    assert.deepEqual(result.run, result.state.routineRuns[0]);
    const conversation = result.state.conversations.find(item => item.id === result.run.conversationId)!;
    assert.equal(conversation.run.status, 'error');
    assert.match(conversation.run.error!, /Configure/);
    assert.equal(conversation.messages.length, 0);
    assert.ok(conversation.run.startedAt);
    assert.ok(conversation.run.finishedAt);
    const deleted = await f.api(`/api/routines/${routine.id}`, {}, 'DELETE');
    assert.equal(deleted.status, 200);
    const retained = await deleted.json() as AppState;
    assert.deepEqual(retained.routines, []);
    assert.equal(retained.routineRuns.length, 1);
    assert.equal(retained.conversations.length, 1);
  } finally { await f.close(); }
});

test('routine APIs retain JSON mutation guards and accept an explicitly allowed LAN HTTP host', async () => {
  const f = await fixture();
  try {
    assert.equal((await f.api('/api/routines', f.input(), 'POST', { 'X-Grok-Bot': '' })).status, 403);
    assert.equal((await f.api('/api/routines', f.input(), 'POST', { Origin: 'http://attacker.invalid' })).status, 403);
    const response = await fetch(f.origin + '/api/routines', { method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify(f.input()) });
    assert.equal(response.status, 403);
    const host = `192.168.25.10:${new URL(f.origin).port}`;
    const status = await new Promise<number>((resolve, reject) => {
      const req = request(f.origin + '/api/routines', {
        method: 'POST', headers: { Host: host, Origin: `http://${host}`, 'Content-Type': 'application/json', 'X-Grok-Bot': '1' },
      }, res => { res.resume(); res.once('end', () => resolve(res.statusCode!)); });
      req.on('error', reject);
      req.end(JSON.stringify(f.input()));
    });
    assert.equal(status, 201);
    assert.equal(f.backend.store.routines.length, 1);
  } finally { await f.close(); }
});

test('routine manual admission shares the HTTP command queue, cancels queued work, and dispatches after active cancellation', async () => {
  const f = await fixture();
  try {
    await f.configure();
    const blocker = await f.backend.store.addConversation(f.backend.store.bots[0].id);
    assert.equal((await f.api(`/api/conversations/${blocker.id}/messages`, { text: 'hold' })).status, 202);
    const first = await f.backend.store.addRoutine(f.input({ prompt: 'hold', name: 'First' }));
    const second = await f.backend.store.addRoutine(f.input({ name: 'Second' }));
    const results = await Promise.all([f.api(`/api/routines/${first.id}/run`, {}), f.api(`/api/routines/${first.id}/run`, {})]);
    assert.deepEqual(results.map(response => response.status).sort(), [202, 409]);
    const admitted = await results.find(response => response.status === 202)!.json() as RoutineRunResult;
    assert.equal(admitted.state.conversations.find(item => item.id === admitted.run.conversationId)!.run.status, 'idle');
    assert.equal((await f.api(`/api/routines/${first.id}`, f.input(), 'PUT')).status, 409);
    assert.equal((await f.api(`/api/routines/${first.id}`, {}, 'DELETE')).status, 409);
    assert.equal((await f.api(`/api/routines/${second.id}`, f.input({ name: 'Edited unrelated' }), 'PUT')).status, 200);
    const queuedResponse = await f.api(`/api/routines/${second.id}/run`, {});
    assert.equal(queuedResponse.status, 202);
    const queued = await queuedResponse.json() as RoutineRunResult;
    assert.equal((await f.api(`/api/conversations/${queued.run.conversationId}/cancel`, {})).status, 200);
    assert.equal(f.backend.store.conversation(queued.run.conversationId).run.status, 'cancelled');
    assert.equal((await f.api(`/api/routines/${first.id}`, { enabled: false }, 'PATCH')).status, 200);
    await f.api(`/api/conversations/${blocker.id}/cancel`, {});
    assert.equal((await f.api(`/api/conversations/${admitted.run.conversationId}/messages`, { text: 'Foreign ordinary message' })).status, 409);
    assert.equal(f.backend.store.conversation(admitted.run.conversationId).messages.length, 0);
    await f.backend.routines.tick();
    assert.ok(['starting', 'running'].includes(f.backend.store.conversation(admitted.run.conversationId).run.status));
    await f.api(`/api/conversations/${admitted.run.conversationId}/cancel`, {});
    assert.equal(f.backend.store.conversation(admitted.run.conversationId).run.status, 'cancelled');
    await f.backend.routines.tick();
    assert.equal(f.backend.store.routineRuns.length, 2);
    assert.equal(f.backend.store.conversation(queued.run.conversationId).messages.length, 0);
    assert.equal((await f.api(`/api/routines/${first.id}`, {}, 'DELETE')).status, 200);
  } finally { await f.close(); }
});

test('scheduled claims use the existing ACP runtime and finish asynchronously with conversation-derived status', async () => {
  const f = await fixture();
  try {
    await f.configure();
    const routine = await f.backend.store.addRoutine(f.input({ schedule: { kind: 'once', at: '2026-01-01T00:01:00Z' } }));
    f.setTime('2026-01-01T00:01:00Z');
    await f.backend.routines.tick();
    assert.equal(f.backend.store.routineRuns.length, 1);
    const run = f.backend.store.routineRuns[0];
    assert.equal(run.routineId, routine.id);
    await f.wait(() => f.backend.store.conversation(run.conversationId).run.status === 'completed');
    const conversation = f.backend.store.conversation(run.conversationId);
    assert.equal(conversation.messages[0].text, routine.prompt);
    assert.equal(conversation.messages[1].text, 'Routine completed locally.');
    const frozenResult = structuredClone(run.result);
    const firstExecution = run.executionRunId;
    assert.equal(frozenResult?.status, 'completed');
    assert.equal((await f.api(`/api/conversations/${conversation.id}/messages`, { text: 'hold' })).status, 202);
    assert.notEqual(conversation.run.id, firstExecution);
    assert.deepEqual(run.result, frozenResult);
    assert.equal((await f.api(`/api/routines/${routine.id}`, {}, 'DELETE')).status, 200);
    await f.api(`/api/conversations/${conversation.id}/cancel`, {});
    assert.deepEqual(run.result, frozenResult);
    await f.backend.routines.tick();
    assert.equal(f.backend.store.routineRuns.length, 1);
    const persisted = JSON.parse(await readFile(join(f.root, 'data', 'state.json'), 'utf8'));
    assert.equal(persisted.conversations.find((item: { id: string }) => item.id === run.conversationId).run.status, 'cancelled');
    assert.equal(persisted.routineRuns[0].result.status, 'completed');
  } finally { await f.close(); }
});

test('manual HTTP results and SSE redact routine names and prompts without losing persisted provider settings', async () => {
  const f = await fixture();
  try {
    const key = 'routine-api-private-key';
    await f.configure(key);
    const routine = await f.backend.store.addRoutine(f.input({ name: `Name ${key}`, prompt: `Prompt ${key}` }));
    const response = await f.api(`/api/routines/${routine.id}/run`, {});
    assert.equal(response.status, 202);
    const bytes = await response.text();
    assert.equal(bytes.includes(key), false);
    assert.equal(JSON.parse(bytes).run.prompt, 'Prompt [redacted]');
    const events = await f.api('/api/events');
    const reader = events.body!.getReader();
    const first = new TextDecoder().decode((await reader.read()).value);
    assert.equal(first.includes(key), false);
    await reader.cancel();
    await f.wait(() => !f.backend.store.active);
    const disk = JSON.parse(await readFile(join(f.root, 'data', 'state.json'), 'utf8'));
    assert.equal(disk.settings.apiKey, key);
    assert.equal(disk.routineRuns[0].prompt, `Prompt ${key}`);
  } finally { await f.close(); }
});

test('routine byte-capacity admission reserves prompt dispatch before committing manual work', async () => {
  for (const remaining of [150000, 330000]) {
    const f = await fixture();
    try {
      await f.configure();
      const manual = await f.backend.store.addRoutine(f.input({ prompt: 'm'.repeat(100000), name: 'Manual near byte limit' }));
      const due = await f.backend.store.addRoutine(f.input({ prompt: 'd'.repeat(100000), schedule: { kind: 'once', at: '2026-01-01T00:01:00Z' } }));
      const filler = await f.backend.store.addConversation(manual.botId);
      filler.messages.push({ id: randomUUID(), role: 'assistant', text: '', createdAt: '2026-01-01T00:00:00Z', tools: [] });
      await f.backend.store.changed();
      const stateFile = join(f.root, 'data', 'state.json');
      const size = Buffer.byteLength(await readFile(stateFile, 'utf8'));
      filler.messages[0].text = 'x'.repeat(64 * 1024 * 1024 - remaining - size);
      await f.backend.store.changed();
      f.setTime('2026-01-01T00:01:00Z');
      const response = await f.api(`/api/routines/${manual.id}/run`, {});
      if (remaining === 150000) {
        assert.equal(response.status, 413);
        assert.equal(f.backend.store.routineRuns.length, 0);
      } else {
        assert.equal(response.status, 202);
        await response.body?.cancel();
        assert.equal(f.backend.store.routineRuns.length, 1);
        const run = f.backend.store.routineRuns[0];
        await f.wait(() => f.backend.store.conversation(run.conversationId).run.status === 'completed');
        assert.equal(run.result?.status, 'completed');
        assert.match(f.backend.store.routineSchedulingError ?? '', /64 MiB/);
        assert.equal(f.backend.store.routine(due.id).nextRunAt, '2026-01-01T00:01:00.000Z');
        assert.equal(f.backend.routines.error, undefined);
      }
      f.backend.store.assertWritable();
      assert.equal(f.backend.store.runtime.available, true);
    } finally {
      await f.backend.close().catch(() => {});
      await rm(f.root, { recursive: true, force: true });
    }
  }
});

test('backend shutdown closes HTTP even after routine persistence failure', async () => {
  const f = await fixture();
  let failed = false;
  try {
    await f.backend.store.addRoutine(f.input({ schedule: { kind: 'once', at: '2026-01-01T00:01:00Z' } }));
    const stateFile = join(f.root, 'data', 'state.json');
    await rename(stateFile, stateFile + '.saved');
    await mkdir(stateFile);
    f.setTime('2026-01-01T00:01:00Z');
    failed = true;
    await assert.rejects(f.backend.routines.tick());
    await assert.rejects(f.backend.close());
    assert.equal(f.backend.server.listening, false);
    assert.ok(f.backend.routines.error);
    assert.equal(f.backend.store.conversations[0].run.status, 'idle');
  } finally {
    if (failed) {
      await f.backend.close().catch(() => {});
      await rm(f.root, { recursive: true, force: true });
    } else await f.close();
  }
});

test('scheduler starts at server listening and stops polling before backend shutdown', async () => {
  const f = await fixture({ autoStart: true, pollIntervalMs: 5 });
  try {
    await f.configure();
    await f.backend.store.addRoutine(f.input({ schedule: { kind: 'once', at: '2026-01-01T00:01:00Z' } }));
    f.setTime('2026-01-01T00:01:00Z');
    await f.wait(() => f.backend.store.routineRuns.length === 1);
    await f.wait(() => !f.backend.store.active);
    await f.backend.close();
    const count = f.backend.store.routineRuns.length;
    f.setTime('2027-01-01T00:00:00Z');
    await f.backend.routines.tick();
    assert.equal(f.backend.store.routineRuns.length, count);
  } finally { await f.close(); }
});
