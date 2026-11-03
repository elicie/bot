import test from 'node:test';
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { reducer, emptyFiles, draftKey, readRoute, routeSearch, runningConversation, pendingWorkerBatch, workerBatchPending, pendingBotWork, ownConversationsFor, conversationsFor, type UIState } from '../web/state.js';
import type { AppState } from '../shared/types.js';

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

test('only executing hidden workers hold global admission while queued parents retain Stop', () => {
  const data = snapshot(1, 'completed');
  const parent = data.conversations[0];
  const batch = {
    id: randomUUID(), botId: parent.botId, parentConversationId: parent.id,
    sourceMessageId: parent.messages[0].id, sourceRunId: parent.run.id,
    createdAt: parent.updatedAt, cancelled: false,
    workers: [{ id: randomUUID(), conversationId: randomUUID(), task: 'Review city data', status: 'queued' as const, createdAt: parent.updatedAt }],
  };
  data.workerBatches = [batch];
  assert.equal(runningConversation(data), undefined, 'a queued batch does not lock unrelated conversations');
  assert.equal(pendingWorkerBatch(data, parent.id), batch);
  data.workerBatches = [{ ...batch, workers: [{ ...batch.workers[0], status: 'running' }] }];
  assert.equal(runningConversation(data), parent, 'an executing hidden child owns the global slot');
  assert.equal(pendingWorkerBatch(data, 'other'), undefined);
  const finished = { ...batch, workers: [{ ...batch.workers[0], status: 'completed' as const }], report: { status: 'queued' as const } };
  assert.equal(workerBatchPending(finished), true);
  assert.equal(workerBatchPending({ ...finished, report: { status: 'running' } }), true);
  assert.equal(workerBatchPending({ ...finished, report: { status: 'completed' } }), false);
  assert.equal(workerBatchPending({ ...batch, cancelled: true }), false);
  assert.equal(workerBatchPending({ ...batch, cancelled: true, workers: [{ ...batch.workers[0], status: 'running' }] }), true);
  data.workerBatches = [{ ...finished, report: { status: 'completed' } }];
  assert.equal(runningConversation(data), undefined);
  data.workerBatches = [];
  assert.equal(runningConversation(data), undefined);

  data.workerBatches = [{ ...finished, report: { status: 'queued' } }];
  assert.equal(runningConversation(data), undefined, 'a queued worker report does not lock unrelated conversations');
  data.workerBatches = [{ ...finished, report: { status: 'running' } }];
  assert.equal(runningConversation(data), parent, 'an executing worker report owns the global slot');
  const actualRun = { ...parent, id: 'actual-running-conversation', run: { id: 'actual-run', status: 'running' as const } };
  data.conversations.push(actualRun);
  assert.equal(runningConversation(data), actualRun, 'main’s actual active conversation takes priority over worker fallback');
});

test('pendingBotWork keeps Stop available for related deliveries and pending source reports only', () => {
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
    assert.equal(pendingBotWork(data, conversation), true, `source can stop a ${status} report after delivery completion`);
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

test('worker batches do not change own-conversation defaults, full history, or explicit recipient and empty-chat selection', () => {
  const data = snapshot(1, 'completed');
  const own = data.conversations[0];
  const newerOwn = { ...own, id: 'newer-own', updatedAt: '2026-10-31T12:00:00Z' };
  const recipient = { ...own, id: 'recipient', updatedAt: '2026-11-01T00:00:00Z' };
  data.conversations = [own, recipient, newerOwn];
  data.botDeliveries = [{
    id: 'delivery', exchangeId: 'exchange', chainId: 'delivery', hop: 1,
    sourceConversationId: 'other-source', sourceMessageId: 'other-message',
    recipientConversationId: recipient.id, fromBotId: 'other-bot', toBotId: own.botId,
    text: 'A public fixture question.', createdAt: recipient.updatedAt, status: 'waiting_children',
  }];
  data.workerBatches = [{
    id: 'batch', botId: own.botId, parentConversationId: own.id,
    sourceMessageId: own.messages[0].id, sourceRunId: own.run.id,
    createdAt: recipient.updatedAt, cancelled: false,
    workers: [{ id: 'worker', conversationId: 'hidden-child', task: 'Review public fixture data', status: 'running', createdAt: recipient.updatedAt }],
  }];
  assert.deepEqual(conversationsFor(data, own.botId).map(item => item.id), [recipient.id, newerOwn.id, own.id], 'history still includes recipient conversations');
  assert.deepEqual(ownConversationsFor(data, own.botId).map(item => item.id), [newerOwn.id, own.id], 'defaults and sidebar previews use own conversations only');
  const fresh = reducer(ui(), { type: 'snapshot', data });
  assert.equal(fresh.selection.conversationId, newerOwn.id);

  const explicit = reducer(fresh, { type: 'select', selection: { botId: own.botId, conversationId: recipient.id } });
  assert.equal(explicit.selection.conversationId, recipient.id, 'history and execution links may explicitly open a recipient');
  const remembered = reducer({ ...explicit, selection: { botId: own.botId, conversationId: 'missing' } }, { type: 'snapshot', data });
  assert.equal(remembered.selection.conversationId, recipient.id, 'a remembered explicit recipient wins over the own-conversation fallback');
  assert.deepEqual(ownConversationsFor(data, own.botId).map(item => item.id), [newerOwn.id, own.id], 'remembering a recipient never makes it an own-conversation preview');

  const empty = reducer(remembered, { type: 'select', selection: { botId: own.botId, conversationId: null } });
  assert.equal(reducer(empty, { type: 'snapshot', data }).selection.conversationId, null, 'an explicit empty chat survives worker snapshots');
  const recipientOnly = { ...data, conversations: [recipient] };
  assert.deepEqual(conversationsFor(recipientOnly, own.botId).map(item => item.id), [recipient.id]);
  assert.deepEqual(ownConversationsFor(recipientOnly, own.botId), []);
  assert.equal(reducer(ui(), { type: 'snapshot', data: recipientOnly }).selection.conversationId, null, 'recipient-only history opens a new empty chat even with worker batches');
});
