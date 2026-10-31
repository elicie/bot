import { test, expect, chromium, type Page } from '@playwright/test';
import { startTestApp } from './app-fixture';
import { startMockProvider } from './mock-provider';
import type { AppState } from '../shared/types';

const snapshot = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());

test('queued routine cancellation keeps the active chat intact and form validation retains input', async () => {
  const app = await startTestApp({ network: true });
  const provider = await startMockProvider({ delayMs: 20_000 });
  const browser = await chromium.launch({ channel: 'chrome', headless: true, args: ['--no-sandbox', '--no-proxy-server', '--host-resolver-rules=MAP grok-bot.test 127.0.0.1'] });
  const page = await browser.newPage();
  try {
    await page.goto(app.url);
    await page.evaluate(async baseUrl => {
      const response = await fetch('/api/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ baseUrl, model: 'local-test-model', workspace: '' }) });
      if (!response.ok) throw new Error(await response.text());
    }, provider.baseUrl);
    await page.getByLabel('메시지', { exact: true }).fill('대기열 검증용 대화');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    await expect.poll(async () => (await snapshot(page)).conversations.some(item => ['starting', 'running'].includes(item.run.status))).toBe(true);
    const activeId = (await snapshot(page)).conversations[0].id;
    await page.getByRole('button', { name: '루틴', exact: true }).click();
    await page.getByRole('button', { name: '루틴 만들기', exact: true }).first().click();
    await page.getByLabel('실행할 지시', { exact: true }).fill('대기 중에는 이 지시를 보존해 줘');
    await page.getByRole('button', { name: '루틴 저장', exact: true }).click();
    await expect(page.getByRole('alert')).toContainText('루틴 이름을 입력해 주세요.');
    await page.getByLabel('루틴 이름', { exact: true }).fill('대기열 루틴');
    await page.getByLabel('실행할 지시', { exact: true }).fill('대기 중에는 이 지시를 보존해 줘');
    await page.getByLabel('실행 일정', { exact: true }).selectOption('interval');
    await page.getByLabel('반복 간격(분)', { exact: true }).fill('0');
    await page.getByRole('button', { name: '루틴 저장', exact: true }).click();
    await expect(page.getByRole('alert')).toContainText('반복 간격은 1~10080');
    await expect(page.getByLabel('루틴 이름', { exact: true })).toHaveValue('대기열 루틴');
    await page.getByLabel('반복 간격(분)', { exact: true }).fill('60');
    await page.getByRole('button', { name: '루틴 저장', exact: true }).click();
    await page.getByRole('button', { name: '지금 실행', exact: true }).click();
    await expect.poll(async () => (await snapshot(page)).routineRuns.length).toBe(1);
    const run = (await snapshot(page)).routineRuns[0];
    await expect(page.getByRole('button', { name: '루틴 삭제', exact: true })).toBeDisabled();
    await page.getByRole('button', { name: '대화 보기', exact: true }).first().click();
    await expect(page.getByText('대기 중에는 이 지시를 보존해 줘', { exact: true })).toBeVisible();
    await page.getByLabel('메시지', { exact: true }).fill('전송이 차단되어야 하는 초안');
    await expect(page.getByRole('button', { name: '메시지 보내기', exact: true })).toHaveCount(0);
    await page.getByLabel('메시지', { exact: true }).press('Enter');
    expect((await snapshot(page)).conversations.find(item => item.id === run.conversationId)?.messages).toHaveLength(0);
    await page.getByRole('button', { name: '작업 중단', exact: true }).click();
    await expect.poll(async () => (await snapshot(page)).routineRuns[0].result?.status).toBe('cancelled');
    expect((await snapshot(page)).conversations.find(item => item.id === activeId)?.run.status).toMatch(/starting|running/);
    expect((await snapshot(page)).conversations.find(item => item.id === run.conversationId)?.messages).toHaveLength(0);
    await page.getByRole('button', { name: '실행 중인 대화 보기', exact: true }).click();
    await page.getByRole('button', { name: '작업 중단', exact: true }).click();
    await expect.poll(async () => (await snapshot(page)).conversations.find(item => item.id === activeId)?.run.status).toBe('cancelled');
    await page.getByRole('button', { name: '루틴', exact: true }).click();
    await expect(page.getByRole('button', { name: '루틴 삭제', exact: true })).toBeEnabled();
  } finally {
    await browser.close();
    await app.stop();
    await provider.close();
  }
});
