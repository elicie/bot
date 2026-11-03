import { randomUUID } from 'node:crypto';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
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
    // Clicking the recipient bot opens its own chat, not the newer conversation another bot's message opened.
    await page.getByRole('complementary', { name: '봇 목록', exact: true }).getByRole('button', { name: /^Reviewer/ }).first().click();
    await expect(page.getByRole('heading', { name: 'Reviewer', exact: true })).toBeVisible();
    await expect(page.getByRole('note', { name: '다른 Bot의 요청', exact: true })).toHaveCount(0);
    await expect(page.getByLabel('메시지', { exact: true })).toBeEnabled();
    expect(stage).toBe(5);
  } finally { await app.stop(); await provider.close(); }
});

for (const viewport of [{ width: 1040, height: 760, startFirst: false }, { width: 390, height: 844, startFirst: false }, { width: 1040, height: 760, startFirst: true }, { width: 390, height: 844, startFirst: true }]) {
  test(`${viewport.startFirst ? 'scheduler-first cancellation reports a no-op' : 'queued cancellation survives restart'} at ${viewport.width}px`, async ({ page }, testInfo) => {
    const dataDir = await mkdtemp(path.join(tmpdir(), 'grok-bot-report-cancel-'));
    let app = await startTestApp({ env: { GROK_BOT_DATA_DIR: dataDir } });
    let recipientId = '';
    let stage = 0;
    const recipient = gate();
    const blocker = gate();
    const resumedBlocker = gate();
    const report = gate();
    let slotBlocker = blocker;
    const provider = await startMockProvider({ reply: request => {
      if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: '검토 요청' };
      const replies: Array<MockCompletion | Promise<MockCompletion>> = [
        { tool: { name: 'search_tool', arguments: { query: 'workspace send_message', limit: 3 } } },
        { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__send_message', tool_input: { toBotId: recipientId, text: '계획을 검토해 주세요.' } } } },
        { text: 'Reviewer에게 검토를 요청했습니다.' },
        recipient.promise,
        slotBlocker.promise,
        report.promise,
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

      // Issue #20: another Bot's routine cannot block the sender. Occupy the actual sender Bot
      // before its report arrives, preserving a real Bot-local FIFO boundary without state overrides.
      const senderId = (await snapshot(page)).botDeliveries![0].fromBotId;
      const scheduled = await api<AppState>(page, '/api/routines', { botId: senderId, name: '정기 점검', prompt: '다른 작업을 처리합니다.', enabled: false, schedule: { kind: 'interval', minutes: 60 } });
      await api(page, `/api/routines/${scheduled.routines[0].id}/run`, {});
      await expect.poll(() => stage, { timeout: 60_000 }).toBe(5);
      recipient.release({ text: '계획을 검토했습니다. 진행해도 좋습니다.' });
      await expect.poll(async () => (await snapshot(page)).botDeliveries?.[0]?.report?.status).toBe('queued');
      const before = await snapshot(page);
      const delivery = before.botDeliveries![0];
      const source = before.conversations.find(conversation => conversation.id === delivery.sourceConversationId)!;
      if (viewport.startFirst) {
        // Seed a loader-validated synthetic long history, not a mocked DOM. Its older selected
        // report stays queued behind an older routine after restarting the actual server.
        const seeded = JSON.parse(await readFile(path.join(dataDir, 'state.json'), 'utf8'));
        const history = seeded.conversations.find((conversation: AppState['conversations'][number]) => conversation.id === source.id);
        for (let index = 0; index < 25; index++) {
          for (const role of ['user', 'assistant']) history.messages.push({ id: randomUUID(), role, text: `이미 이어진 대화 ${index + 1}: 추가 검토 내용입니다.`, createdAt: source.updatedAt, tools: [] });
        }
        for (const conversation of seeded.conversations) { delete conversation.sessionId; delete conversation.sessionFingerprint; }
        const routine = seeded.routineRuns[0];
        delete routine.executionRunId;
        delete routine.result;
        const routineConversation = seeded.conversations.find((conversation: AppState['conversations'][number]) => conversation.id === routine.conversationId);
        routineConversation.messages = [];
        routineConversation.run = { id: '', status: 'idle' };
        // The accepted FIFO, not timestamps, is canonical. Restore the older routine's queue
        // pointer alongside its reset receipt so the selected report remains behind it on reopen.
        seeded.workQueue = [
          { id: randomUUID(), botId: routine.botId, conversationId: routine.conversationId, createdAt: routine.createdAt, kind: 'routine', runId: routine.id },
          ...(seeded.workQueue ?? []).filter((work: { kind: string }) => work.kind !== 'routine'),
        ];
        blocker.release({ text: '이전 fixture 종료' });
        await app.stop();
        await writeFile(path.join(dataDir, 'state.json'), JSON.stringify(seeded), { mode: 0o600 });
        slotBlocker = resumedBlocker;
        stage = 4;
        app = await startTestApp({ env: { GROK_BOT_DATA_DIR: dataDir } });
        await page.goto(`${app.url}/?bot=${source.botId}&conversation=${source.id}`);
        await expect.poll(() => stage, { timeout: 60_000 }).toBe(5);
      }
      const log = page.getByRole('log', { name: '대화 메시지', exact: true });
      const reportStatus = log.getByLabel('Reviewer 답변 이어서 답하기 상태', { exact: true });
      const cancel = log.getByRole('button', { name: 'Reviewer 답변: 이 보고 취소', exact: true });
      await expect(log.getByLabel('Reviewer 답변 상태', { exact: true })).toHaveText('답변 도착');
      await expect(reportStatus).toHaveText('대기 중');
      await expect(cancel).toBeEnabled();
      await cancel.scrollIntoViewIfNeeded();
      if (viewport.startFirst) expect(await log.evaluate(element => element.scrollHeight - element.scrollTop - element.clientHeight > 500)).toBe(true);
      expect(await cancel.textContent()).toBe('이 보고 취소');
      await expect(reportStatus).toHaveAttribute('role', 'status');
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
      await page.route(endpoint, async route => {
        expect(route.request().postDataJSON()).toEqual({ deliveryIds: [delivery.id] });
        await inFlight;
        await route.continue();
      }, { times: 1 });
      await cancel.focus();
      await page.keyboard.press('Enter');
      try {
        await expect(cancel).toBeDisabled();
        await expect(reportStatus).toHaveText('대기 중');
        if (viewport.startFirst) {
          slotBlocker.release({ text: '정기 점검을 마쳤습니다.' });
          await expect.poll(() => stage, { timeout: 60_000 }).toBe(6);
          await expect(reportStatus).toHaveText('답변 중');
        }
      } finally { releaseRequest(); }
      if (viewport.startFirst) {
        const notice = page.getByRole('status', { name: '보고 취소 결과', exact: true });
        await expect(notice).toContainText('취소한 보고가 없습니다.');
        await expect(notice).toBeInViewport();
        await expect(reportStatus).toBeInViewport();
        expect(await log.evaluate(element => element.scrollHeight - element.scrollTop - element.clientHeight > 500)).toBe(true);
        await page.screenshot({ path: testInfo.outputPath(`report-noop-scrolled-${viewport.width}.png`) });
        await page.getByRole('button', { name: '보고 취소 안내 닫기', exact: true }).click();
        await expect(notice).toHaveCount(0);
        await expect(reportStatus).not.toHaveAttribute('role', 'status');
        await expect(reportStatus).toHaveAttribute('aria-live', 'off');
        await expect(cancel).toHaveCount(0);
        report.release({ text: '이미 시작한 후속 답변은 계속 실행됩니다.' });
        await expect(reportStatus).toHaveText('완료');
        expect((await snapshot(page)).botDeliveries![0].report!.status).toBe('completed');
        expect(errors).toEqual([]);
        return;
      }
      await expect(reportStatus).toHaveText('취소됨');
      await expect(reportStatus).not.toHaveAttribute('role', 'status');
      await expect(reportStatus).toHaveAttribute('aria-live', 'off');
      await expect(cancel).toHaveCount(0);
      const cancelled = (await snapshot(page)).botDeliveries![0].report!;
      expect(cancelled).toMatchObject({ status: 'cancelled', finishedAt: expect.any(String) });
      expect(cancelled.runId).toBeUndefined();
      expect((await snapshot(page)).conversations.find(conversation => conversation.id === source.id)).toEqual(source);
      await page.screenshot({ path: testInfo.outputPath(`report-cancelled-${viewport.width}.png`) });
      const disk = JSON.parse(await readFile(path.join(dataDir, 'state.json'), 'utf8'));
      expect(disk.botDeliveries[0].report).toEqual(cancelled);

      // Free the sender Bot's slot and observe several scheduler polls: no follow-up appears.
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
      resumedBlocker.release({ text: '테스트 종료' });
      report.release({ text: '테스트 종료' });
      await app.stop();
      await provider.close();
      if (path.dirname(dataDir) !== tmpdir() || !path.basename(dataDir).startsWith('grok-bot-report-cancel-')) throw Error('Unexpected test data path');
      await rm(dataDir, { recursive: true, force: true });
    }
  });
}

async function chainApi<T>(page: Page, method: string, path: string, body?: unknown): Promise<T> {
  return page.evaluate(async ({ method, path, body }) => {
    const response = await fetch(path, { method, headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: body === undefined ? undefined : JSON.stringify(body) });
    if (!response.ok) throw new Error(`${response.status} ${await response.text()}`);
    return response.json();
  }, { method, path, body });
}

async function chainBots(page: Page, baseUrl: string, workspace = '') {
  await chainApi(page, 'PUT', '/api/settings', { baseUrl, model: 'local-test-model', workspace });
  for (const name of ['Chain B', 'Chain C']) await chainApi(page, 'POST', '/api/bots', { name, instructions: 'Review public fixture data only.' });
  const state = await snapshot(page);
  return { b: state.bots.find(bot => bot.name === 'Chain B')!.id, c: state.bots.find(bot => bot.name === 'Chain C')!.id };
}

async function approveChainTurn(page: Page, conversationId: string) {
  await expect.poll(async () => (await snapshot(page)).conversations.find(conversation => conversation.id === conversationId)?.run.status, { timeout: 60_000 }).toBe('awaiting_permission');
  const permission = (await snapshot(page)).conversations.find(conversation => conversation.id === conversationId)!.run.permission!;
  await chainApi(page, 'POST', `/api/conversations/${conversationId}/permissions/${permission.id}`, { optionId: permission.options.find(option => option.kind === 'allow_once')!.id });
}

test('UI contract only: Enter accepts Bot-local FIFO work and Stop never targets another Bot or a waiting chain', async ({ page }) => {
  // This deliberately routed snapshot is a client interaction regression, NOT native runtime or
  // screenshot evidence. The production scheduler cannot be paused over HTTP/SSE; the real-engine
  // manual-scheduler regression checks acceptance and the eventual single report separately.
  await page.setViewportSize({ width: 390, height: 844 });
  const app = await startTestApp();
  let releaseEvents!: () => void;
  const eventsHeld = new Promise<void>(resolve => { releaseEvents = resolve; });
  const sent: unknown[] = [];
  const cancelled: string[] = [];
  try {
    await page.goto(app.url);
    const bots = await chainBots(page, 'http://127.0.0.1:1/v1');
    const initial = await snapshot(page);
    const a = initial.bots[0].id;
    const source = (await chainApi<{ conversation: AppState['conversations'][number] }>(page, 'POST', '/api/conversations', { botId: a })).conversation;
    const recipient = (await chainApi<{ conversation: AppState['conversations'][number] }>(page, 'POST', '/api/conversations', { botId: bots.b })).conversation;
    const other = (await chainApi<{ conversation: AppState['conversations'][number] }>(page, 'POST', '/api/conversations', { botId: bots.c })).conversation;
    const fixture = await snapshot(page);
    const now = new Date().toISOString();
    const sourceFixture = fixture.conversations.find(conversation => conversation.id === source.id)!;
    sourceFixture.messages = [
      { id: 'fixture-original-user', role: 'user', text: 'Ask B for a review.', tools: [], createdAt: now },
      { id: 'fixture-original-answer', role: 'assistant', text: 'B has the review request.', tools: [], createdAt: now, botExchangeIds: ['fixture-exchange'] },
    ];
    sourceFixture.run = { id: 'fixture-completed-run', status: 'completed', finishedAt: now };
    fixture.botDeliveries = [{
      id: 'fixture-delivery', chainId: 'fixture-delivery', hop: 1, exchangeId: 'fixture-exchange',
      sourceConversationId: source.id, sourceMessageId: 'fixture-original-answer', recipientConversationId: recipient.id,
      fromBotId: a, toBotId: bots.b, text: 'Review the public fixture.', createdAt: now, status: 'queued',
    }];
    fixture.botExchanges = [{
      id: 'fixture-exchange', botIds: [a, bots.b], sourceConversationId: source.id, sourceMessageId: 'fixture-original-answer', updatedAt: now,
      messages: [{ id: 'fixture-delivery', botId: a, text: 'Review the public fixture.', createdAt: now }],
    }];
    const queued = structuredClone(fixture.botDeliveries[0]);
    fixture.workQueue = [{ id: 'fixture-delivery-work', botId: bots.b, conversationId: recipient.id, createdAt: now, kind: 'delivery', deliveryId: queued.id }];
    const otherFixture = fixture.conversations.find(conversation => conversation.id === other.id)!;
    otherFixture.run = { id: 'fixture-unrelated-run', botId: bots.c, status: 'running', startedAt: now };
    const unrelatedRun = structuredClone(otherFixture.run);
    fixture.revision += 100;
    await page.route('**/api/state', route => route.fulfill({ json: fixture }));
    await page.route('**/api/events', async route => { await eventsHeld; await route.abort(); });
    await page.route(`**/api/conversations/${source.id}/cancel`, async route => {
      cancelled.push(route.request().url());
      fixture.revision++;
      sourceFixture.run = { ...sourceFixture.run, status: 'cancelled', finishedAt: now };
      fixture.workQueue = fixture.workQueue!.filter(work => work.conversationId !== source.id);
      await route.fulfill({ json: fixture });
    });
    await page.route(`**/api/conversations/${source.id}/messages`, async route => {
      const body = route.request().postDataJSON() as { text: string };
      sent.push(body);
      fixture.revision++;
      if (sent.length === 1) {
        sourceFixture.messages.push(
          { id: 'fixture-separate-user', role: 'user', text: body.text, tools: [], createdAt: now },
          { id: 'fixture-separate-answer', role: 'assistant', text: '', tools: [], createdAt: now },
        );
        sourceFixture.run = { id: 'fixture-own-active-run', botId: a, status: 'running', startedAt: now };
      } else {
        fixture.workQueue!.push({ id: `fixture-user-work-${sent.length}`, botId: a, conversationId: source.id, createdAt: now, kind: 'user', text: body.text, attachments: [] });
      }
      await route.fulfill({ json: fixture });
    });
    const url = `${app.url}/?bot=${a}&conversation=${source.id}`;
    await page.goto(url, { waitUntil: 'domcontentloaded' });
    const input = page.getByLabel('메시지', { exact: true });
    const send = page.getByRole('button', { name: '메시지 보내기', exact: true });
    const stop = page.getByRole('button', { name: '작업 중단', exact: true });
    await input.fill('A separate user question.');
    // Revised rule 7: B's queued delivery is not A's Stop scope, and live C does not disable A.
    await expect(page.getByLabel('Chain B 답변 상태', { exact: true })).toHaveText('대기 중');
    await expect(stop).toHaveCount(0);
    await expect(page.getByRole('button', { name: 'Bot 체인 중단', exact: true })).toHaveCount(0);
    await expect(send).toBeEnabled();
    await input.press('Enter');
    await expect.poll(() => sent).toEqual([{ text: 'A separate user question.' }]);
    await expect(input).toHaveValue('');
    await expect(stop).toBeVisible();
    await expect(stop).toHaveAttribute('title', '이 대화의 실행과 대기 중인 작업만 중단');
    await expect(send).toHaveCount(0);
    const live = structuredClone(sourceFixture);
    for (const message of ['Queued second question.', 'Queued third question.']) {
      await input.fill(message);
      await expect(send).toBeEnabled();
      await expect(send).toHaveAttribute('title', '메시지 보내기 · 대기열에 추가');
      await input.press('Enter');
      await expect(input).toHaveValue('');
    }
    const pending = page.getByRole('region', { name: '대기 중인 메시지', exact: true });
    await expect(pending.locator('li[data-queued-turn-id] p')).toHaveText(['Queued second question.', 'Queued third question.']);
    expect(sourceFixture).toEqual(live); // Accepted queued turns are not hidden/live transcript input.
    expect(fixture.workQueue!.filter(work => work.conversationId === source.id).map(work => work.id)).toEqual(['fixture-user-work-2', 'fixture-user-work-3']);
    expect(cancelled).toEqual([]);
    expect(fixture.botDeliveries).toEqual([queued]);
    expect(otherFixture.run).toEqual(unrelatedRun);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    await stop.click();
    await expect(stop).toHaveCount(0);
    await expect(pending).toHaveCount(0);
    expect(cancelled).toEqual([`${app.url}/api/conversations/${source.id}/cancel`]);
    expect(sourceFixture.messages).toEqual(live.messages);
    expect(fixture.botDeliveries).toEqual([queued]);
    expect(otherFixture.run).toEqual(unrelatedRun);
    expect(fixture.workQueue).toEqual([{ id: 'fixture-delivery-work', botId: bots.b, conversationId: recipient.id, createdAt: now, kind: 'delivery', deliveryId: queued.id }]);
  } finally { releaseEvents(); await app.stop(); }
});

test('a nested review waits for C, then reports B and A exactly once with hidden report inputs on mobile', async ({ page }) => {
  test.setTimeout(120_000);
  await page.setViewportSize({ width: 390, height: 844 });
  const app = await startTestApp();
  let bots = { b: '', c: '' };
  let stage = 0;
  const reportInputs: string[] = [];
  let releaseC!: () => void;
  const cGate = new Promise<void>(resolve => { releaseC = resolve; });
  const provider = await startMockProvider({ reply: async request => {
    if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: 'Nested report fixture' };
    switch (stage++) {
      case 0: return { tool: { name: 'search_tool', arguments: { query: 'workspace send_message', limit: 3 } } };
      case 1: return { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__send_message', tool_input: { toBotId: bots.b, text: 'Ask C for a final review.' } } } };
      case 2: return { text: 'A asked Chain B to review.' };
      case 3: return { tool: { name: 'search_tool', arguments: { query: 'workspace send_message', limit: 3 } } };
      case 4: return { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__send_message', tool_input: { toBotId: bots.c, text: 'Give the final review verdict.' } } } };
      case 5: return { text: 'B_DELEGATED_ONLY: I asked C; this is not the final review.' };
      case 6: await cGate; return { text: 'C_FINAL_CANARY: the public fixture plan is approved.' };
      case 7:
        reportInputs.push(request.messages.filter(message => message.role === 'user').map(message => text(message.content)).join('\n'));
        return { text: 'B_SUMMARY: C reviewed the plan and approved it.' };
      case 8:
        reportInputs.push(request.messages.filter(message => message.role === 'user').map(message => text(message.content)).join('\n'));
        return { text: 'A_SUMMARY: B and C approved the plan; we can proceed.' };
      default: return { text: 'Unexpected repeated follow-up.' };
    }
  } });
  try {
    await page.goto(app.url);
    bots = await chainBots(page, provider.baseUrl);
    await page.getByLabel('메시지', { exact: true }).fill('Ask B and C for a nested review.');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    const permission = page.getByRole('region', { name: '작업 승인 요청', exact: true });
    await expect(permission).toContainText('Ask C for a final review.');
    await permission.getByRole('button', { name: 'Send once', exact: true }).click();
    await expect.poll(async () => (await snapshot(page)).botDeliveries?.length ?? 0, { timeout: 60_000 }).toBe(1);
    const root = (await snapshot(page)).botDeliveries![0];
    await approveChainTurn(page, root.recipientConversationId);
    await expect.poll(() => stage, { timeout: 60_000 }).toBe(7);

    const waiting = await snapshot(page);
    const leaf = waiting.botDeliveries!.find(delivery => delivery.hop === 2)!;
    expect(waiting.botDeliveries!.find(delivery => delivery.id === root.id)!.status).toBe('waiting_children');
    expect(waiting.botDeliveries!.find(delivery => delivery.id === root.id)!.report).toBeUndefined();
    expect(waiting.botExchanges!.find(exchange => exchange.id === root.exchangeId)!.messages).toHaveLength(1);
    expect(waiting.conversations.find(conversation => conversation.id === root.sourceConversationId)!.messages).toHaveLength(2);
    const log = page.getByRole('log', { name: '대화 메시지', exact: true });
    await expect(log.getByLabel('Chain B 답변 상태', { exact: true })).toHaveText('하위 답변 대기 중');
    await expect(page.getByText('Bot 답변 대기 중', { exact: true })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Bot 체인 중단', exact: true })).toHaveCount(0);
    await expect(log.getByRole('note', { name: '이어서 답한 Bot 답변', exact: true })).toHaveCount(0);
    await page.getByLabel('메시지', { exact: true }).fill('A separate draft while the review is pending.');
    // Revised issue #20 scope: C occupies only C's slot. A can send a new turn, while a waiting
    // outbound chain alone is neither A's live work nor A's pending work and exposes no local Stop.
    await expect(page.getByRole('button', { name: '메시지 보내기', exact: true })).toBeEnabled();
    await expect(page.getByRole('button', { name: '작업 중단', exact: true })).toHaveCount(0);
    for (const width of [320, 390]) {
      await page.setViewportSize({ width, height: 844 });
      await expect(page.getByRole('button', { name: '메시지 보내기', exact: true })).toBeEnabled();
      await expect(page.getByRole('button', { name: '작업 중단', exact: true })).toHaveCount(0);
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    }

    releaseC();
    await expect.poll(async () => (await snapshot(page)).botDeliveries?.find(delivery => delivery.id === root.id)?.report?.status, { timeout: 60_000 }).toBe('completed');
    const final = await snapshot(page);
    const a = final.conversations.find(conversation => conversation.id === root.sourceConversationId)!;
    const b = final.conversations.find(conversation => conversation.id === root.recipientConversationId)!;
    expect(a.messages.map(message => message.role)).toEqual(['user', 'assistant', 'user', 'assistant']);
    expect(b.messages.map(message => message.role)).toEqual(['user', 'assistant', 'user', 'assistant']);
    expect(a.messages[2].reportDeliveryIds).toEqual([root.id]);
    expect(b.messages[2].reportDeliveryIds).toEqual([leaf.id]);
    expect(final.botDeliveries!.find(delivery => delivery.id === root.id)!.executionRunId).toBe(b.run.id);
    expect(reportInputs[0]).toContain('C_FINAL_CANARY');
    expect(reportInputs[1]).toContain('B_SUMMARY');
    expect(final.botExchanges!.find(exchange => exchange.id === root.exchangeId)!.messages.map(message => message.text)).toEqual([root.text, 'B_SUMMARY: C reviewed the plan and approved it.']);
    await expect(log).toContainText('A_SUMMARY');
    await expect(log).not.toContainText('C_FINAL_CANARY');
    await expect(log).not.toContainText('Automatic follow-up');
    await expect(log.getByRole('article', { name: '내 메시지', exact: true })).toHaveCount(1);
    await expect(log.getByRole('note', { name: '이어서 답한 Bot 답변', exact: true })).toHaveCount(1);
    await expect(page.getByRole('button', { name: 'Bot 체인 중단', exact: true })).toHaveCount(0);

    // The same hidden-input/marker contract holds in the recipient's execution conversation.
    await page.goto(`${app.url}/?bot=${bots.b}&conversation=${root.recipientConversationId}`);
    const executionLog = page.getByRole('log', { name: '대화 메시지', exact: true });
    await expect(executionLog).toContainText('B_SUMMARY');
    await expect(executionLog.getByRole('note', { name: '이어서 답한 Bot 답변', exact: true })).toContainText('Chain C의 답변을 받아 이어서 답함');
    await expect(executionLog).not.toContainText('Automatic follow-up');
    await expect(executionLog).not.toContainText('C_FINAL_CANARY');
    await page.reload();
    await expect(executionLog.getByRole('note', { name: '이어서 답한 Bot 답변', exact: true })).toHaveCount(1);
    expect(stage).toBe(9);
    expect((await snapshot(page)).conversations.find(conversation => conversation.id === root.sourceConversationId)!.messages).toHaveLength(4);
  } finally { releaseC(); await app.stop(); await provider.close(); }
});

test('cancelling C follow-up in waiting B reports one non-success to A without a provisional final, including after restart', async ({ page }, testInfo) => {
  test.setTimeout(120_000);
  await page.setViewportSize({ width: 390, height: 844 });
  const dataDir = await mkdtemp(path.join(tmpdir(), 'grok-bot-review-nested-cancel-'));
  let app = await startTestApp({ env: { GROK_BOT_DATA_DIR: dataDir } });
  let bots = { b: '', c: '' };
  let stage = 0;
  let reportInput = '';
  const child = gate();
  const blocker = gate();
  const upstreamBlocker = gate();
  const upstreamPrompt = 'Keep A independently occupied until the selected nested report is cancelled.';
  let upstreamStarted = false;
  const provisional = 'B_PROVISIONAL_NOT_FINAL: I asked C and will summarize later.';
  const pageErrors: string[] = [];
  page.on('pageerror', error => pageErrors.push(error.message));
  const provider = await startMockProvider({ reply: request => {
    if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: 'Nested report cancellation fixture' };
    if (request.messages.some(message => message.role === 'user' && text(message.content).includes(upstreamPrompt))) {
      upstreamStarted = true;
      return upstreamBlocker.promise;
    }
    const replies: Array<MockCompletion | Promise<MockCompletion>> = [
      { tool: { name: 'search_tool', arguments: { query: 'workspace send_message', limit: 3 } } },
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__send_message', tool_input: { toBotId: bots.b, text: 'Ask C and return only the final verdict.' } } } },
      { text: 'A delegated the final review to B.' },
      { tool: { name: 'search_tool', arguments: { query: 'workspace send_message', limit: 3 } } },
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__send_message', tool_input: { toBotId: bots.c, text: 'Give the final verdict for B.' } } } },
      { text: provisional },
      child.promise,
      blocker.promise,
      { text: 'A_CANCELLED_SUMMARY: B could not finish because the pending follow-up was cancelled.' },
    ];
    if (stage === 8) reportInput = request.messages.filter(message => message.role === 'user').map(message => text(message.content)).join('\n');
    return replies[stage++] ?? { text: 'CANCELLED_B_FOLLOW_UP_MUST_NOT_RUN' };
  } });
  try {
    await page.goto(app.url);
    bots = await chainBots(page, provider.baseUrl);
    await page.getByLabel('메시지', { exact: true }).fill('Ask B and C for a final review, not an interim answer.');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    const permission = page.getByRole('region', { name: '작업 승인 요청', exact: true });
    await expect(permission).toContainText('Ask C and return only the final verdict.');
    await permission.getByRole('button', { name: 'Send once', exact: true }).click();
    await expect.poll(async () => (await snapshot(page)).botDeliveries?.length ?? 0, { timeout: 60_000 }).toBe(1);
    const root = (await snapshot(page)).botDeliveries![0];
    await approveChainTurn(page, root.recipientConversationId);
    await expect.poll(() => stage, { timeout: 60_000 }).toBe(7);

    // Issue #20: B's waiting aggregate holds no slot, and a C routine cannot block B's report.
    // Keep B live in an unrelated routine; a separate A routine preserves the queued upstream
    // receipt after cancellation. These are real per-Bot slots, not forged application state.
    const scheduled = await chainApi<AppState>(page, 'POST', '/api/routines', {
      botId: bots.b, name: 'Unrelated FIFO review blocker', prompt: 'Finish an unrelated routine.', enabled: false,
      schedule: { kind: 'interval', minutes: 60 },
    });
    await chainApi(page, 'POST', `/api/routines/${scheduled.routines[0].id}/run`, {});
    await expect.poll(() => stage, { timeout: 60_000 }).toBe(8);
    const upstream = await chainApi<AppState>(page, 'POST', '/api/routines', {
      botId: root.fromBotId, name: 'Independent upstream blocker', prompt: upstreamPrompt, enabled: false,
      schedule: { kind: 'interval', minutes: 60 },
    });
    const upstreamRoutine = upstream.routines.find(routine => routine.name === 'Independent upstream blocker')!;
    await chainApi(page, 'POST', `/api/routines/${upstreamRoutine.id}/run`, {});
    await expect.poll(() => upstreamStarted, { timeout: 60_000 }).toBe(true);
    child.release({ text: 'C_FINAL_CANCEL_CANARY: the plan was approved.' });
    await expect.poll(async () => (await snapshot(page)).botDeliveries?.find(delivery => delivery.hop === 2)?.report?.status).toBe('queued');
    const before = await snapshot(page);
    const leaf = before.botDeliveries!.find(delivery => delivery.hop === 2)!;
    const bBefore = before.conversations.find(conversation => conversation.id === root.recipientConversationId)!;
    const unrelatedRun = before.routineRuns.find(run => run.routineId === scheduled.routines[0].id)!;
    const upstreamRun = before.routineRuns.find(run => run.routineId === upstreamRoutine.id)!;
    const unrelated = before.conversations.find(conversation => conversation.id === unrelatedRun.conversationId)!;
    const independentUpstream = before.conversations.find(conversation => conversation.id === upstreamRun.conversationId)!;
    expect(before.botDeliveries!.find(delivery => delivery.id === root.id)!.status).toBe('waiting_children');
    expect(before.botDeliveries!.find(delivery => delivery.id === root.id)!.report).toBeUndefined();
    expect(before.botExchanges!.find(exchange => exchange.id === root.exchangeId)!.messages).toHaveLength(1);

    await page.goto(`${app.url}/?bot=${bots.b}&conversation=${root.recipientConversationId}`);
    const bLog = page.getByRole('log', { name: '대화 메시지', exact: true });
    const cancel = bLog.getByRole('button', { name: 'Chain C 답변: 이 보고 취소', exact: true });
    await expect(bLog).toContainText(provisional);
    await expect(bLog.getByLabel('Chain C 답변 이어서 답하기 상태', { exact: true })).toHaveText('대기 중');
    await expect(cancel).toBeEnabled();
    await page.setViewportSize({ width: 320, height: 844 });
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    await page.screenshot({ path: testInfo.outputPath('integration-nested-report-queued-320.png') });
    const cancellationResponse = page.waitForResponse(response => response.url().endsWith(`/api/conversations/${root.recipientConversationId}/bot-reports/cancel`) && response.request().method() === 'POST');
    await cancel.click();
    const response = await cancellationResponse;
    expect(response.ok()).toBe(true);
    expect(response.request().postDataJSON()).toEqual({ deliveryIds: [leaf.id] });
    const result = await response.json() as { state: AppState; cancelledDeliveryIds: string[] };
    expect(result.cancelledDeliveryIds).toEqual([leaf.id]);
    expect(result.state.botDeliveries!.find(delivery => delivery.id === leaf.id)!.report?.status).toBe('cancelled');
    await expect(bLog.getByLabel('Chain C 답변 이어서 답하기 상태', { exact: true })).toHaveText('취소됨');
    await expect(cancel).toHaveCount(0);
    const cancelled = await snapshot(page);
    const cancelledRoot = cancelled.botDeliveries!.find(delivery => delivery.id === root.id)!;
    expect(cancelledRoot).toMatchObject({ status: 'cancelled', report: { status: 'queued' } });
    expect(cancelledRoot.chainTermination).toBeUndefined(); // Local report cancellation, not whole-chain Stop.
    expect(cancelled.botDeliveries!.find(delivery => delivery.id === leaf.id)!.report).toMatchObject({ status: 'cancelled', finishedAt: expect.any(String) });
    expect(cancelled.botDeliveries!.find(delivery => delivery.id === leaf.id)!.report!.runId).toBeUndefined();
    expect(cancelled.conversations.find(conversation => conversation.id === bBefore.id)!.messages).toEqual(bBefore.messages);
    expect(cancelled.conversations.find(conversation => conversation.id === bBefore.id)!.run.status).toBe('cancelled');
    expect(cancelled.conversations.find(conversation => conversation.id === unrelated.id)!.run).toEqual(unrelated.run);
    expect(cancelled.conversations.find(conversation => conversation.id === independentUpstream.id)!.run).toEqual(independentUpstream.run);
    expect(cancelled.botExchanges!.find(exchange => exchange.id === root.exchangeId)!.messages).toHaveLength(1);
    expect(stage).toBe(8);

    blocker.release({ text: 'The unrelated routine completed normally.' });
    upstreamBlocker.release({ text: 'The independent upstream routine completed normally.' });
    await expect.poll(async () => (await snapshot(page)).botDeliveries?.find(delivery => delivery.id === root.id)?.report?.status, { timeout: 60_000 }).toBe('completed');
    const final = await snapshot(page);
    const aFinal = final.conversations.find(conversation => conversation.id === root.sourceConversationId)!;
    const bFinal = final.conversations.find(conversation => conversation.id === root.recipientConversationId)!;
    expect(aFinal.messages.map(message => message.role)).toEqual(['user', 'assistant', 'user', 'assistant']);
    expect(aFinal.messages[2].reportDeliveryIds).toEqual([root.id]);
    expect(aFinal.messages[3].text).toContain('A_CANCELLED_SUMMARY');
    expect(reportInput).toMatch(/"status"\s*:\s*"cancelled"/);
    expect(reportInput).toMatch(/"answer"\s*:\s*null/);
    expect(reportInput).not.toContain(provisional);
    expect(reportInput).not.toContain('C_FINAL_CANCEL_CANARY');
    expect(bFinal.messages).toEqual(bBefore.messages);
    expect(final.botExchanges!.find(exchange => exchange.id === root.exchangeId)!.messages.map(message => message.text)).toEqual([root.text]);
    expect(final.routineRuns.find(run => run.id === unrelatedRun.id)!.result?.status).toBe('completed');
    expect(final.routineRuns.find(run => run.id === upstreamRun.id)!.result?.status).toBe('completed');
    expect(stage).toBe(9); // A reports once; B never resumes the cancelled C follow-up.

    const sourceUrl = `/?bot=${root.fromBotId}&conversation=${root.sourceConversationId}`;
    await page.goto(app.url + sourceUrl);
    const aLog = page.getByRole('log', { name: '대화 메시지', exact: true });
    await expect(aLog).toContainText('A_CANCELLED_SUMMARY');
    await expect(aLog.getByLabel('Chain B 답변 상태', { exact: true })).toHaveText('중단됨');
    await expect(aLog.getByRole('note', { name: '이어서 답한 Bot 답변', exact: true })).toHaveCount(1);
    await expect(aLog).not.toContainText(provisional);
    await expect(aLog).not.toContainText('Automatic follow-up');
    await page.screenshot({ path: testInfo.outputPath('integration-nested-report-cancelled-320.png') });
    await app.stop();
    app = await startTestApp({ env: { GROK_BOT_DATA_DIR: dataDir } });
    await page.goto(app.url + sourceUrl);
    await expect(aLog).toContainText('A_CANCELLED_SUMMARY');
    await expect(aLog.getByRole('note', { name: '이어서 답한 Bot 답변', exact: true })).toHaveCount(1);
    await page.waitForTimeout(1500); // Cross a real scheduler poll after persisted reopen.
    const restored = await snapshot(page);
    expect(restored.botDeliveries).toEqual(final.botDeliveries);
    expect(restored.botExchanges).toEqual(final.botExchanges);
    expect(restored.conversations.find(conversation => conversation.id === aFinal.id)!.messages).toEqual(aFinal.messages);
    expect(restored.conversations.find(conversation => conversation.id === bFinal.id)!.messages).toEqual(bFinal.messages);
    await page.goto(`${app.url}/?bot=${bots.b}&conversation=${root.recipientConversationId}`);
    await expect(bLog.getByLabel('Chain C 답변 이어서 답하기 상태', { exact: true })).toHaveText('취소됨');
    await expect(cancel).toHaveCount(0);
    expect(stage).toBe(9);
    expect(pageErrors).toEqual([]);
  } finally {
    child.release({ text: 'Fixture cleanup.' });
    blocker.release({ text: 'Fixture cleanup.' });
    upstreamBlocker.release({ text: 'Fixture cleanup.' });
    await app.stop();
    await provider.close();
    if (path.resolve(dataDir) !== dataDir || path.dirname(dataDir) !== tmpdir() || !path.basename(dataDir).startsWith('grok-bot-review-nested-cancel-')) throw Error('Unexpected review test data path');
    await rm(dataDir, { recursive: true, force: true });
  }
});

test('root Stop removes only its queued messages while C approval and independent Bot runs survive until their own Stop', async ({ page }) => {
  test.setTimeout(120_000);
  const app = await startTestApp();
  let bots = { b: '', c: '' };
  let stage = 0;
  const aBlocker = gate();
  const bBlocker = gate();
  const aPrompt = 'Keep A independently live while its other conversation stops queued requests.';
  const bPrompt = 'Keep B independently live until its nested report is cleaned up locally.';
  let aStarted = false;
  let bStarted = false;
  const provider = await startMockProvider({ reply: request => {
    if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: 'Nested Stop fixture' };
    const users = request.messages.filter(message => message.role === 'user').map(message => text(message.content)).join('\n');
    if (users.includes(aPrompt)) { aStarted = true; return aBlocker.promise; }
    if (users.includes(bPrompt)) { bStarted = true; return bBlocker.promise; }
    const replies: MockCompletion[] = [
      { tool: { name: 'search_tool', arguments: { query: 'workspace send_message', limit: 3 } } },
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__send_message', tool_input: { toBotId: bots.b, text: 'Ask C before giving the final verdict.' } } } },
      { text: 'A delegated to B.' },
      { tool: { name: 'search_tool', arguments: { query: 'workspace send_message', limit: 3 } } },
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__send_message', tool_input: { toBotId: bots.c, text: 'Prepare a review note.' } } } },
      { text: 'B delegated to C.' },
      { tool: { name: 'search_tool', arguments: { query: 'workspace send_message', limit: 3 } } },
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__send_message', tool_input: { toBotId: bots.b, text: 'This unapproved follow-up must survive root Stop until C is stopped locally.' } } } },
    ];
    return replies[stage++] ?? { text: 'Unexpected answer after local Stop cleanup.' };
  } });
  try {
    await page.goto(app.url);
    bots = await chainBots(page, provider.baseUrl, app.workspace);
    await page.getByLabel('메시지', { exact: true }).fill('Start a review that I can stop at C.');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    const permission = page.getByRole('region', { name: '작업 승인 요청', exact: true });
    await expect(permission).toContainText('Ask C before giving the final verdict.');
    await permission.getByRole('button', { name: 'Send once', exact: true }).click();
    await expect.poll(async () => (await snapshot(page)).botDeliveries?.length ?? 0, { timeout: 60_000 }).toBe(1);
    const root = (await snapshot(page)).botDeliveries![0];
    await approveChainTurn(page, root.recipientConversationId);
    await expect.poll(async () => {
      const state = await snapshot(page);
      const leaf = state.botDeliveries?.find(delivery => delivery.hop === 2);
      return state.conversations.find(conversation => conversation.id === leaf?.recipientConversationId)?.run.status;
    }, { timeout: 60_000 }).toBe('awaiting_permission');
    const waiting = await snapshot(page);
    const source = waiting.conversations.find(conversation => conversation.id === root.sourceConversationId)!;
    const leaf = waiting.botDeliveries!.find(delivery => delivery.hop === 2)!;
    const cBefore = waiting.conversations.find(conversation => conversation.id === leaf.recipientConversationId)!;
    const stale = cBefore.run.permission!;
    const stop = page.getByRole('button', { name: '작업 중단', exact: true });
    await expect(page.getByLabel('Chain B 답변 상태', { exact: true })).toHaveText('하위 답변 대기 중');
    // WaitingChildren is not an occupied slot or local pending work: there is no root Stop yet.
    await expect(stop).toHaveCount(0);
    await expect(page.getByRole('button', { name: 'Bot 체인 중단', exact: true })).toHaveCount(0);
    for (const [botId, name, prompt] of [[root.fromBotId, 'Independent A', aPrompt], [bots.b, 'Independent B', bPrompt]]) {
      const scheduled = await chainApi<AppState>(page, 'POST', '/api/routines', { botId, name, prompt, enabled: false, schedule: { kind: 'interval', minutes: 60 } });
      const routine = scheduled.routines.find(item => item.name === name)!;
      await chainApi(page, 'POST', `/api/routines/${routine.id}/run`, {});
    }
    await expect.poll(() => aStarted && bStarted, { timeout: 60_000 }).toBe(true);
    const before = await snapshot(page);
    const independentRuns = before.routineRuns.map(run => before.conversations.find(conversation => conversation.id === run.conversationId)!);
    const input = page.getByLabel('메시지', { exact: true });
    for (const message of ['Queued root question one.', 'Queued root question two.']) {
      await input.fill(message);
      await expect(page.getByRole('button', { name: '메시지 보내기', exact: true })).toBeEnabled();
      await input.press('Enter');
      await expect(input).toHaveValue('');
    }
    const queue = page.getByRole('region', { name: '대기 중인 메시지', exact: true });
    await expect(queue.locator('li[data-queued-turn-id] p')).toHaveText(['Queued root question one.', 'Queued root question two.']);
    const accepted = await snapshot(page);
    expect(accepted.conversations.find(conversation => conversation.id === source.id)).toEqual(source);
    expect(accepted.workQueue!.filter(work => work.conversationId === source.id).map(work => work.kind)).toEqual(['user', 'user']);
    await expect(stop).toHaveAttribute('title', '이 대화의 실행과 대기 중인 작업만 중단');
    await stop.click();
    await expect(queue).toHaveCount(0);
    await expect(stop).toHaveCount(0);
    const stopped = await snapshot(page);
    expect(stopped.conversations.find(conversation => conversation.id === source.id)).toEqual(source);
    expect(stopped.conversations.find(conversation => conversation.id === cBefore.id)).toEqual(cBefore);
    expect(stopped.botDeliveries).toEqual(before.botDeliveries);
    expect(stopped.botDeliveries!.find(delivery => delivery.id === root.id)!.chainTermination).toBeUndefined();
    for (const independent of independentRuns) expect(stopped.conversations.find(conversation => conversation.id === independent.id)!.run).toEqual(independent.run);
    expect(stage).toBe(8);

    // C's approval is still actionable after root Stop. Only Stop in C's own execution invalidates it.
    await page.goto(`${app.url}/?bot=${bots.c}&conversation=${leaf.recipientConversationId}`);
    await expect(permission).toContainText('This unapproved follow-up must survive root Stop');
    await expect(stop).toHaveAttribute('title', '이 대화의 실행과 대기 중인 작업만 중단');
    await stop.click();
    await expect.poll(async () => (await snapshot(page)).conversations.find(conversation => conversation.id === cBefore.id)?.run.status, { timeout: 30_000 }).toBe('cancelled');
    const ownStopped = await snapshot(page);
    expect(ownStopped.conversations.find(conversation => conversation.id === cBefore.id)!.run.permission).toBeUndefined();
    for (const independent of independentRuns) expect(ownStopped.conversations.find(conversation => conversation.id === independent.id)!.run).toEqual(independent.run);
    const lateStatus = await page.evaluate(async ({ id, permission }) => (await fetch(`/api/conversations/${id}/permissions/${permission.id}`, { method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ optionId: permission.options[0].id }) })).status, { id: leaf.recipientConversationId, permission: stale });
    expect(lateStatus).toBe(409);
    expect(ownStopped.botDeliveries!.find(delivery => delivery.id === leaf.id)!.report?.status).toBe('queued');
    // Future reports from independent Bots are separate work: remove each via its own conversation,
    // never by reviving the old whole-chain Stop policy. Both unrelated routine runs remain live.
    await chainApi(page, 'POST', `/api/conversations/${root.recipientConversationId}/cancel`, {});
    await chainApi(page, 'POST', `/api/conversations/${source.id}/cancel`, {});
    const cleaned = await snapshot(page);
    expect(cleaned.botDeliveries!.every(delivery => !['queued', 'running', 'waiting_children'].includes(delivery.status) && !['queued', 'running'].includes(delivery.report?.status ?? ''))).toBe(true);
    for (const independent of independentRuns) expect(cleaned.conversations.find(conversation => conversation.id === independent.id)!.run).toEqual(independent.run);
    aBlocker.release({ text: 'A independent routine completed normally.' });
    bBlocker.release({ text: 'B independent routine completed normally.' });
    await expect.poll(async () => (await snapshot(page)).routineRuns.every(run => run.result?.status === 'completed')).toBe(true);
    await page.goto(`${app.url}/?bot=${root.fromBotId}&conversation=${source.id}`);
    await page.reload();
    await expect(stop).toHaveCount(0);
    await expect(page.getByLabel('Chain B 답변 상태', { exact: true })).toHaveText('중단됨');
    await expect(page.getByText('Bot 작업을 중단했습니다.', { exact: true })).toHaveCount(0); // No whole-chain cancellation claim.
    await expect(page.getByRole('note', { name: '이어서 답한 Bot 답변', exact: true })).toHaveCount(0);
    await page.waitForTimeout(1200); // After explicit local cleanup, nothing is replayed on a scheduler poll.
    expect(stage).toBe(8);
    expect((await snapshot(page)).conversations.find(conversation => conversation.id === source.id)!.messages).toEqual(source.messages);
    expect(await readFile(path.join(app.workspace, 'notes.txt'), 'utf8')).toBe('original local content\n');
  } finally {
    aBlocker.release({ text: 'Fixture cleanup.' });
    bBlocker.release({ text: 'Fixture cleanup.' });
    await app.stop();
    await provider.close();
  }
});
