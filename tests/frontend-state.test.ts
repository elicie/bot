import test from 'node:test';
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { reducer, emptyFiles, type UIState } from '../web/state.js';
import type { AppState } from '../shared/types.js';

function snapshot(revision: number, status: 'running' | 'completed'): AppState {
  const botId = 'a620c2e4-25b6-410b-9d32-20bf4e67753a';
  const conversationId = '42327d73-b322-488c-9cab-09598be6e0ef';
  return {
    revision,
    bots: [{ id: botId, name: 'Grok', color: '#75a9c0', instructions: '', createdAt: '2026-10-31T00:00:00Z' }],
    conversations: [{ id: conversationId, botId, title: 'Conversation', updatedAt: '2026-10-31T00:00:00Z', messages: [{ id: randomUUID(), role: 'assistant', text: status === 'completed' ? 'Final answer' : '', tools: [], createdAt: '2026-10-31T00:00:00Z' }], run: { id: 'run', status } }],
    settings: { baseUrl: 'http://127.0.0.1:8000/v1', model: 'test', hasApiKey: false, workspace: '/tmp/test-workspace' },
    runtime: { available: true, version: '1.0.62' },
  };
}

function ui(): UIState {
  return {
    data: null, connection: 'loading', connectionError: null,
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
