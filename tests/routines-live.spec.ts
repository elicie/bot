import { test, expect, chromium } from '@playwright/test';
import { startTestApp } from './app-fixture';
import { startMockProvider } from './mock-provider';
import type { AppState } from '../shared/types';

test('a scheduled one-time routine runs automatically while its browser history stays consistent', async () => {
  const app = await startTestApp({ network: true });
  const provider = await startMockProvider({ reply: request => ({ text: request.tools?.some(tool => tool.function.name === 'search_tool') ? '예약된 루틴 응답입니다.' : 'Scheduled routine' }) });
  const browser = await chromium.launch({ channel: 'chrome', headless: true, args: ['--no-sandbox', '--no-proxy-server', '--host-resolver-rules=MAP grok-bot.test 127.0.0.1'] });
  const page = await browser.newPage();
  try {
    await page.goto(app.url);
    await page.getByLabel('메시지', { exact: true }).fill('전송 전 대화 초안');
    await page.getByRole('button', { name: '봇 추가', exact: true }).click();
    await page.getByLabel('봇 이름', { exact: true }).fill('두 번째 봇');
    await page.getByRole('dialog', { name: '봇 추가', exact: true }).getByRole('button', { name: '봇 추가', exact: true }).click();
    await page.getByRole('complementary', { name: '봇 목록', exact: true }).getByRole('button', { name: /^Grok/ }).click();
    await expect(page.getByLabel('메시지', { exact: true })).toHaveValue('전송 전 대화 초안');
    await page.getByRole('button', { name: '루틴', exact: true }).click();
    const configured = await page.evaluate(async ({ baseUrl, workspace }) => {
      const initial = await (await fetch('/api/state')).json();
      const command = async (url: string, body: unknown, method = 'POST') => {
        const response = await fetch(url, { method, headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify(body) });
        if (!response.ok) throw new Error(await response.text());
        return response.json();
      };
      await command('/api/settings', { baseUrl, model: 'local-test-model', workspace }, 'PUT');
      return command('/api/routines', { name: '자동 예약 검증', botId: initial.bots[0].id, prompt: '예약 작업을 실행해 줘', enabled: true, schedule: { kind: 'once', at: new Date(Date.now() + 1500).toISOString() } });
    }, { baseUrl: provider.baseUrl, workspace: app.workspace }) as AppState;
    const routine = configured.routines.find(item => item.name === '자동 예약 검증')!;
    await expect(page.getByRole('button', { name: /자동 예약 검증/ })).toBeVisible();
    await page.getByRole('button', { name: /자동 예약 검증/ }).click();
    await expect.poll(async () => page.evaluate(async id => {
      const snapshot = await (await fetch('/api/state')).json() as AppState;
      return snapshot.routineRuns.find(run => run.routineId === id)?.result?.status;
    }, routine.id), { timeout: 30_000 }).toBe('completed');
    const snapshot = await page.evaluate(async () => (await fetch('/api/state')).json()) as AppState;
    expect(snapshot.routineRuns.filter(run => run.routineId === routine.id)).toHaveLength(1);
    expect(snapshot.routineRuns[0].trigger).toBe('scheduled');
    expect(snapshot.routines.find(item => item.id === routine.id)?.enabled).toBe(false);
    await page.getByRole('complementary', { name: '봇 목록', exact: true }).getByRole('button', { name: /^두 번째 봇/ }).click();
    await page.getByRole('complementary', { name: '봇 목록', exact: true }).getByRole('button', { name: /^Grok/ }).click();
    await expect(page.getByLabel('메시지', { exact: true })).toHaveValue('전송 전 대화 초안');
    expect(new URL(page.url()).hash).not.toContain('conversation=');
    await page.getByRole('button', { name: '루틴', exact: true }).click();
    await page.getByRole('button', { name: /자동 예약 검증/ }).click();
    await page.getByRole('button', { name: '대화 보기', exact: true }).first().click();
    await expect(page.getByRole('log').getByText('예약된 루틴 응답입니다.', { exact: true })).toBeVisible();
    await page.reload();
    await expect(page.getByRole('article', { name: '내 메시지', exact: true }).getByText('예약 작업을 실행해 줘', { exact: true })).toBeVisible();
    await page.getByRole('button', { name: '루틴', exact: true }).click();
    await expect(page.getByText('자동 예약 검증', { exact: true }).first()).toBeVisible();
  } finally {
    await browser.close();
    await app.stop();
    await provider.close();
  }
});
