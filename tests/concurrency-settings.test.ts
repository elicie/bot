import test from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import type { AppState, Bot } from '../shared/types.js';

register(`data:text/javascript,${encodeURIComponent(`
  export async function load(url, context, nextLoad) {
    return url.endsWith('.css') ? { format: 'module', source: '', shortCircuit: true } : nextLoad(url, context);
  }
`)}`, import.meta.url);
const { Settings, initialSettingsForm, formReducer, settingsFormDirty, settingsFormInput, concurrencyLimitError } = await import('../web/Settings.js');
const { BotDialog, History } = await import('../web/Bots.js');
const { BotDeleteDialog } = await import('../web/BotMenu.js');
const { BotDataSections } = await import('../web/BotData.js');
const { Skills } = await import('../web/Skills.js');
const { BotIntegrations, vaultScopeRunning } = await import('../web/Vault.js');

Object.defineProperty(globalThis, 'localStorage', { configurable: true, value: { getItem: () => null } });
Object.defineProperty(globalThis, 'window', { configurable: true, value: {
  location: { host: 'localhost' }, matchMedia: () => ({ matches: false }),
} });
const timestamp = '2026-11-03T00:00:00Z';
function fixture(): AppState {
  return {
    revision: 1, routines: [], routineRuns: [],
    bots: ['a', 'b'].map((id) => ({ id, name: `Bot ${id.toUpperCase()}`, instructions: '', color: '#198aff', createdAt: timestamp })),
    conversations: [{ id: 'group-conversation', botId: 'group', groupId: 'group', title: 'Group', updatedAt: timestamp,
      run: { id: 'turn', botId: 'a', status: 'running' },
      messages: [{ id: 'answer', role: 'assistant', speakerBotId: 'b', text: '', tools: [], createdAt: timestamp }] }],
    settings: { baseUrl: 'http://localhost/v1', model: 'fixture', workspace: '/tmp/workspace', hasApiKey: false },
    runtime: { available: true },
  };
}
function input(html: string, label: string) {
  const match = html.match(new RegExp(`<input[^>]*aria-label="${label}"[^>]*>`));
  assert.ok(match, `Missing ${label} input`);
  return match[0];
}
function button(html: string, label: string) {
  const match = html.match(new RegExp(`<button[^>]*aria-label="${label}"[^>]*>`));
  assert.ok(match, `Missing ${label} button`);
  return match[0];
}
const renderSettings = (data: AppState) => renderToStaticMarkup(createElement(Settings, {
  data, tab: 'models', busy: false, error: null, onTab() {}, onClose() {}, onFiles() {}, onSave: async () => true,
  linkPreviewsEnabled: true, linkPreviewsSaving: false, onLinkPreviewsChange: async () => {},
}));

test('legacy settings default to three and concurrency-only edits participate in dirty/save state', () => {
  const data = fixture();
  let form = initialSettingsForm(data.settings);
  assert.equal(form.maxConcurrentRuns, '3');
  assert.equal(settingsFormDirty(form), false);
  form = formReducer(form, { type: 'field', field: 'maxConcurrentRuns', value: '8' });
  assert.equal(settingsFormDirty(form), true);
  assert.equal(settingsFormInput(form)?.maxConcurrentRuns, 8);
  form = formReducer(form, { type: 'saved' });
  assert.equal(form.saved, true);
  assert.equal(settingsFormDirty(form), false);
});

test('concurrency saves accept integer boundaries and reject empty, fractional, or out-of-range drafts', () => {
  for (let limit = 1; limit <= 16; limit++) assert.equal(concurrencyLimitError(String(limit)), null);
  for (const value of ['', ' ', '0', '-1', '17', '1.5', 'NaN', 'Infinity', 'not-a-number']) {
    assert.match(concurrencyLimitError(value) ?? '', /1.*16.*정수/);
    const form = formReducer(initialSettingsForm(fixture().settings), { type: 'field', field: 'maxConcurrentRuns', value });
    assert.equal(settingsFormInput(form), null, `${JSON.stringify(value)} must not reach SettingsInput`);
  }
});

test('live settings reconcile untouched fields without overwriting a dirty concurrency draft', () => {
  const initial = fixture().settings;
  let form = formReducer(initialSettingsForm(initial), { type: 'field', field: 'maxConcurrentRuns', value: '8' });
  form = formReducer(form, { type: 'reconcile', settings: { ...initial, maxConcurrentRuns: 6, model: 'new-model' } });
  assert.equal(form.maxConcurrentRuns, '8');
  assert.equal(form.model, 'new-model');
  assert.equal(settingsFormDirty(form), true);
  form = formReducer(form, { type: 'reconcile', settings: { ...initial, maxConcurrentRuns: 8, model: 'new-model' } });
  assert.equal(settingsFormDirty(form), false);
  const clean = formReducer(initialSettingsForm(initial), { type: 'reconcile', settings: { ...initial, maxConcurrentRuns: 16 } });
  assert.equal(clean.maxConcurrentRuns, '16');
  assert.equal(settingsFormDirty(clean), false);
});

test('reconciliation preserves credential intent and save includes the numeric limit without exposing a stored key', () => {
  let form = initialSettingsForm(fixture().settings);
  form = formReducer(form, { type: 'field', field: 'apiKey', value: 'new-api-key' });
  form = formReducer(form, { type: 'reconcile', settings: { ...fixture().settings, maxConcurrentRuns: 5 } });
  assert.equal(form.apiKey, 'new-api-key');
  assert.equal(settingsFormInput(form)?.apiKey, 'new-api-key');
  assert.equal(settingsFormInput(form)?.maxConcurrentRuns, 5);
  form = formReducer(form, { type: 'clear', value: true });
  assert.equal(settingsFormInput(form)?.clearApiKey, true);
  assert.equal(settingsFormInput(form)?.apiKey, undefined);
  form = formReducer(form, { type: 'saved' });
  assert.equal(settingsFormDirty(form), false);
});

test('Models exposes the labelled number input and locks shared configuration during any live Bot turn', () => {
  const data = fixture();
  const html = renderSettings(data);
  const concurrency = input(html, '동시 실행 Bot 수');
  assert.match(concurrency, /type="number"/);
  assert.match(concurrency, /min="1" max="16" step="1"/);
  assert.match(concurrency, /value="3"/);
  assert.match(concurrency, /disabled=/);
  assert.match(input(html, 'Base URL'), /disabled=/);
  assert.match(html, /모든 Bot의 작업이 끝나면 설정을 저장할 수 있습니다/);
  assert.doesNotMatch(button(html, '설정 닫기'), /disabled=/);
  data.conversations[0].run.status = 'completed';
  const idle = renderSettings(data);
  assert.doesNotMatch(input(idle, '동시 실행 Bot 수'), /disabled=/);
  assert.doesNotMatch(input(idle, 'Base URL'), /disabled=/);
});

function renderBotDialog(data: AppState, bot?: Bot) {
  return renderToStaticMarkup(createElement(BotDialog, { data, bot, busy: false, error: null, onClose() {}, onSave: async () => true }));
}
test('Bot editing and deletion lock only the actual group owner, while creation/history remain available', () => {
  const data = fixture();
  assert.match(input(renderBotDialog(data, data.bots[0]), '봇 이름'), /disabled=/);
  assert.doesNotMatch(input(renderBotDialog(data, data.bots[1]), '봇 이름'), /disabled=/);
  assert.doesNotMatch(input(renderBotDialog(data), '봇 이름'), /disabled=/);
  const deleting = (bot: Bot) => renderToStaticMarkup(createElement(BotDeleteDialog, { data, bot, busy: false, error: null, onCancel() {}, onConfirm() {} }));
  assert.match(deleting(data.bots[0]), /이 Bot의 작업이 끝난 뒤 삭제할 수 있어요/);
  assert.doesNotMatch(deleting(data.bots[1]), /이 Bot의 작업이 끝난 뒤 삭제할 수 있어요/);
  const history = renderToStaticMarkup(createElement(History, { bot: data.bots[1], conversations: [], selectedId: null, busy: false, onSelect() {}, onNew() {}, onClose() {} }));
  assert.match(history, /<button type="button" class="small-button">/);
});

test('private skill mutations and local MCP switches lock by owner, not by another active Bot', () => {
  const data = fixture();
  const renderSkills = (bot: Bot) => renderToStaticMarkup(createElement(Skills, { data, bot, busy: false, onClose() {}, onChange() {}, onCreateWithBot() {} }));
  const requestButton = (html: string) => html.match(/<button[^>]*>[^<]*(?:<svg[\s\S]*?<\/svg>)?봇에게 만들기 요청<\/button>/)?.[0] ?? '';
  assert.match(requestButton(renderSkills(data.bots[0])), /disabled=/);
  assert.doesNotMatch(requestButton(renderSkills(data.bots[1])), /disabled=/);
  data.mcpServers = data.bots.map((bot) => ({ id: `mcp-${bot.id}`, botId: bot.id, name: `server-${bot.id}`, transport: 'http', enabled: true, createdAt: timestamp, updatedAt: timestamp }));
  const renderMcp = (bot: Bot) => renderToStaticMarkup(createElement(BotIntegrations, { data, bot, dispatch() {} }));
  assert.match(input(renderMcp(data.bots[0]), 'server-a 사용'), /disabled=/);
  assert.doesNotMatch(input(renderMcp(data.bots[1]), 'server-b 사용'), /disabled=/);
});

test('Bot memories and knowledge wait only for their owner while template import remains global', () => {
  const data = fixture();
  for (const bot of data.bots) {
    bot.memories = [{ id: `memory-${bot.id}`, text: `Memory ${bot.id}`, source: 'user', createdAt: timestamp }];
    bot.knowledge = [{ id: `knowledge-${bot.id}`, name: `knowledge-${bot.id}.txt`, bytes: 12, hash: 'a'.repeat(64), createdAt: timestamp }];
  }
  const render = (bot: Bot) => renderToStaticMarkup(createElement(BotDataSections, { bot, data, dispatch() {}, onBotCreated() {} }));
  const knowledgeAdd = (html: string) => html.match(/<button[^>]*class="text-button"[^>]*>파일 추가<\/button>/)?.[0] ?? '';
  for (const [index, ownerBusy] of [[0, true], [1, false]] as const) {
    const html = render(data.bots[index]);
    for (const control of [input(html, '새 메모리'), button(html, `메모리 삭제: Memory ${data.bots[index].id}`), button(html, `파일 삭제: knowledge-${data.bots[index].id}.txt`), knowledgeAdd(html)]) {
      assert.ok(control, 'memory/knowledge control must exist');
      assert.equal(control.includes('disabled='), ownerBusy, 'only the actual group speaker owns a live Bot configuration slot');
    }
    assert.match(input(html, 'Bot 템플릿 파일 선택'), /disabled=/, 'template import still needs the entire app idle');
  }
  delete data.conversations[0].run.botId;
  assert.doesNotMatch(input(render(data.bots[0]), '새 메모리'), /disabled=/);
  assert.match(input(render(data.bots[1]), '새 메모리'), /disabled=/, 'legacy active speaker fallback also applies');
  data.conversations[0].run.status = 'completed';
  assert.doesNotMatch(input(render(data.bots[1]), 'Bot 템플릿 파일 선택'), /disabled=/);
});

test('global secrets stay blocked anywhere during a run; an idle Bot scope remains editable', () => {
  const data = fixture();
  assert.equal(vaultScopeRunning(data), true);
  assert.equal(vaultScopeRunning(data, ''), true);
  assert.equal(vaultScopeRunning(data, 'a'), true);
  assert.equal(vaultScopeRunning(data, 'b'), false);
  delete data.conversations[0].run.botId;
  assert.equal(vaultScopeRunning(data, 'a'), false);
  assert.equal(vaultScopeRunning(data, 'b'), true);
  data.conversations[0].run.status = 'completed';
  assert.equal(vaultScopeRunning(data), false);
});
