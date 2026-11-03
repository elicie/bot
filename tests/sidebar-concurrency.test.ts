import test, { beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import type { AppState, Conversation, QueuedWork } from '../shared/types.js';

// Focused SSR checks exercise real state selectors and sidebar markup; the parent owns browser/CSS verification.
register(`data:text/javascript,${encodeURIComponent(`
  export async function load(url, context, nextLoad) {
    return url.endsWith('.css') ? { format: 'module', source: '', shortCircuit: true } : nextLoad(url, context);
  }
`)}`, import.meta.url);
const { Sidebar } = await import('../web/Sidebar.js');

const storage = new Map<string, string>();
Object.defineProperty(globalThis, 'localStorage', { configurable: true, value: {
  getItem: (key: string) => storage.get(key) ?? null,
  setItem: (key: string, value: string) => storage.set(key, value),
} });
beforeEach(() => storage.clear());
const timestamp = '2026-11-03T00:00:00Z';

function fixture(): AppState {
  const group: Conversation = {
    id: 'group-conversation', botId: 'group', groupId: 'group', title: 'Group', updatedAt: timestamp,
    run: { id: 'group-turn', botId: 'a', status: 'awaiting_permission' },
    messages: [{ id: 'answer', role: 'assistant', speakerBotId: 'b', text: '', tools: [], createdAt: timestamp }],
  };
  const queued: QueuedWork[] = [
    { id: 'a-user', botId: 'a', conversationId: 'a-conversation', kind: 'user', text: 'Next', attachments: [], createdAt: timestamp },
    { id: 'a-delivery', botId: 'a', conversationId: 'recipient', kind: 'delivery', deliveryId: 'delivery', createdAt: timestamp },
    { id: 'b-group', botId: 'b', conversationId: group.id, kind: 'group', roundId: 'round', speakerBotIds: ['b', 'c'], createdAt: timestamp },
  ];
  return {
    revision: 1, routines: [], routineRuns: [],
    bots: ['a', 'b', 'c'].map((id) => ({ id, name: `Bot ${id.toUpperCase()}`, color: '#198aff', instructions: '', pinned: id === 'a', unread: id === 'b', createdAt: timestamp })),
    conversations: [{ id: 'a-conversation', botId: 'a', title: 'Own chat', updatedAt: timestamp,
      run: { id: 'own-turn', status: 'completed' }, messages: [{ id: 'own-answer', role: 'assistant', text: 'Own preview', tools: [], createdAt: timestamp }] }, group],
    workQueue: queued,
    settings: { baseUrl: '', model: 'fixture', hasApiKey: false, workspace: '' },
    runtime: { available: true },
  };
}
function render(data: AppState, busy = false) {
  return renderToStaticMarkup(createElement(Sidebar, {
    data, busy, selection: { botId: 'a', conversationId: null }, lastConversation: { a: null }, chat: true,
    search: '', open: true, mobile: false,
    onSearch() {}, onSelect() {}, onClose() {}, onNew() {}, onSettings() {}, onRoutines() {}, onCapabilities() {}, onPatch() {},
  }));
}
function row(html: string, name: string) {
  const match = html.match(new RegExp(`<button[^>]*aria-label="${name}"[^>]*>[\\s\\S]*?<\\/button>`));
  assert.ok(match, `Missing ${name} sidebar button`);
  return match[0];
}

test('actual group owner shows active and pending simultaneously without changing the Bot button name', () => {
  const html = render(fixture());
  const a = row(html, 'Bot A');
  assert.match(a, /aria-current="true"/);
  assert.match(a, /data-running="true"/);
  assert.match(a, /data-pending="2"/);
  assert.match(a, /title="Bot A · 승인 대기 · 대기 2건"/);
  assert.match(a, /class="running-dot"/);
  assert.match(a, /class="bot-pending-count"[^>]*>2<\/span>/);
  assert.match(html, /id="execution-a" hidden="">승인 대기 · 대기 2건<\/span>/);
  assert.doesNotMatch(a, /disabled=/);
  assert.doesNotMatch(row(html, 'Bot B'), /data-running=/);
});

test('pending lanes use work.botId, never the group ID or later frozen speakers', () => {
  const html = render(fixture());
  const b = row(html, 'Bot B');
  assert.match(b, /data-pending="1"/);
  assert.match(b, /class="bot-execution-preview">대기 1건<\/small>/);
  assert.match(b, /data-unread="true"/);
  assert.match(b, /class="unread-dot"/);
  assert.doesNotMatch(row(html, 'Bot C'), /data-pending=|data-running=|bot-pending-count/);
  assert.doesNotMatch(html, /aria-label="group"/);
});

test('legacy group turns resolve the last assistant speaker and old snapshots tolerate no queue', () => {
  const data = fixture();
  delete data.conversations[1].run.botId;
  delete data.workQueue;
  const html = render(data);
  assert.doesNotMatch(row(html, 'Bot A'), /data-running=|bot-pending-count/);
  assert.match(row(html, 'Bot B'), /data-running="true"/);
  assert.match(row(html, 'Bot A'), /bot-tile/);
});

test('compact pinned avatars retain both execution indicators and unrelated creation stays enabled', () => {
  storage.set('grok-bot-sidebar-compact', 'true');
  const html = render(fixture());
  assert.match(html, /class="sidebar reference-sidebar [^"]*is-compact/);
  assert.match(row(html, 'Bot A'), /class="running-dot"/);
  assert.match(row(html, 'Bot A'), /class="bot-pending-count"/);
  assert.doesNotMatch(row(html, '봇 추가'), /disabled=/);
  assert.match(row(render(fixture(), true), '봇 추가'), /disabled=/);
});

test('idle ordinary rows retain their own conversation preview and hidden Bots stay hidden', () => {
  const data = fixture();
  data.workQueue = [];
  data.bots[2].hidden = true;
  data.conversations.push({ ...data.conversations[0], id: 'b-own', botId: 'b' });
  const html = render(data);
  assert.match(row(html, 'Bot B'), /<small>Own preview<\/small>/);
  assert.doesNotMatch(html, /aria-label="Bot C"/);
});
