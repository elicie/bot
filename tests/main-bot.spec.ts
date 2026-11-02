import { test, expect, type Page } from '@playwright/test';
import { startTestApp } from './app-fixture';
import type { AppState } from '../shared/types';

const snapshot = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());

async function api<T>(page: Page, method: string, path: string, body?: unknown): Promise<T> {
  return page.evaluate(async ({ method, path, body }) => {
    const response = await fetch(path, { method, headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: body === undefined ? undefined : JSON.stringify(body) });
    if (!response.ok) throw new Error(`${response.status} ${await response.text()}`);
    return response.json();
  }, { method, path, body });
}

const sidebar = (page: Page) => page.getByRole('complementary', { name: '봇 목록', exact: true });
const row = (page: Page, name: string) => sidebar(page).getByRole('button', { name: new RegExp(`^${name}`) });
async function showSidebar(page: Page) {
  if (page.viewportSize()!.width <= 760 && !(await sidebar(page).isVisible())) await page.getByRole('button', { name: '봇 목록 열기', exact: true }).click();
}
async function openMenuItem(page: Page, name: string, item: string) {
  await showSidebar(page);
  await sidebar(page).getByRole('button', { name: `추가 작업: ${name}`, exact: true }).click();
  await page.getByRole('menu', { name: `${name} 작업`, exact: true }).getByRole('menuitem', { name: item, exact: true }).click();
}

for (const viewport of [{ width: 1280, height: 860 }, { width: 390, height: 844 }]) {
  test(`main bot: default, choosing, first load, and deletion at ${viewport.width}px`, async ({ page }, testInfo) => {
    const app = await startTestApp();
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.setViewportSize(viewport);
    try {
      await page.goto(app.url);
      await expect(page.getByRole('heading', { name: 'Grok', exact: true })).toBeVisible();
      await api(page, 'POST', '/api/bots', { name: 'Alpha' });
      const created = await api<AppState>(page, 'POST', '/api/bots', { name: 'Beta' });
      const [grok, , beta] = created.bots;
      expect(created.mainBotId).toBeUndefined();

      // The first bot is the main bot by default, labelled and first in the pinned group.
      await showSidebar(page);
      await expect(row(page, 'Grok')).toHaveAccessibleDescription('메인 Bot');
      await expect(sidebar(page).locator('.pinned-bots .bot-row').first()).toHaveAttribute('title', 'Grok');
      await expect(row(page, 'Beta')).not.toHaveAttribute('data-main-bot', 'true');

      // "메인 Bot 선택" from Beta's menu preselects Beta.
      await openMenuItem(page, 'Beta', '메인 Bot 선택');
      const dialog = page.getByRole('dialog', { name: '메인 Bot 선택', exact: true });
      await expect(dialog).toBeVisible();
      await expect(dialog.getByRole('radio', { name: 'Beta', exact: true })).toBeChecked();
      await expect(dialog.getByRole('radio', { name: 'Beta', exact: true })).toBeFocused();
      await expect(dialog.locator('label').filter({ hasText: 'Grok' })).toContainText('현재 메인 Bot');
      await page.screenshot({ path: testInfo.outputPath(`main-bot-dialog-${viewport.width}.png`) });
      await dialog.getByRole('button', { name: '메인 Bot으로 설정', exact: true }).click();
      await expect(dialog).toHaveCount(0);
      const chosen = await snapshot(page);
      expect(chosen.mainBotId).toBe(beta.id);
      expect(chosen.bots.find(bot => bot.id === grok.id)?.pinned).toBe(true);

      await showSidebar(page);
      await expect(row(page, 'Beta')).toHaveAccessibleDescription('메인 Bot');
      await expect(row(page, 'Beta')).toContainText('메인 Bot');
      await expect(row(page, 'Grok')).not.toHaveAttribute('data-main-bot', 'true');
      await expect(sidebar(page).locator('.pinned-bots .bot-row').first()).toHaveAttribute('title', 'Beta');
      await expect(sidebar(page).locator('.pinned-bots .bot-row')).toHaveCount(2);
      await page.screenshot({ path: testInfo.outputPath(`main-bot-sidebar-${viewport.width}.png`) });

      // A first load without a selected bot opens the main bot.
      await page.goto(app.url);
      await expect(page.getByRole('heading', { name: 'Beta', exact: true })).toBeVisible();

      // Deleting the main bot says who takes over, and does it.
      await openMenuItem(page, 'Beta', 'Bot 삭제');
      const remove = page.getByRole('dialog', { name: 'Beta 삭제', exact: true });
      await expect(remove).toContainText('이 Bot은 메인 Bot이에요. 삭제하면 “Grok”이(가) 메인 Bot이 돼요.');
      await remove.getByRole('button', { name: '삭제', exact: true }).click();
      await expect(remove).toHaveCount(0);
      expect((await snapshot(page)).mainBotId).toBeUndefined();
      await showSidebar(page);
      await expect(row(page, 'Grok')).toHaveAccessibleDescription('메인 Bot');

      // Choosing the current main bot just closes the dialog.
      await openMenuItem(page, 'Grok', '메인 Bot 선택');
      const revision = (await snapshot(page)).revision;
      await dialog.getByRole('button', { name: '메인 Bot으로 설정', exact: true }).click();
      await expect(dialog).toHaveCount(0);
      expect((await snapshot(page)).revision).toBe(revision);
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
      expect(errors).toEqual([]);
    } finally {
      await app.stop();
    }
  });
}
