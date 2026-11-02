import test from 'node:test';
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { reducer, emptyFiles, draftKey, readRoute, routeSearch, runningConversation, pendingWorkerBatch, workerBatchPending, type UIState } from '../web/state.js';
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
});
