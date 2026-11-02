import assert from 'node:assert/strict';
import test from 'node:test';
import { createHash, randomUUID } from 'node:crypto';
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { AddressInfo } from 'node:net';
import { createBackend } from '../server/app.js';
import { botDeliverySchema, botExchangeSchema, MAX_BOT_MESSAGE_BYTES } from '../server/bot-messages.js';
import { Store } from '../server/store.js';
import type { AppState, Message } from '../shared/types.js';
import { startMockProvider, type MockCompletion, type MockProviderRequest } from './mock-provider.js';
import { stateSummary } from './state-summary.js';

const use = (name: string, input: Record<string, unknown>): MockCompletion => ({
  tool: { name: 'use_tool', arguments: { tool_name: `workspace__${name}`, tool_input: input } },
});
const discover: MockCompletion = { tool: { name: 'search_tool', arguments: { query: 'workspace list_bots send_message', limit: 5 } } };

async function fixture() {
  const root = await mkdtemp(join(tmpdir(), 'grok-bot-messages-'));
  const workspace = join(root, 'workspace');
  await mkdir(workspace);
  await writeFile(join(workspace, 'notes.txt'), 'untouched\n');
  let plan: MockCompletion[] = [];
  let hold: Promise<MockCompletion> | undefined;
  let release: ((completion: MockCompletion) => void) | undefined;
  const inference: MockProviderRequest[] = [];
  const provider = await startMockProvider({ reply: request => {
    if (request.tools?.some(tool => tool.function.name === 'session_title')) return { tool: { name: 'session_title', arguments: { title: 'Bot message fixture' } } };
    inference.push(request);
    return plan.shift() ?? hold ?? { text: 'Local turn complete.' };
  } });
  const dataDir = join(root, 'data');
  const backend = await createBackend({ dataDir, serveWeb: false, routines: { autoStart: false }, runtime: { cancelDeadlineMs: 50 } });
  await new Promise<void>(resolve => backend.server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${(backend.server.address() as AddressInfo).port}`;
  const api = (route: string, body?: unknown, method = body === undefined ? 'GET' : 'POST') => fetch(origin + route, {
    method, headers: { Origin: origin, 'X-Grok-Bot': '1', 'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body),
  });
  const wait = async (predicate: () => boolean) => {
    const deadline = Date.now() + 20000;
    while (!predicate()) {
      if (Date.now() >= deadline) assert.fail(`Timed out. ${stateSummary(backend.store.snapshot())}`);
      await new Promise(resolve => setTimeout(resolve, 20));
    }
  };
  const sourceBot = backend.store.bots[0];
  await backend.store.updateBot(sourceBot.id, { name: 'Sender', instructions: 'SOURCE_IDENTITY_CANARY' });
  const recipient = await backend.store.addBot({ name: 'Recipient', instructions: 'RECIPIENT_IDENTITY_CANARY', color: '#3b82f6' });
  const configure = async (apiKey = '', files = false) => {
    assert.equal((await api('/api/settings', { baseUrl: provider.baseUrl, model: 'local-test-model', workspace: files ? workspace : '', apiKey }, 'PUT')).status, 200);
  };
  const source = await backend.store.addConversation(sourceBot.id);
  const send = (text = 'Send a local bot message.', id = source.id) => api(`/api/conversations/${id}/messages`, { text });
  const approve = async (id = source.id, allow = true) => {
    await wait(() => ['awaiting_permission', 'error'].includes(backend.store.conversation(id).run.status));
    const conversation = backend.store.conversation(id);
    assert.equal(conversation.run.status, 'awaiting_permission', JSON.stringify(conversation.run));
    const permission = structuredClone(conversation.run.permission!);
    assert.equal(permission.options.length, 2);
    assert.deepEqual(permission.options.map(option => option.kind), ['allow_once', 'reject_once']);
    const option = permission.options.find(item => item.kind === (allow ? 'allow_once' : 'reject_once'))!;
    assert.equal((await api(`/api/conversations/${id}/permissions/${permission.id}`, { optionId: option.id })).status, 200);
    return permission;
  };
  return {
    root, workspace, dataDir, provider, backend, origin, api, source, sourceBot, recipient, inference, wait, configure, send, approve,
    setPlan: (...completions: MockCompletion[]) => { plan = completions; },
    hold: () => { hold = new Promise(resolve => { release = resolve; }); },
    release: (text = 'Released.') => { release?.({ text }); hold = undefined; },
    complete: async (id = source.id) => {
      await wait(() => ['completed', 'cancelled', 'error'].includes(backend.store.conversation(id).run.status));
      // Terminal in-memory state precedes the final atomic write. Disk round-trip assertions
      // must wait for that existing durable boundary, rather than winning a race with the writer.
      await backend.store.flush();
    },
    close: async () => { release?.({ text: 'Fixture closing.' }); await backend.close(); await provider.close(); await rm(root, { recursive: true, force: true }); },
  };
}

test('successful bot waits do not build snapshot diagnostics', async (context) => {
  const f = await fixture();
  const snapshot = context.mock.method(f.backend.store, 'snapshot');
  try {
    let polls = 0;
    await f.wait(() => ++polls > 1);
    assert.equal(polls, 2);
    assert.equal(snapshot.mock.callCount(), 0);
  } finally { snapshot.mock.restore(); await f.close(); }
});

test('recipient completion waits for pending durable writes before a state round trip', { timeout: 60000 }, async () => {
  const f = await fixture();
  let releaseWrite: (() => void) | undefined;
  try {
    await f.configure();
    f.setPlan(discover, use('send_message', { toBotId: f.recipient.id, text: 'Pending durable reply.' }), { text: 'Queued.' });
    await f.send(); await f.approve(); await f.complete();
    const delivery = f.backend.store.botDeliveries[0];
    const beforeRecipient = f.inference.length;
    f.hold();
    await f.backend.routines.tick();
    await f.wait(() => f.inference.length > beforeRecipient);
    await f.backend.store.flush();

    // Hold the existing ordered writer tail, as a slow disk would, without delaying ACP text.
    const writer = f.backend.store as unknown as { writeTail: Promise<void> };
    const gate = new Promise<void>(resolve => { releaseWrite = resolve; });
    writer.writeTail = Promise.all([writer.writeTail, gate]).then(() => undefined);
    f.release('Recipient final answer.');
    await f.wait(() => f.backend.store.conversation(delivery.recipientConversationId).run.status === 'completed');
    assert.equal(f.backend.store.botExchanges[0].messages[1].text, 'Recipient final answer.');
    const pendingDisk = JSON.parse(await readFile(join(f.dataDir, 'state.json'), 'utf8'));
    assert.equal(pendingDisk.botExchanges[0].messages.length, 1);

    let settled = false;
    const complete = f.complete(delivery.recipientConversationId).then(() => { settled = true; });
    await new Promise<void>(resolve => setImmediate(resolve));
    assert.equal(settled, false, 'terminal memory state alone must not settle a disk round trip');
    releaseWrite!();
    await complete;
    const reopened = await Store.open(f.dataDir);
    assert.deepEqual(reopened.botExchanges, f.backend.store.botExchanges);
    assert.equal(reopened.botExchanges[0].messages[1].text, 'Recipient final answer.');
  } finally { releaseWrite?.(); await f.close(); }
});

test('real source engine discovers authenticated bot tools, approves a durable idempotent delivery, and projects one recipient answer', { timeout: 60000 }, async () => {
  const f = await fixture();
  try {
    assert.equal(f.backend.store.runtime.available, true, f.backend.store.runtime.error);
    await f.configure();
    const input = { toBotId: f.recipient.id, text: 'Review this plain chat message.', requestId: 'delivery-1' };
    f.setPlan(discover, use('list_bots', {}), use('send_message', input), use('send_message', input),
      use('send_message', { ...input, text: 'A second distinct message is forbidden.' }), { text: 'The message was queued.' });
    assert.equal((await f.send()).status, 202);
    await f.wait(() => f.source.run.status === 'awaiting_permission');
    assert.equal(f.backend.store.botDeliveries.length, 0);
    assert.equal(f.backend.store.botExchanges.length, 0);
    const pending = f.source.run.permission!;
    assert.match(pending.description, /Recipient/);
    assert.match(pending.description, /Review this plain chat message\./);
    assert.deepEqual(pending.paths, []);
    const runId = f.source.run.id;
    const unauthenticated = await fetch(`${f.origin}/internal/mcp/${runId}`, {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'tools/list' }),
    });
    assert.equal(unauthenticated.status, 403);
    const permission = await f.approve();
    await f.complete();
    assert.equal(f.source.run.status, 'completed');
    assert.equal(f.source.messages.at(-1)!.text, 'The message was queued.');
    assert.equal(f.backend.store.botDeliveries.length, 1);
    const delivery = f.backend.store.botDeliveries[0];
    const exchange = f.backend.store.botExchanges[0];
    assert.equal(delivery.status, 'queued');
    assert.equal(delivery.sourceMessageId, f.source.messages.at(-1)!.id);
    assert.deepEqual(f.source.messages.at(-1)!.botExchangeIds, [exchange.id]);
    assert.deepEqual(exchange.botIds, [f.sourceBot.id, f.recipient.id]);
    assert.equal(exchange.messages[0].text, input.text);
    const disk = JSON.parse(await readFile(join(f.dataDir, 'state.json'), 'utf8'));
    assert.equal(disk.botDeliveries[0].id, delivery.id);
    assert.equal(disk.botDeliveries[0].status, 'queued');
    const outcomes = f.inference.flatMap(request => request.messages.filter(message => message.role === 'tool').map(message => String(message.content)));
    assert.ok(outcomes.some(text => text.includes(f.recipient.id) && text.includes('Recipient')));
    assert.ok(outcomes.filter(text => text.includes(delivery.id)).length >= 2);
    assert.ok(outcomes.some(text => text.includes('one distinct bot message')));
    assert.equal((await f.api(`/api/conversations/${f.source.id}/permissions/${permission.id}`, { optionId: permission.options[0].id })).status, 409);
    assert.equal((await f.api(`/api/conversations/${delivery.recipientConversationId}/messages`, { text: 'Hijacked prompt' })).status, 409);
    const beforeRecipient = f.inference.length;
    f.setPlan({ text: 'Recipient final answer.' });
    await f.backend.routines.tick();
    await f.complete(delivery.recipientConversationId);
    assert.equal(delivery.status, 'completed');
    const recipient = f.backend.store.conversation(delivery.recipientConversationId);
    assert.equal(recipient.botId, f.recipient.id);
    assert.equal(recipient.messages[0].sourceBotId, f.sourceBot.id);
    assert.equal(recipient.messages[0].text, input.text);
    assert.equal(exchange.messages.length, 2);
    assert.equal(exchange.messages[1].botId, f.recipient.id);
    assert.equal(exchange.messages[1].text, 'Recipient final answer.');
    const recipientRequests = JSON.stringify(f.inference.slice(beforeRecipient));
    assert.match(recipientRequests, /RECIPIENT_IDENTITY_CANARY/);
    assert.doesNotMatch(recipientRequests, /SOURCE_IDENTITY_CANARY/);
    await f.backend.routines.tick();
    await f.backend.routines.tick();
    assert.equal(f.backend.store.botDeliveries.length, 1);
    assert.equal(exchange.messages.length, 2);
    assert.equal(f.source.messages.length, 2);
    assert.equal((await f.api(`/api/conversations/${recipient.id}/messages`, { text: 'Hijacked follow-up' })).status, 409);
    for (const request of f.inference) assert.deepEqual(request.tools!.map(tool => tool.function.name).sort(), ['search_tool', 'todo_write', 'use_tool']);
    const fingerprint = createHash('sha256').update(JSON.stringify([f.backend.store.settings, f.sourceBot.instructions, 'broker-v2', '2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8'])).digest('hex');
    assert.notEqual(f.source.sessionFingerprint, fingerprint);
    const reopened = await Store.open(f.dataDir);
    assert.deepEqual(reopened.botExchanges, f.backend.store.botExchanges);
    assert.deepEqual(reopened.botDeliveries, f.backend.store.botDeliveries);
  } finally { await f.close(); }
});

test('bot delivery denial and cancellation consume no approval and recipient runs cannot access file tools', { timeout: 60000 }, async () => {
  const f = await fixture();
  try {
    await f.configure('', true);
    const input = { toBotId: f.recipient.id, text: 'A message requiring explicit approval.' };
    f.setPlan(discover, use('send_message', input), { text: 'The message was denied.' });
    assert.equal((await f.send()).status, 202);
    await f.approve(f.source.id, false);
    await f.complete();
    assert.equal(f.backend.store.botDeliveries.length, 0);
    f.setPlan(use('send_message', input));
    await f.send('Cancel the next proposed message.');
    await f.wait(() => f.source.run.status === 'awaiting_permission');
    const stale = structuredClone(f.source.run.permission!);
    assert.equal((await f.api(`/api/conversations/${f.source.id}/cancel`, {})).status, 200);
    assert.equal(f.source.run.status, 'cancelled');
    assert.equal(f.backend.store.botDeliveries.length, 0);
    assert.equal((await f.api(`/api/conversations/${f.source.id}/permissions/${stale.id}`, { optionId: stale.options[0].id })).status, 409);
    f.setPlan(use('send_message', input), { text: 'Queued.' });
    await f.send('Approve the next message.');
    await f.approve();
    await f.complete();
    const delivery = f.backend.store.botDeliveries[0];
    const beforeRecipient = f.inference.length;
    f.setPlan(discover, use('read_file', { path: 'notes.txt' }), use('write_file', { path: 'notes.txt', content: 'must not be saved' }), { text: 'Chat-only reply.' });
    await f.backend.routines.tick();
    await f.complete(delivery.recipientConversationId);
    assert.equal(delivery.status, 'completed');
    assert.equal(await readFile(join(f.workspace, 'notes.txt'), 'utf8'), 'untouched\n');
    const outcomes = f.inference.slice(beforeRecipient).flatMap(request => request.messages.filter(message => message.role === 'tool').map(message => String(message.content)));
    assert.ok(outcomes.some(text => text.includes('chat-only')));
    assert.equal(f.backend.store.conversation(delivery.recipientConversationId).run.permission, undefined);
  } finally { await f.close(); }
});

test('routines and bot messages share FIFO admission, queued cancellation, and one active recipient run', { timeout: 60000 }, async () => {
  const f = await fixture();
  try {
    await f.configure();
    f.setPlan(discover, use('send_message', { toBotId: f.recipient.id, text: 'Queued behind a routine.' }), { text: 'Queued.' });
    await f.send();
    await f.wait(() => f.source.run.status === 'awaiting_permission');
    const routine = await f.backend.store.addRoutine({ botId: f.sourceBot.id, name: 'First routine', prompt: 'Routine gets the first turn.', schedule: { kind: 'interval', minutes: 1 }, enabled: false });
    const routineRun = await f.backend.routines.run(routine.id);
    assert.equal(f.backend.store.conversation(routineRun.conversationId).run.status, 'idle');
    await f.approve();
    await f.complete();
    const delivery = f.backend.store.botDeliveries[0];
    f.hold();
    await f.backend.routines.tick();
    await f.wait(() => f.backend.store.conversation(routineRun.conversationId).run.status === 'running');
    assert.equal(delivery.status, 'queued');
    await f.backend.routines.tick();
    assert.equal(f.backend.store.conversation(delivery.recipientConversationId).run.status, 'idle');
    assert.equal((await f.api(`/api/conversations/${delivery.recipientConversationId}/messages`, { text: 'Do not replace queued work.' })).status, 409);
    assert.equal((await f.api(`/api/conversations/${delivery.recipientConversationId}/cancel`, {})).status, 200);
    assert.equal(delivery.status, 'cancelled');
    assert.ok(delivery.finishedAt);
    assert.equal(f.backend.store.conversation(delivery.recipientConversationId).messages.length, 0);
    assert.equal((await f.api(`/api/conversations/${routineRun.conversationId}/cancel`, {})).status, 200);
    f.release();
    assert.equal(f.backend.store.conversation(routineRun.conversationId).run.status, 'cancelled');
    const reopened = await Store.open(f.dataDir);
    assert.equal(reopened.botDeliveries[0].status, 'cancelled');
    await f.backend.routines.tick();
    assert.equal(delivery.status, 'cancelled');
  } finally { await f.close(); }
});

test('bot chains require fresh approval at each hop and stop after four deliveries without automatic replies', { timeout: 90000 }, async () => {
  const f = await fixture();
  try {
    await f.configure();
    let current = f.source.id;
    let toBotId = f.recipient.id;
    let chainId: string | undefined;
    for (let hop = 1; hop <= 4; hop++) {
      f.setPlan(discover, use('send_message', { toBotId, text: `Explicit hop ${hop}` }), { text: `Reply at hop ${hop}` });
      if (hop === 1) assert.equal((await f.send('Start an explicitly approved chain.')).status, 202);
      else await f.backend.routines.tick();
      await f.approve(current);
      await f.complete(current);
      assert.equal(f.backend.store.botDeliveries.length, hop);
      const delivery = f.backend.store.botDeliveries.at(-1)!;
      chainId ??= delivery.chainId;
      assert.equal(delivery.chainId, chainId);
      assert.equal(delivery.hop, hop);
      assert.equal(delivery.status, 'queued');
      current = delivery.recipientConversationId;
      toBotId = delivery.fromBotId;
    }
    const before = f.inference.length;
    f.setPlan(discover, use('send_message', { toBotId, text: 'A fifth hop is forbidden.' }), { text: 'Chain ended.' });
    await f.backend.routines.tick();
    await f.complete(current);
    assert.equal(f.backend.store.botDeliveries.length, 4);
    assert.ok(f.backend.store.botDeliveries.every(delivery => delivery.status === 'completed'));
    assert.ok(f.backend.store.botExchanges.every(exchange => exchange.messages.length === 2));
    const outcomes = f.inference.slice(before).flatMap(request => request.messages.filter(message => message.role === 'tool').map(message => String(message.content)));
    assert.ok(outcomes.some(text => text.includes('four hops')));
    await f.backend.routines.tick();
    assert.equal(f.backend.store.active, undefined);
    const reopened = await Store.open(f.dataDir);
    assert.equal(reopened.botDeliveries.length, 4);
  } finally { await f.close(); }
});

test('bot messages migrate missing arrays, reject invalid persisted links, and retain queued or interrupted deliveries without replay', { timeout: 60000 }, async () => {
  const f = await fixture();
  try {
    await f.configure();
    const stateFile = join(f.dataDir, 'state.json');
    const original = JSON.parse(await readFile(stateFile, 'utf8'));
    delete original.botExchanges;
    delete original.botDeliveries;
    await writeFile(stateFile, JSON.stringify(original));
    const migrated = await Store.open(f.dataDir);
    assert.deepEqual(migrated.snapshot().botExchanges, []);
    assert.deepEqual(migrated.snapshot().botDeliveries, []);
    f.setPlan(discover, use('send_message', { toBotId: f.recipient.id, text: 'Survive a restart.' }), { text: 'Queued.' });
    await f.send();
    await f.approve();
    await f.complete();
    const queued = await Store.open(f.dataDir);
    assert.equal(queued.botDeliveries[0].status, 'queued');
    const disk = JSON.parse(await readFile(stateFile, 'utf8'));
    const corruptions = [
      (state: typeof disk) => { state.botDeliveries[0].extra = true; },
      (state: typeof disk) => { state.botDeliveries[0].hop = 5; },
      (state: typeof disk) => { state.botDeliveries[0].chainId = randomUUID(); },
      (state: typeof disk) => { state.botDeliveries[0].toBotId = state.botDeliveries[0].fromBotId; },
      (state: typeof disk) => { state.botDeliveries[0].sourceMessageId = randomUUID(); },
      (state: typeof disk) => { state.botExchanges[0].messages[0].text = 'Not the approved message'; },
      (state: typeof disk) => { state.botExchanges[0].botIds.reverse(); },
      (state: typeof disk) => { state.botDeliveries[0].status = 'completed'; },
    ];
    for (const corrupt of corruptions) {
      const state = structuredClone(disk);
      corrupt(state);
      const bytes = JSON.stringify(state);
      await writeFile(stateFile, bytes);
      await assert.rejects(Store.open(f.dataDir), /existing state.json was not replaced/);
      assert.equal(await readFile(stateFile, 'utf8'), bytes);
    }
    await writeFile(stateFile, JSON.stringify(disk));
    f.hold();
    await f.backend.routines.tick();
    const delivery = f.backend.store.botDeliveries[0];
    await f.wait(() => delivery.status === 'running');
    const interrupted = await Store.open(f.dataDir);
    assert.equal(interrupted.botDeliveries[0].status, 'interrupted');
    assert.ok(interrupted.botDeliveries[0].finishedAt);
    assert.equal(interrupted.botExchanges[0].messages.length, 1);
    assert.equal(interrupted.conversation(delivery.recipientConversationId).run.permission, undefined);
    const reopened = await Store.open(f.dataDir);
    assert.equal(reopened.botDeliveries[0].status, 'interrupted');
    f.release();
    await f.complete(delivery.recipientConversationId);
  } finally { await f.close(); }
});

test('bot message snapshots and SSE redact credentials while preserving durable approved text', { timeout: 60000 }, async () => {
  const f = await fixture();
  try {
    const key = 'bot-delivery-private-provider-key';
    await f.configure(key);
    f.setPlan(discover, use('send_message', { toBotId: f.recipient.id, text: `Approved ${key}` }), { text: `Queued ${key}` });
    await f.send();
    await f.wait(() => f.source.run.status === 'awaiting_permission');
    assert.equal(JSON.stringify(f.backend.store.snapshot()).includes(key), false);
    await f.approve();
    await f.complete();
    const state = await (await f.api('/api/state')).json() as AppState;
    assert.equal(state.botExchanges![0].messages[0].text, 'Approved [redacted]');
    assert.equal(state.botDeliveries![0].text, 'Approved [redacted]');
    const events = await f.api('/api/events');
    const reader = events.body!.getReader();
    assert.equal(new TextDecoder().decode((await reader.read()).value).includes(key), false);
    await reader.cancel();
    const disk = JSON.parse(await readFile(join(f.dataDir, 'state.json'), 'utf8'));
    assert.equal(disk.settings.apiKey, key);
    assert.equal(disk.botDeliveries[0].text, `Approved ${key}`);
    assert.equal(disk.botExchanges[0].messages[0].text, `Approved ${key}`);
    f.setPlan({ text: `Recipient answered ${key}` });
    await f.backend.routines.tick();
    await f.complete(state.botDeliveries![0].recipientConversationId);
    assert.equal(f.backend.store.botExchanges[0].messages[1].text, 'Recipient answered [redacted]');
    assert.equal(JSON.stringify(f.backend.store.snapshot()).includes(key), false);
  } finally { await f.close(); }
});

test('bot admission validates complete reviewable messages and reserves capacity before mutating durable work', async () => {
  const root = await mkdtemp(join(tmpdir(), 'grok-bot-message-capacity-'));
  try {
    const store = await Store.open(root);
    const sender = store.bots[0];
    const recipient = await store.addBot({ name: 'Receiver', color: '#3b82f6', instructions: '' });
    await store.updateSettings({ baseUrl: 'http://127.0.0.1:12345/v1', model: 'fixture', workspace: '', apiKey: '-' });
    const source = await store.addConversation(sender.id);
    const now = new Date().toISOString();
    const message: Message = { id: randomUUID(), role: 'assistant', text: '', createdAt: now, tools: [] };
    source.messages.push(message);
    source.run = { id: randomUUID(), status: 'running', startedAt: now };
    await store.changed();
    const queue = (input: unknown) => store.queueBotMessage(source.id, source.run.id, message.id, input as Parameters<Store['queueBotMessage']>[3]);
    for (const input of [
      { toBotId: sender.id, text: 'self' }, { toBotId: randomUUID(), text: 'missing' },
      { toBotId: recipient.id, text: '' }, { toBotId: recipient.id, text: ' ', extra: true },
      { toBotId: recipient.id, text: 'x'.repeat(100001) }, { toBotId: recipient.id, text: '😀'.repeat(50000) },
    ]) await assert.rejects(queue(input));
    assert.equal(Buffer.byteLength('😀'.repeat(50000)), 200000);
    assert.ok(200000 > MAX_BOT_MESSAGE_BYTES);
    assert.equal(store.botDeliveries.length, 0);
    const file = join(root, 'state.json');
    const bytes = Buffer.byteLength(await readFile(file, 'utf8'));
    message.text = 'x'.repeat(64 * 1024 * 1024 - 500000 - bytes);
    await store.changed();
    const before = store.snapshot().revision;
    await assert.rejects(queue({ toBotId: recipient.id, text: 'Do not commit over capacity.' }), /64 MiB/);
    assert.equal(store.snapshot().revision, before);
    assert.equal(store.botExchanges.length, 0);
    assert.equal(store.botDeliveries.length, 0);
    assert.equal(store.conversations.length, 1);
    assert.equal(message.botExchangeIds, undefined);
    store.assertWritable();
    message.text = '';
    await store.changed();
    const delivery = await queue({ toBotId: recipient.id, text: 'Fits after capacity is available.' });
    assert.equal(botDeliverySchema.safeParse(delivery).success, true);
    assert.equal(botExchangeSchema.safeParse(store.botExchanges[0]).success, true);
    const snapshot = store.snapshot();
    assert.deepEqual(snapshot.botExchanges![0].botIds, [sender.id, recipient.id]);
    assert.deepEqual(snapshot.conversations[0].messages[0].botExchangeIds, [delivery.exchangeId]);
    assert.equal(snapshot.botDeliveries![0].sourceMessageId, message.id);
    assert.equal(snapshot.botDeliveries![0].recipientConversationId, delivery.recipientConversationId);
    assert.equal(snapshot.botDeliveries![0].chainId, delivery.id);
    assert.equal((await queue({ toBotId: recipient.id, text: delivery.text })).id, delivery.id);
    await assert.rejects(queue({ toBotId: recipient.id, text: 'Second distinct message.' }), /one distinct/);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('cancellation releases the command queue before waiting for an approved send to finish', { timeout: 30000 }, async t => {
  const f = await fixture();
  try {
    await f.configure();
    f.setPlan(discover, use('send_message', { toBotId: f.recipient.id, text: 'Cancel before admission can commit.' }), { text: 'Cancelled.' });
    await f.send();
    await f.wait(() => f.source.run.status === 'awaiting_permission');
    const permission = structuredClone(f.source.run.permission!);
    const resolve = f.backend.runtime.resolve.bind(f.backend.runtime);
    let enter!: () => void;
    const entered = new Promise<void>(done => { enter = done; });
    let release!: () => void;
    const gate = new Promise<void>(done => { release = done; });
    t.mock.method(f.backend.runtime, 'resolve', async (...args: Parameters<typeof resolve>) => {
      enter();
      await gate;
      return resolve(...args);
    });
    const approving = f.api(`/api/conversations/${f.source.id}/permissions/${permission.id}`, { optionId: permission.options[0].id });
    await entered;
    let cancelled!: () => void;
    const cancellationEntered = new Promise<void>(done => { cancelled = done; });
    const cancel = f.backend.runtime.cancel.bind(f.backend.runtime);
    t.mock.method(f.backend.runtime, 'cancel', (...args: Parameters<typeof cancel>) => {
      cancelled();
      return cancel(...args);
    });
    const cancellationRequested = new Promise<void>(done => {
      f.backend.server.once('request', request => {
        assert.equal(request.url, `/api/conversations/${f.source.id}/cancel`);
        done();
      });
    });
    const cancelling = f.api(`/api/conversations/${f.source.id}/cancel`, {});
    await cancellationRequested;
    release();
    assert.equal((await approving).status, 200);
    await cancellationEntered;
    assert.equal((await cancelling).status, 200);
    assert.equal(f.source.run.status, 'cancelled');
    assert.equal(f.backend.store.botDeliveries.length, 0);
    assert.equal(f.backend.store.botExchanges.length, 0);
  } finally {
    t.mock.restoreAll();
    await f.close();
  }
});

test('recipient startup failures and active cancellation are durable terminal results with no replay or projected partial answer', { timeout: 60000 }, async () => {
  const f = await fixture();
  try {
    await f.configure();
    const input = { toBotId: f.recipient.id, text: 'A durable queued message.' };
    f.setPlan(discover, use('send_message', input), { text: 'Queued.' });
    await f.send();
    await f.approve();
    await f.complete();
    const failed = f.backend.store.botDeliveries[0];
    const runtime = f.backend.store.runtime;
    f.backend.store.runtime = { available: false, error: 'Fixture engine unavailable.' };
    await f.backend.routines.tick();
    assert.equal(failed.status, 'error');
    assert.equal(failed.error, 'Fixture engine unavailable.');
    assert.ok(failed.finishedAt);
    assert.equal(f.backend.store.conversation(failed.recipientConversationId).messages.length, 0);
    assert.equal(f.backend.store.botExchanges[0].messages.length, 1);
    f.backend.store.runtime = runtime;
    f.setPlan(use('send_message', input), { text: 'Queued again from a new run.' });
    await f.send('Create a second independently approved delivery.');
    await f.approve();
    await f.complete();
    const active = f.backend.store.botDeliveries[1];
    f.hold();
    await f.backend.routines.tick();
    await f.wait(() => active.status === 'running');
    assert.equal((await f.api(`/api/conversations/${active.recipientConversationId}/cancel`, {})).status, 200);
    f.release();
    assert.equal(active.status, 'cancelled');
    assert.ok(active.startedAt);
    assert.ok(active.finishedAt);
    assert.equal(f.backend.store.botExchanges[1].messages.length, 1);
    const reopened = await Store.open(f.dataDir);
    assert.deepEqual(reopened.botDeliveries.map(delivery => delivery.status), ['error', 'cancelled']);
    await f.backend.routines.tick();
    assert.equal(f.backend.store.active, undefined);
  } finally { await f.close(); }
});
