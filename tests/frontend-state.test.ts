import test from 'node:test';
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { reducer, emptyFiles, draftKey, readRoute, routeSearch, pendingBotWork, ownConversationsFor, actualBotId, hasConversationWork, isBotRunning, queuedWorkForBot, queuedWorkForConversation, runningConversation, runningConversations, runningConversationsForBot, type UIState } from '../web/state.js';
import type { AppState, Conversation } from '../shared/types.js';
import { cancelQueuedTurn } from '../web/api.js';

function snapshot(revision: number, status: 'running' | 'completed'): AppState {
  const botId = 'a620c2e4-25b6-410b-9d32-20bf4e67753a';
  const conversationId = '42327d73-b322-488c-9cab-09598be6e0ef';
  return {
    revision,
    routines: [],
    routineRuns: [],
    bots: [{ id: botId, name: 'Grok', color: '#75a9c0', instructions: '', createdAt: '2026-10-31T00:00:00Z' }],
    conversations: [{ id: conversationId, botId, title: 'Conversation', updatedAt: '2026-10-31T00:00:00Z', messages: [{ id: randomUUID(), role: 'assistant', text: status === 'completed' ? 'Final answer' : '', tools: [], createdAt: '2026-10-31T00:00:00Z' }], run: { id: 'run', status } }],
    settings: { baseUrl: 'http://127.0.0.1:8000/v1', model: 'test', hasApiKey: false, workspace: '/tmp/test-workspace' },
    runtime: { available: true, version: 'grok-bot-engine 0.1.0 (2bdd1d6a6369)' },
  };
}

function ui(): UIState {
  return {
    data: null, connection: 'loading', connectionError: null, surface: { kind: 'chat' },
    selection: { botId: null, conversationId: null }, lastConversation: {}, drafts: {}, overlay: null,
    sidebarOpen: false, viewport: 'desktop', computerOpen: false, command: null, error: null, search: '', files: emptyFiles(),
  };
}

test('a delayed HTTP response cannot roll a completed SSE snapshot back to running', () => {
  const completed = reducer(ui(), { type: 'snapshot', data: snapshot(20, 'completed') });
  const delayed = reducer(completed, { type: 'snapshot', data: snapshot(19, 'running') });
  assert.equal(delayed, completed);
  assert.equal(delayed.data?.conversations[0].run.status, 'completed');
  assert.equal(delayed.data?.conversations[0].messages[0].text, 'Final answer');
  const newer = reducer(delayed, { type: 'snapshot', data: snapshot(21, 'running') });
  assert.equal(newer.data?.conversations[0].run.status, 'running');
});

test('an explicit new-chat draft stays selected across routines, new background conversations, and route restoration', () => {
  const data = snapshot(1, 'completed');
  const botId = data.bots[0].id;
  let state = reducer(ui(), { type: 'snapshot', data });
  const selection = { botId, conversationId: null };
  state = reducer(state, { type: 'select', selection });
  assert.deepEqual(state.selection, selection);
  assert.equal(state.lastConversation[botId], null);
  state = reducer(state, { type: 'draft', key: draftKey(selection), text: 'Unsent new conversation' });
  const route = { selection, surface: { kind: 'routines' as const } };
  state = reducer(state, { type: 'route', route });
  assert.deepEqual(state.selection, selection);
  const updated = structuredClone(data);
  updated.revision++;
  updated.conversations.push({ ...updated.conversations[0], id: randomUUID(), updatedAt: '2026-10-31T00:01:00Z' });
  state = reducer(state, { type: 'snapshot', data: updated });
  const restored = readRoute(routeSearch(route));
  state = reducer(state, { type: 'route', route: restored });
  state = reducer(state, { type: 'select', selection: state.selection });
  assert.deepEqual(state.selection, selection);
  assert.equal(state.drafts[draftKey(state.selection)], 'Unsent new conversation');
  assert.equal(state.surface.kind, 'chat');
  assert.deepEqual(reducer({ ...ui(), ...readRoute(routeSearch({ selection, surface: { kind: 'chat' } })) }, { type: 'snapshot', data: updated }).selection, selection);
});

test('chat, routine, and bot-pair routes round-trip through query parameters without fragments', () => {
  const selection = { botId: 'bot-id', conversationId: 'conversation-id' };
  for (const kind of ['chat', 'routines'] as const) {
    const route = { selection, surface: { kind } };
    const search = routeSearch(route);
    assert.ok(search.startsWith('?'));
    assert.equal(search.includes('#'), false);
    assert.deepEqual(readRoute(search), route);
  }
  const pair = { selection, surface: { kind: 'bot-chat' as const, exchangeId: 'exchange-id' } };
  assert.deepEqual(readRoute(routeSearch(pair)), pair);
  assert.equal(routeSearch(pair).includes('#'), false);
  assert.equal(routeSearch({ selection: { botId: null, conversationId: null }, surface: { kind: 'chat' } }), '');
  assert.deepEqual(readRoute('?bot=bot-id'), { selection: { botId: 'bot-id', conversationId: null }, surface: { kind: 'chat' } });
});

test('external document refresh updates clean editors and preserves unsaved edits with a warning', () => {
  const state = ui();
  state.files.editor = { kind: 'ready', document: { path: 'notes.txt', content: 'before', hash: 'old-hash' }, content: 'before', save: 'idle' };
  const document = { path: 'notes.txt', content: 'after AI write', hash: 'new-hash' };
  const clean = reducer(state, { type: 'document-revalidated', document, previousHash: 'old-hash' });
  assert.equal(clean.files.editor.kind, 'ready');
  if (clean.files.editor.kind === 'ready') assert.equal(clean.files.editor.content, 'after AI write');
  const edited = reducer(state, { type: 'edit-file', content: 'unsaved user draft' });
  const dirty = reducer(edited, { type: 'document-revalidated', document, previousHash: 'old-hash' });
  assert.equal(dirty.files.editor.kind, 'ready');
  if (dirty.files.editor.kind === 'ready') {
    assert.equal(dirty.files.editor.content, 'unsaved user draft');
    assert.match(dirty.files.editor.error ?? '', /외부 작업/);
    assert.equal(dirty.files.editor.document.hash, 'old-hash');
  }
});

test('pendingBotWork describes related delivery/report waiting without defining conversation Stop scope', () => {
  const data = snapshot(1, 'completed');
  data.runtime.botMessageChains = true;
  const conversation = data.conversations[0];
  const delivery: NonNullable<AppState['botDeliveries']>[number] = {
    id: 'delivery', exchangeId: 'exchange', chainId: 'delivery', hop: 1,
    sourceConversationId: conversation.id, sourceMessageId: conversation.messages[0].id,
    recipientConversationId: 'recipient-conversation', fromBotId: conversation.botId, toBotId: 'recipient-bot',
    text: 'A public fixture review request.', createdAt: '2026-10-31T00:00:00Z', status: 'queued',
  };
  assert.equal(pendingBotWork(null, conversation), false);
  assert.equal(pendingBotWork(data, undefined), false);
  assert.equal(pendingBotWork(data, conversation), false);
  for (const status of ['queued', 'running', 'waiting_children'] as const) {
    data.botDeliveries = [{ ...delivery, status }];
    assert.equal(pendingBotWork(data, conversation), true, `source sees ${status}`);
    data.botDeliveries = [{ ...delivery, status, sourceConversationId: 'other-source', recipientConversationId: conversation.id }];
    assert.equal(pendingBotWork(data, conversation), true, `recipient sees ${status}`);
    data.botDeliveries = [{ ...delivery, status, sourceConversationId: 'other-source' }];
    assert.equal(pendingBotWork(data, conversation), false, `same bot but unrelated conversation does not see ${status}`);
  }
  for (const status of ['completed', 'cancelled', 'error', 'interrupted'] as const) {
    data.botDeliveries = [{ ...delivery, status }];
    assert.equal(pendingBotWork(data, conversation), false, `${status} has no pending work`);
  }
  for (const status of ['queued', 'running'] as const) {
    data.botDeliveries = [{ ...delivery, status: 'completed', report: { status } }];
    assert.equal(pendingBotWork(data, conversation), true, `source sees a ${status} report after delivery completion`);
    data.botDeliveries[0].sourceConversationId = 'other-source';
    data.botDeliveries[0].recipientConversationId = conversation.id;
    assert.equal(pendingBotWork(data, conversation), false, 'a report belongs to its source, not its recipient');
  }
  for (const status of ['completed', 'cancelled', 'error', 'interrupted', 'skipped'] as const) {
    data.botDeliveries = [{ ...delivery, status: 'completed', report: { status } }];
    assert.equal(pendingBotWork(data, conversation), false, `${status} report never leaves a false Stop button`);
  }
  data.botDeliveries = [{ ...delivery, status: 'running' }];
  delete data.runtime.botMessageChains;
  assert.equal(pendingBotWork(data, conversation), false, 'legacy Node backend must not expose unsupported chain Stop');
});

test('opening a bot prefers its own newest conversation over a newer one opened by another bot', () => {
  const data = snapshot(1, 'completed');
  const own = data.conversations[0];
  const recipient = { ...own, id: 'recipient-conversation', title: '1+1 은 ?', updatedAt: '2026-11-01T00:00:00Z' };
  data.conversations.push(recipient);
  data.botDeliveries = [{
    id: 'delivery', exchangeId: 'exchange', chainId: 'delivery', hop: 1,
    sourceConversationId: 'other-source', sourceMessageId: 'other-message',
    recipientConversationId: recipient.id, fromBotId: 'other-bot', toBotId: own.botId,
    text: 'A public fixture question.', createdAt: '2026-11-01T00:00:00Z', status: 'completed',
  }];
  assert.deepEqual(ownConversationsFor(data, own.botId).map(item => item.id), [own.id]);
  // No remembered conversation: selecting the bot lands on its own conversation, not the newer recipient one.
  const state = reducer(reducer(ui(), { type: 'snapshot', data }), { type: 'select', selection: { botId: own.botId, conversationId: own.id } });
  assert.equal(state.selection.conversationId, own.id);
  const fresh = reducer(ui(), { type: 'snapshot', data });
  assert.equal(fresh.selection.conversationId, own.id);
  // A bot with only recipient conversations opens a new chat.
  data.conversations = [recipient];
  assert.deepEqual(ownConversationsFor(data, own.botId), []);
  assert.equal(reducer(ui(), { type: 'snapshot', data }).selection.conversationId, null);
});

const conversation = (id: string, botId: string, status: Conversation['run']['status']): Conversation => ({
  id, botId, title: id, messages: [], run: { id: `run:${id}`, status }, updatedAt: '2026-11-03T00:00:00Z',
});

test('plural live selectors keep aggregate guards and attribute group/thread work to the actual Bot', () => {
  const data = snapshot(1, 'completed');
  const first = conversation('first', 'a', 'running');
  const approval = conversation('approval', 'b', 'awaiting_permission');
  const group: Conversation = { ...conversation('group-conversation', 'group-id', 'starting'), groupId: 'group-id', run: { id: 'group-run', botId: 'c', status: 'starting' } };
  group.messages.push({ id: 'prior-member', role: 'assistant', speakerBotId: 'a', text: '', createdAt: '', tools: [] });
  const thread = { ...conversation('thread', 'a', 'running'), thread: { parentConversationId: 'first', parentMessageId: 'message' } };
  data.conversations = [conversation('completed', 'd', 'completed'), first, approval, group, thread];
  assert.deepEqual(runningConversations(data).map(item => item.id), ['first', 'approval', 'group-conversation', 'thread']);
  assert.equal(runningConversation(data), first, 'the legacy shared-resource guard still sees any active Bot');
  assert.equal(actualBotId(group), 'c', 'explicit owner wins over prior group speaker and group ID');
  assert.deepEqual(runningConversationsForBot(data, 'a').map(item => item.id), ['first', 'thread']);
  assert.equal(isBotRunning(data, 'b'), true, 'waiting for permission still occupies its own Bot');
  assert.equal(isBotRunning(data, 'c'), true);
  assert.equal(isBotRunning(data, 'group-id'), false);
  assert.equal(isBotRunning(data, 'd'), false);
  delete group.run.botId;
  assert.equal(actualBotId(group), 'a', 'legacy group snapshots use the last assistant speaker');
  group.messages = [];
  assert.equal(actualBotId(group), 'group-id', 'missing ownership metadata is tolerated');
  assert.deepEqual(runningConversations(null), []);
  assert.deepEqual(runningConversationsForBot(undefined, 'a'), []);
  assert.equal(runningConversation(null), undefined);
  assert.equal(isBotRunning(data, null), false);
});

test('queue selectors preserve accepted FIFO and canonical next-speaker ownership without inferring group members', () => {
  const data = snapshot(1, 'running');
  data.workQueue = [
    { id: 'one', botId: 'a', conversationId: 'main', createdAt: '2', kind: 'user', text: 'First', attachments: [] },
    { id: 'two', botId: 'b', conversationId: 'group', createdAt: '1', kind: 'user', text: '@B', attachments: [], speakerBotIds: ['b', 'c'] },
    { id: 'three', botId: 'a', conversationId: 'main', createdAt: '3', kind: 'report', deliveryId: 'delivery' },
    { id: 'four', botId: 'c', conversationId: 'group', createdAt: '4', kind: 'group', roundId: 'round' },
  ];
  const before = structuredClone(data.workQueue);
  assert.deepEqual(queuedWorkForConversation(data, 'main').map(work => work.id), ['one', 'three']);
  assert.deepEqual(queuedWorkForBot(data, 'b').map(work => work.id), ['two']);
  assert.deepEqual(queuedWorkForBot(data, 'c').map(work => work.id), ['four'], 'future frozen speaker is not the current waiting lane');
  assert.deepEqual(data.workQueue, before, 'selectors neither sort nor rewrite frozen speakers');
  assert.deepEqual(queuedWorkForBot(data, null), []);
  delete data.workQueue;
  assert.deepEqual(queuedWorkForConversation(data, 'main'), []);
  assert.deepEqual(queuedWorkForBot(null, 'a'), []);
});

test('conversation Stop never follows another Bot or descendant while queued-only own work is stoppable', () => {
  const data = snapshot(1, 'completed');
  const source = data.conversations[0];
  data.runtime.botMessageChains = true;
  data.workQueue = [];
  data.botDeliveries = [{
    id: 'delivery', exchangeId: 'exchange', chainId: 'delivery', hop: 1,
    sourceConversationId: source.id, sourceMessageId: source.messages[0].id,
    recipientConversationId: 'recipient', fromBotId: source.botId, toBotId: 'b',
    text: 'Public fixture', createdAt: '', status: 'running',
  }];
  assert.equal(pendingBotWork(data, source), true, 'the transcript may show that another Bot is answering');
  assert.equal(hasConversationWork(data, source), false, 'waiting on another conversation does not create a misleading Stop');
  data.workQueue.push({ id: 'queued', conversationId: source.id, botId: source.botId, createdAt: '', kind: 'user', text: 'Second question', attachments: [] });
  assert.equal(hasConversationWork(data, source), true);
  data.workQueue[0].conversationId = 'other-conversation';
  assert.equal(hasConversationWork(data, source), false, 'same Bot, different queued conversation is not owned');
  source.run.status = 'awaiting_permission';
  assert.equal(hasConversationWork(data, source), true);
  assert.equal(hasConversationWork(data, undefined), false);
});

test('simultaneous approval snapshots and accepted same-conversation queues keep live runs, navigation, and newer drafts intact', () => {
  const data = snapshot(10, 'running');
  const first = data.conversations[0];
  first.run.status = 'awaiting_permission';
  first.run.permission = { id: 'permission-a', title: 'Approve A', kind: 'file_write', description: '', paths: [], options: [] };
  const second = conversation('second', 'b', 'awaiting_permission');
  second.run.permission = { id: 'permission-b', title: 'Approve B', kind: 'bot_message', description: '', paths: [], options: [] };
  data.conversations.push(second);
  data.bots.push({ ...data.bots[0], id: 'b', name: 'B' });
  let state = reducer(ui(), { type: 'snapshot', data });
  state = reducer(state, { type: 'draft', key: first.id, text: 'sent draft' });
  state = reducer(state, { type: 'draft', key: first.id, text: 'newer unsent draft' });
  state = reducer(state, { type: 'select', selection: { botId: 'b', conversationId: second.id } });
  const queued = structuredClone(data);
  queued.revision++;
  queued.workQueue = [{ id: 'queued-input', botId: first.botId, conversationId: first.id, createdAt: '', kind: 'user', text: 'sent draft', attachments: [] }];
  state = reducer(state, { type: 'snapshot', data: queued });
  state = reducer(state, { type: 'draft-sent', key: first.id, text: 'sent draft' });
  assert.deepEqual(state.selection, { botId: 'b', conversationId: second.id });
  assert.equal(state.drafts[first.id], 'newer unsent draft');
  assert.deepEqual(state.data?.conversations[0].run, first.run, 'admission does not overwrite the selected live run/permission');
  assert.equal(state.data?.conversations[1].run.permission?.id, 'permission-b');
  const accepted = state.data?.workQueue?.[0];
  assert.equal(accepted?.kind, 'user');
  if (accepted?.kind === 'user') assert.equal(accepted.text, 'sent draft');
  assert.equal(reducer(state, { type: 'snapshot', data }), state, 'a delayed HTTP response cannot drop the queue or either approval');
});

test('individual queue cancellation sends only the selected encoded ID and never falls back to conversation Stop', async (t) => {
  const data = snapshot(1, 'running');
  const calls: { path: string; options?: RequestInit }[] = [];
  t.mock.method(globalThis, 'fetch', async (path: string, options?: RequestInit) => {
    calls.push({ path, options });
    return new Response(JSON.stringify(calls.length === 1 ? data : { error: 'This queued turn no longer exists.' }), {
      status: calls.length === 1 ? 200 : 404,
      headers: { 'Content-Type': 'application/json' },
    });
  });
  const result = await cancelQueuedTurn('conversation/selected', 'turn?one');
  assert.equal(result.conversations[0].run.id, data.conversations[0].run.id);
  assert.equal(calls[0].path, '/api/conversations/conversation%2Fselected/queued-turns/turn%3Fone/cancel');
  assert.equal(calls[0].options?.method, 'POST');
  assert.equal(calls[0].options?.body, '{}');
  await assert.rejects(cancelQueuedTurn('conversation/selected', 'turn?one'), { status: 404 });
  assert.equal(calls.length, 2, 'a stale ID cannot trigger broad conversation cancellation');
  assert.equal(calls[1].path, calls[0].path);
});
