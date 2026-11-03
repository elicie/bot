import { test, expect, type Page } from '@playwright/test';
import { access, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import type { AppState } from '../shared/types';
import { startTestApp } from './app-fixture';
import { startMockProvider, type MockCompletion, type MockProviderRequest } from './mock-provider';

const state = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());
const mutate = <T,>(page: Page, path: string, body: unknown, method = 'POST'): Promise<T> => page.evaluate(async ({ path, body, method }) => {
  const response = await fetch(path, { method, headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify(body) });
  if (!response.ok) throw Error(await response.text());
  return response.json();
}, { path, body, method });
const gate = () => {
  let release!: (reply: MockCompletion) => void;
  return { promise: new Promise<MockCompletion>(resolve => { release = resolve; }), release: (text: string) => release({ text }) };
};
const main = (request: MockProviderRequest) => request.tools?.some(tool => tool.function.name === 'search_tool');
const content = (value: unknown) => typeof value === 'string' ? value : JSON.stringify(value ?? '');
const query = (request: MockProviderRequest) => request.messages.findLastIndex(message => message.role === 'user' && content(message.content).includes('<user_query>'));
const prompt = (request: MockProviderRequest) => content(request.messages[query(request)]?.content);

async function select(page: Page, name: string) {
  const button = page.getByRole('button', { name, exact: true });
  if (!await button.isVisible()) await page.getByRole('button', { name: '봇 목록 열기', exact: true }).click();
  await button.click();
}
async function send(page: Page, text: string) {
  await page.getByRole('textbox', { name: '메시지', exact: true }).fill(text);
  await expect(page.getByRole('button', { name: '메시지 보내기', exact: true })).toBeEnabled();
  await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
}
async function prepare(page: Page, url: string, baseUrl: string, workspace: string) {
  await page.goto(url);
  await mutate(page, '/api/settings', { baseUrl, model: 'local-test-model', workspace }, 'PUT');
  await mutate(page, '/api/bots', { name: 'Bot A', instructions: 'Answer as Bot A.', color: '#75a9c0', shape: 'blob' });
  await mutate(page, '/api/bots', { name: 'Bot B', instructions: 'Answer as Bot B.', color: '#68a884', shape: 'flower' });
  const snapshot = await state(page);
  const a = snapshot.bots.find(bot => bot.name === 'Bot A')!.id;
  const b = snapshot.bots.find(bot => bot.name === 'Bot B')!.id;
  const first = await mutate<{ conversation: { id: string } }>(page, '/api/conversations', { botId: a });
  const second = await mutate<{ conversation: { id: string } }>(page, '/api/conversations', { botId: b });
  await page.goto(`${url}/?bot=${a}&conversation=${first.conversation.id}`);
  await expect(page.getByRole('textbox', { name: '메시지', exact: true })).toBeVisible();
  return { a, b, first: first.conversation.id, second: second.conversation.id };
}

for (const viewport of [{ width: 1280, height: 900 }, { width: 390, height: 844 }]) {
  test(`different Bots chat concurrently, same Bot queues and cancels without stopping the other (${viewport.width})`, async ({ page }, testInfo) => {
    const app = await startTestApp();
    const heldA = gate();
    const heldB = gate();
    const prompts: string[] = [];
    const provider = await startMockProvider({ reply: request => {
      if (!main(request)) return { text: 'Concurrent chat fixture' };
      const text = prompt(request);
      prompts.push(text);
      if (text.includes('HOLD_A')) return heldA.promise;
      if (text.includes('HOLD_B')) return heldB.promise;
      return { text: 'QUEUED_B_FINISHED' };
    } });
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    try {
      await page.setViewportSize(viewport);
      const theme = viewport.width < 760 ? 'light' : 'dark';
      await page.emulateMedia({ colorScheme: theme, reducedMotion: 'reduce' });
      await page.addInitScript((theme) => localStorage.setItem('grok-bot.browser-preferences', JSON.stringify({ theme, spellcheck: true })), theme);
      const ids = await prepare(page, app.url, provider.baseUrl, '');
      // The limit is a real settings control and persists through a reload, not a fixture-only API.
      if (viewport.width < 760) await page.getByRole('button', { name: '봇 목록 열기', exact: true }).click();
      await page.getByRole('button', { name: '설정', exact: true }).click();
      await page.getByRole('button', { name: '모델 설정', exact: true }).first().click();
      const maximum = page.getByRole('spinbutton', { name: '동시 실행 Bot 수', exact: true });
      await expect(maximum).toHaveValue('3');
      await maximum.fill('2');
      await page.getByRole('button', { name: '설정 저장', exact: true }).click();
      await expect.poll(async () => (await state(page)).settings.maxConcurrentRuns).toBe(2);
      await page.getByRole('button', { name: '설정 닫기', exact: true }).click();
      await select(page, 'Bot A');
      await send(page, 'HOLD_A: 이 작업을 잠시 계속해 주세요.');
      await expect.poll(async () => (await state(page)).conversations.find(c => c.id === ids.first)?.run.status).toBe('running');
      const aRun = (await state(page)).conversations.find(c => c.id === ids.first)!.run.id;

      await send(page, 'CANCEL_A_QUEUE: 이 대기 요청만 취소합니다.');
      const pendingA = page.getByRole('region', { name: '대기 중인 메시지', exact: true });
      await expect(pendingA).toContainText('CANCEL_A_QUEUE');
      await pendingA.getByRole('button', { name: '대기 메시지 취소', exact: true }).click();
      await expect.poll(async () => (await state(page)).workQueue?.filter(work => work.conversationId === ids.first).length ?? 0).toBe(0);
      expect((await state(page)).conversations.find(c => c.id === ids.first)!.run.id).toBe(aRun);

      await select(page, 'Bot B');
      await page.getByRole('textbox', { name: '메시지', exact: true }).fill('HOLD_B: Bot A가 답변하는 동안 나도 대화합니다.');
      await expect(page.getByRole('button', { name: '메시지 보내기', exact: true })).toBeEnabled();
      await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
      await expect.poll(async () => (await state(page)).conversations.filter(c => c.run.status === 'running').length).toBe(2);
      const bRun = (await state(page)).conversations.find(c => c.id === ids.second)!.run.id;
      await send(page, 'B_QUEUE_SURVIVES: 다른 Bot의 Stop과 무관한 대기 메시지.');
      await expect(page.getByRole('region', { name: '대기 중인 메시지', exact: true })).toContainText('B_QUEUE_SURVIVES');
      await select(page, 'Bot A');
      await send(page, 'A_QUEUE_STOPPED: 이 대화의 Stop으로 함께 취소.');
      await page.reload();
      await expect(page.getByRole('region', { name: '대기 중인 메시지', exact: true })).toContainText('A_QUEUE_STOPPED');
      await page.getByRole('button', { name: '작업 중단', exact: true }).click();
      await expect.poll(async () => (await state(page)).conversations.find(c => c.id === ids.first)?.run.status).toBe('cancelled');
      const afterStop = await state(page);
      expect(afterStop.workQueue?.some(work => work.conversationId === ids.first)).toBeFalsy();
      expect(afterStop.workQueue?.some(work => work.conversationId === ids.second)).toBeTruthy();
      expect(afterStop.conversations.find(c => c.id === ids.second)!.run).toMatchObject({ id: bRun, status: 'running' });
      heldA.release('Cancelled A may not complete.');
      await select(page, 'Bot B');
      await expect(page.getByRole('region', { name: '대기 중인 메시지', exact: true })).toContainText('B_QUEUE_SURVIVES');
      await page.screenshot({ path: testInfo.outputPath(`concurrent-queue-${viewport.width}.png`), fullPage: true });
      heldB.release('Bot B first answer.');
      await expect(page.getByRole('log', { name: '대화 메시지', exact: true })).toContainText('QUEUED_B_FINISHED', { timeout: 30_000 });
      await expect.poll(async () => (await state(page)).workQueue?.length ?? 0).toBe(0);
      expect(prompts.some(text => text.includes('CANCEL_A_QUEUE') || text.includes('A_QUEUE_STOPPED'))).toBe(false);
      expect(prompts.filter(text => text.includes('B_QUEUE_SURVIVES'))).toHaveLength(1);
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
      expect(errors).toEqual([]);
    } finally {
      heldA.release('Cleanup A.');
      heldB.release('Cleanup B.');
      await app.stop();
      await provider.close();
    }
  });
}

test('two concurrent approval cards remain bound to their own Bot and SSE updates', async ({ page }) => {
  const app = await startTestApp();
  const provider = await startMockProvider({ reply: request => {
    if (!main(request)) return { text: 'Independent approval fixture' };
    const index = query(request);
    const text = prompt(request);
    const isA = text.includes('APPROVE_A');
    const calls = request.messages.slice(index + 1).flatMap(message => message.tool_calls ?? []) as Array<{ function?: { name?: string } }>;
    if (!calls.some(call => call.function?.name === 'search_tool')) return { tool: { name: 'search_tool', arguments: { query: 'workspace write_file', limit: 3 } } };
    if (!calls.some(call => call.function?.name === 'use_tool')) return { tool: { name: 'use_tool', arguments: {
      tool_name: 'workspace__write_file', tool_input: { path: isA ? 'approved-a.txt' : 'denied-b.txt', content: isA ? 'ONLY_A_APPROVED\n' : 'B_MUST_NOT_WRITE\n' },
    } } };
    return { text: isA ? 'APPROVED_A_COMPLETE' : 'DENIED_B_COMPLETE' };
  } });
  try {
    await page.setViewportSize({ width: 1280, height: 900 });
    const ids = await prepare(page, app.url, provider.baseUrl, app.workspace);
    await send(page, 'APPROVE_A: a 파일 저장을 제안해 주세요.');
    const card = page.getByRole('region', { name: '작업 승인 요청', exact: true });
    await expect(card).toContainText('approved-a.txt');
    const aPermission = (await state(page)).conversations.find(c => c.id === ids.first)!.run.permission!.id;
    await select(page, 'Bot B');
    await send(page, 'DENY_B: b 파일 저장을 제안해 주세요.');
    await expect(card).toContainText('denied-b.txt');
    const snapshot = await state(page);
    expect(snapshot.conversations.filter(c => c.run.status === 'awaiting_permission')).toHaveLength(2);
    expect(snapshot.conversations.find(c => c.id === ids.second)!.run.permission!.id).not.toBe(aPermission);
    await card.getByRole('button', { name: 'Deny', exact: true }).click();
    await expect(page.getByRole('log', { name: '대화 메시지', exact: true })).toContainText('DENIED_B_COMPLETE', { timeout: 30_000 });
    expect((await state(page)).conversations.find(c => c.id === ids.first)!.run.permission!.id).toBe(aPermission);
    await select(page, 'Bot A');
    await expect(card).toContainText('approved-a.txt');
    await card.getByRole('button', { name: 'Allow once', exact: true }).click();
    await expect(page.getByRole('log', { name: '대화 메시지', exact: true })).toContainText('APPROVED_A_COMPLETE', { timeout: 30_000 });
    await expect.poll(async () => (await state(page)).conversations.filter(c => c.run.status === 'awaiting_permission').length).toBe(0);
    expect(await readFile(join(app.workspace, 'approved-a.txt'), 'utf8')).toBe('ONLY_A_APPROVED\n');
    await expect(access(join(app.workspace, 'denied-b.txt'))).rejects.toThrow();
  } finally {
    await app.stop();
    await provider.close();
  }
});
