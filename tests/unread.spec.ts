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

/** Send a message in a new conversation of `botId` through the API and wait for its answer. */
async function answerIn(page: Page, botId: string) {
  const { conversation } = await api<{ conversation: Conversation }>(page, 'POST', '/api/conversations', { botId });
  await api(page, 'POST', `/api/conversations/${conversation.id}/messages`, { text: '백그라운드 작업' });
  await expect.poll(async () => (await snapshot(page)).conversations.find(item => item.id === conversation.id)?.run.status, { timeout: 30_000 }).toBe('completed');
  return conversation.id;
}

const sidebar = (page: Page) => page.getByRole('complementary', { name: '봇 목록', exact: true });
const mobile = (page: Page) => page.viewportSize()!.width <= 760;
async function openSidebar(page: Page) {
  if (mobile(page) && !(await sidebar(page).isVisible())) await page.getByRole('button', { name: '봇 목록 열기', exact: true }).click();
}
const row = (page: Page, name: string) => sidebar(page).getByRole('button', { name: new RegExp(`^${name}`) });
async function menu(page: Page, name: string, item: string) {
  await openSidebar(page);
  await sidebar(page).getByRole('button', { name: `추가 작업: ${name}`, exact: true }).click();
  await page.getByRole('menu', { name: `${name} 작업`, exact: true }).getByRole(item === '알림 받기' || item === '알림 끄기' ? 'menuitemcheckbox' : 'menuitem', { name: item, exact: true }).click();
}

for (const viewport of [{ width: 1280, height: 860 }, { width: 390, height: 844 }]) {
  test(`unread state, notifications, and the message sound at ${viewport.width}px`, async ({ page, context }, testInfo) => {
    const app = await startTestApp();
    const provider = await startMockProvider({ reply: () => ({ text: '작업을 마쳤습니다.' }) });
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await context.grantPermissions(['notifications']);
    await page.addInitScript(() => {
      const recorded: { title: string; body?: string }[] = [];
      const sounds = { count: 0 };
      Object.assign(window, { __notifications: recorded, __sounds: sounds });
      const Original = window.Notification;
      class Recording extends Original {
        constructor(title: string, options?: NotificationOptions) {
          super(title, options);
          recorded.push({ title, body: options?.body });
        }
      }
      Object.defineProperty(window, 'Notification', { value: Recording, configurable: true });
      window.addEventListener('grok-bot:message-sound', () => { sounds.count++; });
    });
    await page.setViewportSize(viewport);
    try {
      await page.goto(app.url);
      await api(page, 'PUT', '/api/settings', { baseUrl: provider.baseUrl, model: 'local-test-model', workspace: '' });
      const state = await api<AppState>(page, 'POST', '/api/bots', { name: '알림 Bot' });
      const other = state.bots.find(bot => bot.name === '알림 Bot')!;
      await expect(page.getByRole('heading', { name: 'Grok', exact: true })).toBeVisible();

      // An answer in a bot the user is not viewing makes it unread, plays the sound, and survives reload.
      await answerIn(page, other.id);
      await expect.poll(async () => (await snapshot(page)).bots.find(bot => bot.id === other.id)?.unread).toBe(true);
      await expect.poll(() => page.evaluate(() => (window as unknown as { __sounds: { count: number } }).__sounds.count)).toBeGreaterThan(0);
      await openSidebar(page);
      await expect(row(page, '알림 Bot')).toHaveAttribute('data-unread', 'true');
      await expect(row(page, '알림 Bot')).toHaveAccessibleDescription('읽지 않은 활동');
      await expect(row(page, 'Grok')).not.toHaveAttribute('data-unread', 'true');
      await sidebar(page).getByRole('button', { name: '추가 작업: 알림 Bot', exact: true }).click();
      await page.screenshot({ path: testInfo.outputPath(`unread-${viewport.width}.png`) });
      await page.keyboard.press('Escape');
      await page.reload();
      await openSidebar(page);
      await expect(row(page, '알림 Bot')).toHaveAttribute('data-unread', 'true');

      // Opening the bot marks it read on the server.
      await row(page, '알림 Bot').click();
      await expect(page.getByRole('heading', { name: '알림 Bot' }).or(page.getByRole('article', { name: '알림 Bot 메시지' }).first())).toBeVisible();
      await expect.poll(async () => (await snapshot(page)).bots.find(bot => bot.id === other.id)?.unread ?? false).toBe(false);

      // An answer in the bot being viewed stays read.
      await answerIn(page, other.id);
      await expect.poll(async () => (await snapshot(page)).bots.find(bot => bot.id === other.id)?.unread ?? false).toBe(false);

      // Manual "mark unread" sticks until the bot is opened again.
      await menu(page, '알림 Bot', '읽지 않음으로 표시');
      await expect.poll(async () => (await snapshot(page)).bots.find(bot => bot.id === other.id)?.unread).toBe(true);
      await page.waitForTimeout(300);
      expect((await snapshot(page)).bots.find(bot => bot.id === other.id)?.unread).toBe(true);
      await openSidebar(page);
      await row(page, 'Grok').click();
      await openSidebar(page);
      await expect(row(page, '알림 Bot')).toHaveAttribute('data-unread', 'true');
      await menu(page, '알림 Bot', '읽음으로 표시');
      await expect.poll(async () => (await snapshot(page)).bots.find(bot => bot.id === other.id)?.unread ?? false).toBe(false);
      await openSidebar(page);
      await expect(row(page, '알림 Bot')).not.toHaveAttribute('data-unread', 'true');

      // "알림 받기" shows a browser notification when the bot finishes while another bot is shown.
      await menu(page, '알림 Bot', '알림 받기');
      await expect.poll(async () => (await snapshot(page)).bots.find(bot => bot.id === other.id)?.notifyOnUpdates).toBe(true);
      await answerIn(page, other.id);
      await expect.poll(() => page.evaluate(() => (window as unknown as { __notifications: { title: string; body?: string }[] }).__notifications)).toEqual([{ title: '알림 Bot', body: '작업을 마쳤습니다.' }]);
      await openSidebar(page);
      await sidebar(page).getByRole('button', { name: '추가 작업: 알림 Bot', exact: true }).click();
      await expect(page.getByRole('menuitemcheckbox', { name: '알림 끄기', exact: true })).toHaveAttribute('aria-checked', 'true');
      await page.keyboard.press('Escape');

      // The message sound is a per-browser setting in 일반.
      await openSidebar(page);
      await sidebar(page).getByRole('button', { name: '설정', exact: true }).click();
      const sound = page.getByRole('switch', { name: '메시지 소리', exact: true });
      await expect(sound).toBeChecked();
      await expect(page.getByText('Grok Bot이 메시지를 보낼 때 재생되는 소리를 켜거나 꺼요', { exact: true })).toBeVisible();
      await sound.uncheck();
      expect(await page.evaluate(() => localStorage.getItem('grok-bot.message-sound'))).toBe('off');
      await page.reload();
      const before = await page.evaluate(() => (window as unknown as { __sounds: { count: number } }).__sounds.count);
      await answerIn(page, other.id);
      await page.waitForTimeout(200);
      expect(await page.evaluate(() => (window as unknown as { __sounds: { count: number } }).__sounds.count)).toBe(before);
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
      expect(errors).toEqual([]);
    } finally {
      await app.stop();
      await provider.close();
    }
  });
}
