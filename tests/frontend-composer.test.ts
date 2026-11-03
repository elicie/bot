import test from 'node:test';
import assert from 'node:assert/strict';
import { createElement, isValidElement, type ComponentProps, type ReactElement, type ReactNode } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { register } from 'node:module';
import type { AppState, Conversation } from '../shared/types.js';

// These assertions cover actions/handlers and state, not styling; parent-owned browser tests cover CSS.
// register (Node 20.6+) also supports the project's Node 22.12 floor, unlike registerHooks.
register(`data:text/javascript,${encodeURIComponent(`
  export async function load(url, context, nextLoad) {
    return url.endsWith('.css') ? { format: 'module', source: '', shortCircuit: true } : nextLoad(url, context);
  }
`)}`, import.meta.url);
const { Composer, OtherRuns, Transcript } = await import('../web/Conversation.js');
const { QueuedTurns } = await import('../web/QueuedTurns.js');
const { GroupView, GroupDialog, groupStructureBusy } = await import('../web/Groups.js');
const { ThreadPanel } = await import('../web/Threads.js');
const { botReportDescription } = await import('../web/BotChat.js');

type ComposerProps = ComponentProps<typeof Composer>;
const props = (overrides: Partial<ComposerProps> = {}): ComposerProps => ({
  botName: 'A', value: 'A separate user question', onChange() {}, onSend() {}, onCancel() {},
  running: false, pendingWork: false, disabled: false, busy: false, model: 'fixture', ...overrides,
});
const render = (overrides: Partial<ComposerProps> = {}) => renderToStaticMarkup(createElement(Composer, props(overrides)));
const sendButton = (html: string) => html.match(/<button type="submit"[^>]*>/)?.[0] ?? '';

function descendants(node: ReactNode): ReactElement<Record<string, unknown>>[] {
  if (Array.isArray(node)) return node.flatMap(descendants);
  if (!isValidElement<Record<string, unknown>>(node)) return [];
  return [node, ...descendants(node.props.children as ReactNode)];
}

// Render under React's hook dispatcher, then exercise the actual form/keyboard callbacks.
function captureComposer(input: ComposerProps) {
  let tree: ReactNode;
  function Probe() { tree = Composer(input); return tree; }
  renderToStaticMarkup(createElement(Probe));
  return descendants(tree);
}

test('own queued work leaves Send alongside a single conversation-only Stop', () => {
  const html = render({ pendingWork: true });
  assert.equal((html.match(/aria-label="작업 중단"/g) ?? []).length, 1);
  assert.match(html, /title="이 대화의 실행과 대기 중인 작업만 중단"/);
  assert.match(sendButton(html), /aria-label="메시지 보내기"/);
  assert.doesNotMatch(sendButton(html), /disabled/);
  assert.doesNotMatch(html, /Bot 체인 중단/);
  assert.match(html, /<textarea[^>]*>A separate user question<\/textarea>/);
});

test('an active selected turn keeps Stop while a nonempty draft can be submitted to its queue', () => {
  const html = render({ running: true, pendingWork: true });
  assert.equal((html.match(/aria-label="작업 중단"/g) ?? []).length, 1);
  assert.equal((html.match(/aria-label="메시지 보내기"/g) ?? []).length, 1);
  assert.doesNotMatch(sendButton(html), /disabled/);
  const empty = render({ running: true, value: '  ' });
  assert.match(empty, /aria-label="작업 중단"/);
  assert.doesNotMatch(empty, /aria-label="메시지 보내기"/);
  const otherBot = render();
  assert.doesNotMatch(otherBot, /aria-label="작업 중단"/);
  assert.doesNotMatch(sendButton(otherBot), /disabled/);
});

test('form submit and Enter enqueue a live conversation draft while Shift+Enter and IME remain unchanged', () => {
  let sent = 0;
  let prevented = 0;
  const tree = captureComposer(props({ running: true, onSend() { sent++; } }));
  const submit = tree.find(node => node.type === 'form')!.props.onSubmit as (event: { preventDefault(): void }) => void;
  const keyDown = tree.find(node => node.type === 'textarea')!.props.onKeyDown as (event: {
    key: string; shiftKey: boolean; nativeEvent: { isComposing: boolean }; keyCode: number; preventDefault(): void;
  }) => void;
  submit({ preventDefault() { prevented++; } });
  const enter = { key: 'Enter', shiftKey: false, nativeEvent: { isComposing: false }, keyCode: 13, preventDefault() { prevented++; } };
  keyDown(enter);
  assert.equal(sent, 2);
  assert.equal(prevented, 2);
  keyDown({ ...enter, shiftKey: true });
  keyDown({ ...enter, nativeEvent: { isComposing: true } });
  keyDown({ ...enter, keyCode: 229 });
  assert.equal(sent, 2);
  assert.equal(prevented, 2);
});

test('offline, in-flight, read-only, and empty drafts still block submit without dropping source text', () => {
  for (const blocked of [{ disabled: true }, { busy: true }, { readOnly: true }, { value: ' ' }]) {
    let sent = 0;
    const tree = captureComposer(props({ ...blocked, running: true, onSend() { sent++; } }));
    const submit = tree.find(node => node.type === 'form')!.props.onSubmit as (event: { preventDefault(): void }) => void;
    submit({ preventDefault() {} });
    assert.equal(sent, 0);
    const html = render({ ...blocked, pendingWork: true });
    assert.match(sendButton(html), /disabled/);
    if (!('value' in blocked)) assert.match(html, /A separate user question/);
  }
});

test('attachments stay addable and attachment-only messages can queue while the selected turn is live', () => {
  let sent = 0;
  const attachments: NonNullable<ComposerProps['attachments']> = {
    items: [{ key: 'file', kind: 'text', file: new File(['public fixture'], 'notes.txt', { type: 'text/plain' }) }],
    errors: [], add() {}, remove() {}, dismiss() {},
  };
  const input = props({ running: true, value: '', attachments, onSend() { sent++; } });
  const html = renderToStaticMarkup(createElement(Composer, input));
  assert.doesNotMatch(html.match(/<button[^>]*aria-label="파일 첨부"[^>]*>/)?.[0] ?? '', /disabled/);
  assert.doesNotMatch(sendButton(html), /disabled/);
  assert.match(html, /notes.txt/);
  const tree = captureComposer(input);
  const submit = tree.find(node => node.type === 'form')!.props.onSubmit as (event: { preventDefault(): void }) => void;
  submit({ preventDefault() {} });
  assert.equal(sent, 1);
});

function fixture(): AppState {
  const timestamp = '2026-11-03T00:00:00Z';
  return {
    revision: 1,
    bots: ['a', 'b', 'c'].map(id => ({ id, name: id.toUpperCase(), color: '#198aff', instructions: '', createdAt: timestamp })),
    conversations: [], routines: [], routineRuns: [], workQueue: [],
    groups: [{ id: 'group', name: 'Team', color: '#198aff', memberBotIds: ['a', 'b'], createdAt: timestamp, updatedAt: timestamp }],
    settings: { baseUrl: 'http://127.0.0.1/v1', model: 'fixture', workspace: '', hasApiKey: false },
    runtime: { available: true, botMessageChains: true },
  };
}
const conversation = (id: string, botId: string, status: Conversation['run']['status'] = 'running'): Conversation => ({
  id, botId, title: `Title ${id}`, updatedAt: '2026-11-03T00:00:00Z', messages: [], run: { id: `run:${id}`, botId, status },
});

test('accepted own user turns show text, attachment names, frozen speakers, and cancel exactly their selected ID', () => {
  const data = fixture();
  data.workQueue = [
    { id: 'queued-one', botId: 'a', conversationId: 'selected', createdAt: '', kind: 'user', text: 'First public question', attachments: [] },
    { id: 'queued-other', botId: 'b', conversationId: 'other', createdAt: '', kind: 'user', text: 'Must not appear', attachments: [] },
    { id: 'queued-two', botId: 'b', conversationId: 'selected', createdAt: '', kind: 'user', text: '', attachments: [{ id: 'file-id', name: 'queued-notes.txt', kind: 'text', mimeType: 'text/plain', size: 12 }], speakerBotIds: ['b', 'c'] },
    { id: 'report', botId: 'a', conversationId: 'selected', createdAt: '', kind: 'report', deliveryId: 'delivery' },
  ];
  const cancelled: string[] = [];
  const tree = QueuedTurns({ data, conversationId: 'selected', onCancel: id => cancelled.push(id) });
  const html = renderToStaticMarkup(tree);
  assert.match(html, /aria-label="대기 중인 메시지"/);
  assert.match(html, /role="status" aria-live="polite" aria-atomic="true">대기 중 · 3개/);
  assert.match(html, /data-queued-turn-id="queued-one"/);
  assert.match(html, /data-queued-turn-id="queued-two"/);
  assert.match(html, /First public question/);
  assert.match(html, /queued-notes.txt/);
  assert.match(html, /답변할 Bot: B, C/);
  assert.doesNotMatch(html, /Must not appear/);
  assert.equal((html.match(/aria-label="대기 메시지 취소"/g) ?? []).length, 2, 'specialized work has no unsupported per-item cancel');
  const buttons = descendants(tree).filter(node => node.props.label === '대기 메시지 취소');
  (buttons[1].props.onClick as () => void)();
  assert.deepEqual(cancelled, ['queued-two']);
  assert.equal(data.workQueue.length, 4, 'no optimistic queue/run mutation');
  assert.equal(QueuedTurns({ data, conversationId: 'missing', onCancel() {} }), null);
  assert.match(renderToStaticMarkup(createElement(QueuedTurns, { data, conversationId: 'selected', disabled: true, onCancel() {} })), /aria-label="대기 메시지 취소"[^>]*disabled/);
});

test('all other active conversations stay reachable even if the first run is already visible, with approval attention', () => {
  const data = fixture();
  const shown = conversation('shown', 'a');
  const approval = conversation('approval', 'b', 'awaiting_permission');
  const group = { ...conversation('group-thread', 'group'), groupId: 'group', thread: { parentConversationId: 'parent', parentMessageId: 'message' }, run: { id: 'group-run', botId: 'c', status: 'starting' as const } };
  data.conversations = [shown, approval, group, conversation('finished', 'a', 'completed')];
  const opened: string[] = [];
  const tree = OtherRuns({ data, shownIds: [shown.id], onOpen: item => opened.push(item.id) });
  const html = renderToStaticMarkup(tree);
  assert.doesNotMatch(html, /data-conversation-id="shown"|Title finished/);
  assert.match(html, /data-conversation-id="approval"/);
  assert.match(html, /data-conversation-id="group-thread"/);
  assert.match(html, /승인 대기/);
  assert.match(html, /C · Team · 스레드/);
  const buttons = descendants(tree).filter(node => node.type === 'button');
  assert.equal(buttons.length, 2);
  (buttons[1].props.onClick as () => void)();
  assert.deepEqual(opened, ['group-thread']);
});

test('group and normal/group thread composers all admit drafts while their own or another Bot is active', () => {
  for (const status of ['running', 'completed'] as const) {
    const data = fixture();
    const group: Conversation = { ...conversation('group-chat', 'group', status), groupId: 'group', groupRound: { id: 'round', userMessageId: 'message', pendingBotIds: ['b'], createdAt: '' } };
    data.conversations = [conversation('other-bot', 'b'), group];
    const common = { data, busy: false, online: true, run: async <T,>(_name: string, operation: () => Promise<T>) => operation(), onSnapshot() {}, onSettings() {} };
    const groupHtml = renderToStaticMarkup(createElement(GroupView, { ...common,
      surface: { kind: 'group', groupId: 'group', conversationId: group.id }, onRoute() {}, onSidebar() {}, onEdit() {}, onThread() {}, draft: () => 'Queued group question', onDraft() {},
    }));
    assert.doesNotMatch(sendButton(groupHtml), /disabled/);
    assert.match(groupHtml, /aria-label="작업 중단"/);
    assert.doesNotMatch(groupHtml.match(/<button[^>]*aria-label="새 대화"[^>]*>/)?.[0] ?? '', /disabled/);
    for (const groupThread of [false, true]) {
      const thread: Conversation = { ...conversation('thread', groupThread ? 'group' : 'a', status), thread: { parentConversationId: group.id, parentMessageId: 'message' }, ...(groupThread ? { groupId: 'group', groupRound: group.groupRound } : {}) };
      data.conversations = [conversation('other-bot', 'b'), group, thread];
      const threadHtml = renderToStaticMarkup(createElement(ThreadPanel, { ...common, threadId: thread.id, onClose() {}, draft: 'Queued thread question', onDraft() {} }));
      assert.doesNotMatch(sendButton(threadHtml), /disabled/);
      assert.match(threadHtml, /aria-label="스레드 메시지"/);
    }
  }
});

test('group membership and deletion wait only for that group while normal chat remains independent', () => {
  const data = fixture();
  const group = data.groups![0];
  const render = () => renderToStaticMarkup(createElement(GroupDialog, {
    data, group, busy: false, error: null, run: async <T,>(_name: string, operation: () => Promise<T>) => operation(),
    onSnapshot() {}, onClose() {}, onCreated() {}, onDeleted() {},
  }));
  data.conversations = [conversation('other-bot', 'b')];
  assert.equal(groupStructureBusy(data, group.id), false);
  assert.doesNotMatch(render().match(/<button[^>]*aria-label="B 위로"[^>]*>/)?.[0] ?? '', /disabled/);
  const own: Conversation = { ...conversation('group-last-speaker', group.id), groupId: group.id, run: { id: 'group-run', botId: 'b', status: 'running' } };
  data.conversations.push(own);
  assert.equal(groupStructureBusy(data, group.id), true, 'the last speaker remains busy after pending member IDs are exhausted');
  assert.match(render().match(/<button[^>]*aria-label="B 위로"[^>]*>/)?.[0] ?? '', /disabled/);
  own.run.status = 'completed';
  data.workQueue = [{ id: 'accepted-next-round', botId: 'a', conversationId: own.id, createdAt: '', kind: 'user', text: 'Next group round', attachments: [] }];
  assert.equal(groupStructureBusy(data, group.id), true, 'accepted future rounds also protect membership order');
  data.workQueue = [];
  assert.equal(groupStructureBusy(data, group.id), false);
});

test('independent approval cards preserve their own permission option identity', () => {
  const data = fixture();
  for (const id of ['a', 'b']) {
    const selected = conversation(id, id, 'awaiting_permission');
    selected.run.permission = { id: `permission-${id}`, title: `Approval ${id}`, kind: 'file_write', paths: [], description: '', options: [{ id: `allow-${id}`, label: '한 번 허용', kind: 'allow_once' }, { id: `deny-${id}`, label: '거절', kind: 'reject_once' }] };
    data.conversations.push(selected);
  }
  for (let index = 0; index < 2; index++) {
    const html = renderToStaticMarkup(createElement(Transcript, { bot: data.bots[index], conversation: data.conversations[index], data, modelConfigured: true, runtimeAvailable: true, busy: false, onSettings() {}, onRuntimeSettings() {}, onPermission() {} }));
    const id = data.bots[index].id;
    assert.match(html, new RegExp(`data-option-id="allow-${id}"`));
    assert.match(html, new RegExp(`data-option-id="deny-${id}"`));
    assert.doesNotMatch(html, new RegExp(`data-option-id="allow-${id === 'a' ? 'b' : 'a'}"`));
  }
});

test('application-owned skipped reasons remain Korean rather than being labelled as quota exhaustion', () => {
  for (const error of [
    'This bot message chain was stopped.',
    'Bot message chain exceeded the 30-minute wall-clock limit.',
    'The sending recipient conversation already finished.',
    'The sending recipient run failed or stopped.',
  ]) {
    const label = botReportDescription({ status: 'skipped', error });
    assert.match(label, /[가-힣]/);
    assert.notEqual(label, error);
  }
  assert.match(botReportDescription({ status: 'skipped' }), /한도/);
  assert.equal(botReportDescription({ status: 'error', error: 'provider diagnostic' }), 'provider diagnostic');
});
