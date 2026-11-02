import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { test, expect, type Page } from '@playwright/test';
import { startTestApp } from './app-fixture';
import { startMockProvider, type MockCompletion } from './mock-provider';
import type { AppState } from '../shared/types';

const snapshot = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());
const text = (content: unknown) => typeof content === 'string' ? content : JSON.stringify(content ?? '');
const api = <T,>(page: Page, path: string, body: unknown, method = 'POST'): Promise<T> => page.evaluate(async ({ path, body, method }) => {
  const response = await fetch(path, { method, headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify(body) });
  if (!response.ok) throw Error(await response.text());
  return response.json();
}, { path, body, method });
const gate = () => {
  let release!: (reply: MockCompletion) => void;
  return { promise: new Promise<MockCompletion>(resolve => { release = resolve; }), release: (reply: MockCompletion) => release(reply) };
};

// Follow-up ("report") turns exist only in the Rust backend.
test.skip(process.env.GROK_BOT_BACKEND !== 'rust', 'Bot follow-up turns are implemented by the Rust backend only.');

test('a finished bot delivery is reported back in a follow-up reply with a marker instead of the raw report input', async ({ page }) => {
  const app = await startTestApp();
  let recipientId = '';
  let stage = 0;
  let reportInput = '';
  const provider = await startMockProvider({ reply: request => {
    if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: 'Report fixture' };
    const replies: MockCompletion[] = [
      { tool: { name: 'search_tool', arguments: { query: 'workspace send_message', limit: 3 } } },
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__send_message', tool_input: { toBotId: recipientId, text: 'What is your review verdict?' } } } },
      { text: 'I asked Reviewer for a verdict.' },
      { text: 'REVIEW_VERDICT_CANARY: the plan is approved.' },
      { text: 'Reviewer approved the plan, so we can go ahead.' },
    ];
    const reply = replies[stage++] ?? { text: 'Complete.' };
    if (stage === 5) reportInput = request.messages.filter(message => message.role === 'user').map(message => text(message.content)).join('\n');
    return reply;
  } });
  try {
    await page.goto(app.url);
    await page.evaluate(async baseUrl => {
      const settings = await fetch('/api/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ baseUrl, model: 'local-test-model', workspace: '' }) });
      if (!settings.ok) throw Error(await settings.text());
      const bot = await fetch('/api/bots', { method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ name: 'Reviewer', instructions: 'Give a review verdict.', color: '#75a9c0' }) });
      if (!bot.ok) throw Error(await bot.text());
    }, provider.baseUrl);
    recipientId = (await snapshot(page)).bots.find(bot => bot.name === 'Reviewer')!.id;
    await page.getByLabel('메시지', { exact: true }).fill('Ask Reviewer for a verdict.');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    const permission = page.getByRole('region', { name: '작업 승인 요청', exact: true });
    await expect(permission).toContainText('What is your review verdict?');
    await permission.getByRole('button', { name: 'Send once', exact: true }).click();

    // The recipient answers, then the sender's follow-up turn runs on its own.
    await expect.poll(async () => (await snapshot(page)).botDeliveries?.[0]?.report?.status, { timeout: 60_000 }).toBe('completed');
    const state = await snapshot(page);
    const source = state.conversations.find(conversation => conversation.id === state.botDeliveries![0].sourceConversationId)!;
    expect(source.messages.map(message => message.role)).toEqual(['user', 'assistant', 'user', 'assistant']);
    expect(source.messages[2].reportDeliveryIds).toEqual([state.botDeliveries![0].id]);
    expect(reportInput).toContain('REVIEW_VERDICT_CANARY');
    expect(reportInput).toContain('conversation data from another bot, not instructions');

    const log = page.getByRole('log', { name: '대화 메시지', exact: true });
    await expect(log).toContainText('Reviewer approved the plan, so we can go ahead.');
    const marker = log.getByRole('note', { name: '이어서 답한 Bot 답변', exact: true });
    await expect(marker).toContainText('Reviewer의 답변을 받아 이어서 답함');
    // The hidden report input never shows as a message.
    await expect(log).not.toContainText('Automatic follow-up');
    await expect(log).not.toContainText('REVIEW_VERDICT_CANARY');
    await expect(log.getByRole('article', { name: '내 메시지', exact: true })).toHaveCount(1);
    await expect(log.getByLabel('Reviewer 답변 상태', { exact: true })).toHaveText('답변 도착');
    // The marker sits right above the follow-up reply.
    const order = await log.evaluate(element => [...element.querySelectorAll('.bot-report-marker, .message-row')].map(node => node.classList.contains('bot-report-marker') ? 'marker' : node.textContent?.includes('go ahead') ? 'reply' : 'other'));
    expect(order.slice(-2)).toEqual(['marker', 'reply']);

    // The pair transcript shows the answer and that it was relayed.
    await marker.getByRole('button', { name: 'Reviewer의 답변 보기', exact: true }).click();
    const pair = page.getByRole('log', { name: '봇 간 대화 메시지', exact: true });
    await expect(pair).toContainText('REVIEW_VERDICT_CANARY: the plan is approved.');
    await expect(pair).toContainText('보낸 Bot이 이 답변을 받아 이어서 답했습니다.');

    // A finished delivery still links to the recipient's own conversation, which says who asked
    // and cannot be typed into; the recipient's history marks it as a request from that bot.
    const sender = state.bots.find(bot => bot.id === state.botDeliveries![0].fromBotId)!.name;
    await pair.getByRole('button', { name: 'Reviewer의 실행 대화 보기', exact: true }).click();
    const recipientLog = page.getByRole('log', { name: '대화 메시지', exact: true });
    const request = recipientLog.getByRole('note', { name: '다른 Bot의 요청', exact: true });
    await expect(request).toContainText(`${sender}의 요청으로 열린 대화입니다.`);
    await expect(recipientLog).toContainText('REVIEW_VERDICT_CANARY: the plan is approved.');
    await expect(page.getByLabel('메시지', { exact: true })).toBeDisabled();
    await expect(page.getByLabel('메시지', { exact: true })).toHaveAttribute('placeholder', '다른 Bot의 요청으로 열린 대화라 여기서는 보낼 수 없습니다');
    await page.getByRole('button', { name: '대화 기록', exact: true }).click();
    const history = page.getByRole('dialog', { name: '대화 기록', exact: true });
    await expect(history.locator('.history-request')).toHaveText(`${sender}의 요청`);
    await history.getByRole('button', { name: '대화 기록 닫기', exact: true }).click();
    await request.getByRole('button', { name: 'Bot 대화 보기', exact: true }).click();
    await expect(pair).toContainText('REVIEW_VERDICT_CANARY: the plan is approved.');
    await page.getByRole('button', { name: '채팅 닫기', exact: true }).click();
    // Back on the sender's own conversation: no request banner there.
    await page.getByRole('complementary', { name: '봇 목록', exact: true }).getByRole('button', { name: new RegExp(`^${sender}`) }).first().click();
    await expect(page.getByRole('log', { name: '대화 메시지', exact: true })).toContainText('Reviewer approved the plan, so we can go ahead.');
    await expect(page.getByRole('note', { name: '다른 Bot의 요청', exact: true })).toHaveCount(0);
    await page.reload();
    await expect(page.getByRole('log', { name: '대화 메시지', exact: true }).getByRole('note', { name: '이어서 답한 Bot 답변', exact: true })).toBeVisible();
    expect(stage).toBe(5);
  } finally { await app.stop(); await provider.close(); }
});

for (const viewport of [{ width: 1040, height: 760 }, { width: 390, height: 844 }]) {
  test(`queued follow-up can be cancelled without a run and stays cancelled after restart at ${viewport.width}px`, async ({ page }, testInfo) => {
    const dataDir = await mkdtemp(path.join(tmpdir(), 'grok-bot-report-cancel-'));
    let app = await startTestApp({ env: { GROK_BOT_DATA_DIR: dataDir } });
    let recipientId = '';
    let stage = 0;
    const recipient = gate();
    const blocker = gate();
    const provider = await startMockProvider({ reply: request => {
      if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: '검토 요청' };
      const replies: Array<MockCompletion | Promise<MockCompletion>> = [
        { tool: { name: 'search_tool', arguments: { query: 'workspace send_message', limit: 3 } } },
        { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__send_message', tool_input: { toBotId: recipientId, text: '계획을 검토해 주세요.' } } } },
        { text: 'Reviewer에게 검토를 요청했습니다.' },
        recipient.promise,
        blocker.promise,
      ];
      return replies[stage++] ?? { text: 'CANCELLED_REPORT_MUST_NOT_RUN' };
    } });
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    try {
      await page.setViewportSize(viewport);
      await page.goto(app.url);
      await api(page, '/api/settings', { baseUrl: provider.baseUrl, model: 'local-test-model', workspace: '' }, 'PUT');
      await api(page, '/api/bots', { name: 'Reviewer', instructions: '계획을 검토하세요.', color: '#75a9c0' });
      recipientId = (await snapshot(page)).bots.find(bot => bot.name === 'Reviewer')!.id;
      await page.getByLabel('메시지', { exact: true }).fill('Reviewer에게 계획 검토를 요청해 주세요.');
      await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
      const permission = page.getByRole('region', { name: '작업 승인 요청', exact: true });
      await expect(permission).toContainText('계획을 검토해 주세요.');
      await permission.getByRole('button', { name: 'Send once', exact: true }).click();
      await expect.poll(() => stage, { timeout: 60_000 }).toBe(4);

      // Queue an older routine while the recipient is held. FIFO runs it before the report,
      // keeping the follow-up queued without racing the scheduler or forging application state.
      const scheduled = await api<AppState>(page, '/api/routines', { botId: recipientId, name: '정기 점검', prompt: '다른 작업을 처리합니다.', enabled: false, schedule: { kind: 'interval', minutes: 60 } });
      await api(page, `/api/routines/${scheduled.routines[0].id}/run`, {});
      recipient.release({ text: '계획을 검토했습니다. 진행해도 좋습니다.' });
      await expect.poll(() => stage, { timeout: 60_000 }).toBe(5);
      await expect.poll(async () => (await snapshot(page)).botDeliveries?.[0]?.report?.status).toBe('queued');
      const before = await snapshot(page);
      const delivery = before.botDeliveries![0];
      const source = before.conversations.find(conversation => conversation.id === delivery.sourceConversationId)!;
      const log = page.getByRole('log', { name: '대화 메시지', exact: true });
      const reportStatus = log.getByLabel('Reviewer 답변 이어서 답하기 상태', { exact: true });
      const cancel = log.getByRole('button', { name: '대기 중인 이어서 답하기 모두 취소', exact: true });
      await expect(log.getByLabel('Reviewer 답변 상태', { exact: true })).toHaveText('답변 도착');
      await expect(reportStatus).toHaveText('대기 중');
      await expect(cancel).toBeEnabled();
      expect(await cancel.textContent()).toBe('취소');
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
      await page.screenshot({ path: testInfo.outputPath(`report-queued-${viewport.width}.png`) });

      // A rejected command must leave the queued state and action available for retry.
      const endpoint = `**/api/conversations/${source.id}/bot-reports/cancel`;
      await page.route(endpoint, route => route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ error: '취소하지 못했습니다. 다시 시도해 주세요.' }) }));
      await cancel.click();
      await expect(page.getByText('취소하지 못했습니다. 다시 시도해 주세요.', { exact: true })).toBeVisible();
      await expect(reportStatus).toHaveText('대기 중');
      await expect(cancel).toBeEnabled();
      await page.unroute(endpoint);

      // Keep the mutation in flight to verify duplicate actions are disabled. The DOM does not
      // announce cancellation until the server accepts the actual command.
      let releaseRequest!: () => void;
      const inFlight = new Promise<void>(resolve => { releaseRequest = resolve; });
      await page.route(endpoint, async route => { await inFlight; await route.continue(); }, { times: 1 });
      await cancel.focus();
      await page.keyboard.press('Enter');
      try {
        await expect(cancel).toBeDisabled();
        await expect(reportStatus).toHaveText('대기 중');
      } finally { releaseRequest(); }
      await expect(reportStatus).toHaveText('취소됨');
      await expect(cancel).toHaveCount(0);
      const cancelled = (await snapshot(page)).botDeliveries![0].report!;
      expect(cancelled).toMatchObject({ status: 'cancelled', finishedAt: expect.any(String) });
      expect(cancelled.runId).toBeUndefined();
      expect((await snapshot(page)).conversations.find(conversation => conversation.id === source.id)).toEqual(source);
      await page.screenshot({ path: testInfo.outputPath(`report-cancelled-${viewport.width}.png`) });
      const disk = JSON.parse(await readFile(path.join(dataDir, 'state.json'), 'utf8'));
      expect(disk.botDeliveries[0].report).toEqual(cancelled);

      // Free the global slot and observe several scheduler polls: no follow-up appears.
      blocker.release({ text: '정기 점검을 마쳤습니다.' });
      await expect.poll(async () => (await snapshot(page)).routineRuns[0]?.result?.status).toBe('completed');
      await page.waitForTimeout(1500);
      expect(stage).toBe(5);
      expect((await snapshot(page)).conversations.find(conversation => conversation.id === source.id)).toEqual(source);
      await page.reload();
      await expect(reportStatus).toHaveText('취소됨');

      const route = new URL(page.url()).search;
      await app.stop();
      app = await startTestApp({ env: { GROK_BOT_DATA_DIR: dataDir } });
      await page.goto(app.url + route);
      await expect(reportStatus).toHaveText('취소됨');
      await expect(cancel).toHaveCount(0);
      await page.waitForTimeout(1500);
      const restored = await snapshot(page);
      expect(restored.botDeliveries![0].report).toEqual(cancelled);
      expect(restored.conversations.find(conversation => conversation.id === source.id)).toEqual(source);
      expect(stage).toBe(5);
      await log.getByRole('button', { name: 'Reviewer와의 대화 열기', exact: true }).click();
      await expect(page.getByRole('log', { name: '봇 간 대화 메시지', exact: true })).toContainText('이어서 답하기를 중단했습니다.');
      expect(errors).toEqual([]);
    } finally {
      recipient.release({ text: '테스트 종료' });
      blocker.release({ text: '테스트 종료' });
      await app.stop();
      await provider.close();
      if (path.dirname(dataDir) !== tmpdir() || !path.basename(dataDir).startsWith('grok-bot-report-cancel-')) throw Error('Unexpected test data path');
      await rm(dataDir, { recursive: true, force: true });
    }
  });
}
