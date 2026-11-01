import { test, expect, type Page } from '@playwright/test';
import { startTestApp } from './app-fixture';
import { startMockProvider } from './mock-provider';
import type { AppState, Conversation } from '../shared/types';

const snapshot = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());

async function api<T>(page: Page, method: string, path: string, body?: unknown): Promise<T> {
  return page.evaluate(async ({ method, path, body }) => {
    const response = await fetch(path, { method, headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: body === undefined ? undefined : JSON.stringify(body) });
    if (!response.ok) throw new Error(`${response.status} ${await response.text()}`);
    return response.json();
  }, { method, path, body });
}

const sidebar = (page: Page) => page.getByRole('complementary', { name: '봇 목록', exact: true });
async function openMenuItem(page: Page, name: string, item: string) {
  if (page.viewportSize()!.width <= 760 && !(await sidebar(page).isVisible())) await page.getByRole('button', { name: '봇 목록 열기', exact: true }).click();
  await sidebar(page).getByRole('button', { name: `추가 작업: ${name}`, exact: true }).click();
  await page.getByRole('menu', { name: `${name} 작업`, exact: true }).getByRole('menuitem', { name: item, exact: true }).click();
}

for (const viewport of [{ width: 1280, height: 860 }, { width: 390, height: 844 }]) {
  test(`bot deletion confirms, removes history, and keeps the last bot at ${viewport.width}px`, async ({ page }, testInfo) => {
    const app = await startTestApp();
    const provider = await startMockProvider({ reply: () => ({ text: '곧 삭제될 답변' }) });
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.setViewportSize(viewport);
    try {
      await page.goto(app.url);
      await api(page, 'PUT', '/api/settings', { baseUrl: provider.baseUrl, model: 'local-test-model', workspace: '' });
      const created = await api<AppState>(page, 'POST', '/api/bots', { name: '삭제할 Bot' });
      const doomed = created.bots.find(bot => bot.name === '삭제할 Bot')!;
      const { conversation } = await api<{ conversation: Conversation }>(page, 'POST', '/api/conversations', { botId: doomed.id });
      await api(page, 'POST', `/api/conversations/${conversation.id}/messages`, { text: '기록을 남겨요' });
      await expect.poll(async () => (await snapshot(page)).conversations.find(item => item.id === conversation.id)?.run.status, { timeout: 30_000 }).toBe('completed');
      await api(page, 'POST', '/api/routines', { botId: doomed.id, name: '삭제될 루틴', prompt: '보고', schedule: { kind: 'interval', minutes: 60 }, enabled: true });

      await openMenuItem(page, '삭제할 Bot', 'Bot 삭제');
      const dialog = page.getByRole('dialog', { name: '삭제할 Bot 삭제', exact: true });
      await expect(dialog.getByRole('heading', { name: '“삭제할 Bot”을(를) 삭제할까요?', exact: true })).toBeVisible();
      await expect(dialog).toContainText('이 Bot과 채팅 기록이 영구적으로 삭제돼요. 되돌릴 수 없어요.');
      await expect(dialog).toContainText('대화 1개와 첨부 파일');
      await expect(dialog).toContainText('루틴 1개와 실행 기록');
      await page.screenshot({ path: testInfo.outputPath(`bot-delete-${viewport.width}.png`) });
      await dialog.getByRole('button', { name: '취소', exact: true }).click();
      await expect(dialog).toHaveCount(0);
      expect((await snapshot(page)).bots.some(bot => bot.id === doomed.id)).toBe(true);

      await openMenuItem(page, '삭제할 Bot', 'Bot 삭제');
      await dialog.getByRole('button', { name: '삭제', exact: true }).click();
      await expect(dialog).toHaveCount(0);
      const after = await snapshot(page);
      expect(after.bots.map(bot => bot.name)).toEqual(['Grok']);
      expect(after.conversations.some(item => item.botId === doomed.id)).toBe(false);
      expect(after.routines).toEqual([]);
      if (page.viewportSize()!.width <= 760 && !(await sidebar(page).isVisible())) await page.getByRole('button', { name: '봇 목록 열기', exact: true }).click();
      await expect(sidebar(page).getByRole('button', { name: /^삭제할 Bot/ })).toHaveCount(0);

      // The last bot cannot be deleted.
      await openMenuItem(page, 'Grok', 'Bot 삭제');
      await expect(dialog.or(page.getByRole('dialog', { name: 'Grok 삭제', exact: true }))).toContainText('마지막 Bot은 삭제할 수 없어요.');
      await expect(page.getByRole('dialog', { name: 'Grok 삭제', exact: true }).getByRole('button', { name: '삭제', exact: true })).toBeDisabled();
      await page.keyboard.press('Escape');
      await expect(page.getByRole('dialog', { name: 'Grok 삭제', exact: true })).toHaveCount(0);
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
      expect(errors).toEqual([]);
    } finally {
      await app.stop();
      await provider.close();
    }
  });
}
