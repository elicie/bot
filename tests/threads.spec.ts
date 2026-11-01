import { test, expect, type Page } from '@playwright/test';
import type { AppState, Conversation, Group } from '../shared/types';
import { startTestApp } from './app-fixture';
import { startMockProvider, type MockProviderRequest } from './mock-provider';

const snapshot = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());
const api = <T,>(page: Page, path: string, method: string, body: unknown): Promise<T> => page.evaluate(async ({ path, method, body }) => {
  const response = await fetch(path, { method, headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify(body) });
  if (!response.ok) throw new Error(await response.text());
  return response.json();
}, { path, method, body });
const text = (content: unknown) => typeof content === 'string' ? content : Array.isArray(content) ? content.map(part => (part as { text?: string }).text ?? '').join('\n') : '';
const prompt = (request: MockProviderRequest) => [...request.messages].reverse().map(message => text(message.content)).find(value => value.includes('<user_query>')) ?? '';
const speakerOf = (request: MockProviderRequest) => /You are (.+?)\. Everything quoted/.exec(prompt(request))?.[1] ?? null;
const isMain = (request: MockProviderRequest) => request.tools?.some(tool => tool.function.name === 'search_tool');

const main = (page: Page) => page.getByRole('main');
const panel = (page: Page) => page.getByRole('complementary', { name: '스레드', exact: true });
const route = (page: Page, name: string) => new URL(page.url()).searchParams.get(name);

for (const viewport of [{ width: 1280, height: 820 }, { width: 390, height: 844 }]) {
  test(`threads open beside the chat, keep their route, and quote the parent at ${viewport.width}px`, async ({ page }) => {
    const app = await startTestApp();
    let answers = 0;
    const provider = await startMockProvider({
      reply: request => {
        if (!isMain(request)) return { text: 'Fixture title' };
        const speaker = speakerOf(request);
        answers += 1;
        return { text: speaker ? `${speaker} 스레드 답변` : `답변 ${answers}` };
      },
    });
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    try {
      await page.setViewportSize(viewport);
      await page.goto(app.url);
      await page.evaluate(async baseUrl => {
        const response = await fetch('/api/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ baseUrl, model: 'local-test-model', workspace: '' }) });
        if (!response.ok) throw new Error(await response.text());
      }, provider.baseUrl);

      // A 1:1 chat: start a thread from the assistant reply.
      await main(page).getByLabel('메시지', { exact: true }).fill('원래 질문입니다');
      await main(page).getByRole('button', { name: '메시지 보내기' }).click();
      const reply = main(page).getByRole('article', { name: 'Grok 메시지' });
      await expect(reply).toContainText('답변 1', { timeout: 30_000 });
      await expect.poll(async () => (await snapshot(page)).conversations[0]?.run.status).toBe('completed');
      const parent = (await snapshot(page)).conversations[0];
      await main(page).getByRole('button', { name: '스레드에서 답장' }).last().click();
      await expect(panel(page)).toBeVisible();
      await expect(panel(page).getByRole('blockquote')).toContainText('답변 1');
      const thread = (await snapshot(page)).conversations.find(item => item.thread)!;
      expect(thread.thread).toEqual({ parentConversationId: parent.id, parentMessageId: parent.messages[1].id });
      expect(route(page, 'thread')).toBe(thread.id);
      expect(route(page, 'conversation')).toBe(parent.id);
      expect(new URL(page.url()).hash).toBe('');
      if (viewport.width > 760) {
        const [chat, side] = await Promise.all([main(page).boundingBox(), panel(page).boundingBox()]);
        expect(side!.x).toBeGreaterThanOrEqual(chat!.x + chat!.width - 1);
      } else {
        const side = await panel(page).boundingBox();
        expect(side).toMatchObject({ x: 0, width: viewport.width });
      }

      await panel(page).getByLabel('스레드 메시지', { exact: true }).fill('스레드 질문입니다');
      await panel(page).getByRole('button', { name: '메시지 보내기' }).click();
      await expect(panel(page).getByRole('article', { name: 'Grok 메시지' })).toContainText('답변 2', { timeout: 30_000 });
      const first = provider.requests.filter(isMain).map(prompt).at(-1)!;
      expect(first).toContain('"threadParent":{"from":"Grok","text":"답변 1"}');
      expect(first).toContain('{"from":"user","text":"원래 질문입니다"}');
      // The thread does not touch the parent transcript.
      expect((await snapshot(page)).conversations.find(item => item.id === parent.id)!.messages).toHaveLength(2);
      await expect(main(page).getByRole('article')).toHaveCount(2);

      // Reload keeps the open thread; closing and browser navigation follow the query route.
      await page.reload();
      await expect(panel(page).getByRole('article', { name: 'Grok 메시지' })).toContainText('답변 2');
      await panel(page).getByRole('button', { name: '스레드 닫기' }).click();
      await expect(panel(page)).toHaveCount(0);
      expect(route(page, 'thread')).toBeNull();
      await expect(main(page).getByRole('button', { name: '답글 스레드 열기' })).toHaveText('답글 2개');
      await page.goBack();
      await expect(panel(page)).toBeVisible();
      await panel(page).getByRole('button', { name: '스레드 닫기' }).click();
      await main(page).getByRole('button', { name: '답글 스레드 열기' }).click();
      await expect(panel(page).getByRole('article', { name: '내 메시지' })).toContainText('스레드 질문입니다');
      await panel(page).getByRole('button', { name: '스레드 닫기' }).click();

      // Threads in a group conversation are answered by the members, with the parent quoted.
      const bots: string[] = [];
      for (const name of ['Alpha', 'Beta']) {
        const state = await api<AppState>(page, '/api/bots', 'POST', { name });
        bots.push(state.bots.find(bot => bot.name === name)!.id);
      }
      const { group } = await api<{ group: Group }>(page, '/api/groups', 'POST', { name: '팀', memberBotIds: bots });
      const { conversation } = await api<{ conversation: Conversation }>(page, `/api/groups/${group.id}/conversations`, 'POST', {});
      await page.goto(`${app.url}/?group=${group.id}&conversation=${conversation.id}`);
      await main(page).getByLabel('메시지', { exact: true }).fill('그룹 질문');
      await main(page).getByRole('button', { name: '메시지 보내기' }).click();
      await expect(main(page).getByRole('article', { name: 'Beta 메시지' })).toContainText('Beta 스레드 답변', { timeout: 30_000 });
      await expect.poll(async () => (await snapshot(page)).conversations.find(item => item.id === conversation.id)?.run.status).toBe('completed');
      await main(page).getByRole('button', { name: '스레드에서 답장' }).last().click();
      await expect(panel(page).getByRole('blockquote')).toContainText('Beta');
      expect(route(page, 'group')).toBe(group.id);
      expect(route(page, 'thread')).not.toBeNull();
      await panel(page).getByLabel('스레드 메시지', { exact: true }).fill('스레드에서 둘 다 답해 주세요');
      await panel(page).getByRole('button', { name: '메시지 보내기' }).click();
      await expect(panel(page).locator('article.message-row.assistant')).toHaveCount(2, { timeout: 30_000 });
      await expect(panel(page).locator('article.message-row.assistant').nth(0)).toHaveAccessibleName('Alpha 메시지');
      await expect(panel(page).locator('article.message-row.assistant').nth(1)).toHaveAccessibleName('Beta 메시지');
      await expect(panel(page).locator('article.message-row.assistant').nth(1)).toContainText('Beta 스레드 답변', { timeout: 30_000 });
      await expect(panel(page).locator('article.message-row.assistant').nth(0)).toContainText('Alpha 스레드 답변');
      const threadPrompts = provider.requests.filter(isMain).slice(-2).map(prompt);
      for (const value of threadPrompts) expect(value).toContain('"threadParent":{"from":"Beta","text":"Beta 스레드 답변"}');
      await page.reload();
      await expect(panel(page).locator('article.message-row.assistant')).toHaveCount(2);
      await panel(page).getByRole('button', { name: '스레드 닫기' }).click();
      await expect(main(page).getByRole('button', { name: '답글 스레드 열기' })).toHaveText('답글 3개');
      expect(errors).toEqual([]);
    } finally {
      await provider.close();
      await app.stop();
    }
  });
}
