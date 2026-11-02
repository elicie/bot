import test from 'node:test';
import assert from 'node:assert/strict';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { registerHooks } from 'node:module';

// These SSR assertions cover action availability, not styling; real-app browser tests cover CSS.
registerHooks({ load(url, context, nextLoad) {
  return url.endsWith('.css') ? { format: 'module', source: '', shortCircuit: true } : nextLoad(url, context);
} });
const { Composer } = await import('../web/Conversation.js');
const { botReportDescription } = await import('../web/BotChat.js');

const render = (running: boolean, pendingBotWork: boolean, disabled = false) => renderToStaticMarkup(createElement(Composer, {
  botName: 'A', value: 'A separate user question', onChange() {}, onSend() {}, onCancel() {},
  running, pendingBotWork, disabled, busy: false, model: 'fixture',
}));

test('waiting Bot work leaves the normal send action available alongside chain Stop', () => {
  const html = render(false, true);
  assert.match(html, /aria-label="Bot 체인 중단"/);
  assert.match(html, /type="submit"[^>]*aria-label="메시지 보내기"[^>]*>/);
  assert.doesNotMatch(html.match(/<button type="submit"[^>]*>/)?.[0] ?? '', /disabled/);
  assert.match(html, /<textarea[^>]*>A separate user question<\/textarea>/);
});

test('only an active own turn replaces Send with the existing Stop action', () => {
  const html = render(true, true);
  assert.match(html, /aria-label="작업 중단"/);
  assert.doesNotMatch(html, /aria-label="메시지 보내기"/);
  assert.doesNotMatch(html, /aria-label="Bot 체인 중단"/);
});

test('application-owned skipped reasons are shown in Korean rather than labelled as quota exhaustion', () => {
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

test('the existing global-slot disabled state still prevents Send without removing the draft', () => {
  const html = render(false, true, true);
  assert.match(html.match(/<button type="submit"[^>]*>/)?.[0] ?? '', /disabled/);
  assert.match(html, /aria-label="Bot 체인 중단"/);
  assert.match(html, /A separate user question/);
});
