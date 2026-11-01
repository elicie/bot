import { test, expect, type Page } from '@playwright/test';
import type { AppState } from '../shared/types';
import { startTestApp } from './app-fixture';
import { startMockProvider, type MockProviderRequest } from './mock-provider';

const snapshot = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());
const api = <T,>(page: Page, path: string, method: string, body: unknown): Promise<T> => page.evaluate(async ({ path, method, body }) => {
  const response = await fetch(path, { method, headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify(body) });
  if (!response.ok) throw new Error(await response.text());
  return response.json();
}, { path, method, body });

const text = (content: unknown) => typeof content === 'string' ? content : Array.isArray(content) ? content.map(part => (part as { text?: string }).text ?? '').join('\n') : '';
/** The member addressed by the latest group prompt, or null outside group chats. */
export function speakerOf(request: MockProviderRequest) {
  const prompt = [...request.messages].reverse().map(message => text(message.content)).find(value => value.includes('<user_query>')) ?? '';
  return /You are (.+?)\. Everything quoted/.exec(prompt)?.[1] ?? null;
}
const isMain = (request: MockProviderRequest) => request.tools?.some(tool => tool.function.name === 'search_tool');

async function prepare(page: Page, baseUrl: string) {
  await page.evaluate(async baseUrl => {
    const response = await fetch('/api/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ baseUrl, model: 'local-test-model', workspace: '' }) });
    if (!response.ok) throw new Error(await response.text());
  }, baseUrl);
  for (const name of ['Alpha', 'Beta']) await api(page, '/api/bots', 'POST', { name, color: name === 'Alpha' ? '#ff6b00' : '#00b6a6', shape: 'star' });
}

async function openSidebar(page: Page) {
  if (page.viewportSize()!.width <= 760) await page.getByRole('button', { name: '봇 목록 열기', exact: true }).click();
}

const main = (page: Page) => page.getByRole('main');
const memberReplies = (page: Page) => main(page).getByRole('log', { name: '대화 메시지' }).locator('article.message-row.assistant');

for (const viewport of [{ width: 1280, height: 820 }, { width: 390, height: 844 }]) {
  test(`group chats answer in member order with attribution at ${viewport.width}px`, async ({ page }) => {
    const app = await startTestApp();
    let release: () => void = () => undefined;
    let holdAlpha = false;
    const provider = await startMockProvider({
      reply: async request => {
        if (!isMain(request)) return { text: 'Fixture title' };
        const speaker = speakerOf(request);
        if (speaker === 'Alpha' && holdAlpha) await new Promise<void>(resolve => { release = resolve; });
        return { text: speaker ? `${speaker}의 답변입니다.` : '기본 답변입니다.' };
      },
    });
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    try {
      await page.setViewportSize(viewport);
      await page.goto(app.url);
      await prepare(page, provider.baseUrl);
      await page.reload();

      // Create a group from the sidebar with two members in a chosen order.
      await openSidebar(page);
      await page.getByRole('region', { name: '그룹' }).getByRole('button', { name: '그룹 채팅 만들기' }).click();
      const dialog = page.getByRole('dialog', { name: '그룹 채팅 만들기' });
      await dialog.getByLabel('그룹 이름', { exact: true }).fill('기획 팀');
      await dialog.getByRole('button', { name: '하트형 그룹 모양' }).click();
      await expect(dialog.getByRole('button', { name: '그룹 만들기' })).toBeDisabled();
      await dialog.getByRole('button', { name: 'Alpha 멤버 추가' }).click();
      await dialog.getByRole('button', { name: 'Beta 멤버 추가' }).click();
      await dialog.getByRole('button', { name: '그룹 만들기' }).click();
      await expect(dialog).toBeHidden();
      const group = (await snapshot(page)).groups![0];
      expect(group).toMatchObject({ name: '기획 팀', shape: 'heart' });
      expect(new URL(page.url()).searchParams.get('group')).toBe(group.id);
      expect(new URL(page.url()).hash).toBe('');
      await expect(main(page).getByRole('button', { name: '그룹 편집' })).toContainText('기획 팀');

      // Both members answer once, Alpha first, each with its own name.
      await main(page).getByLabel('메시지', { exact: true }).fill('출시 일정을 정리해 주세요');
      await main(page).getByRole('button', { name: '메시지 보내기' }).click();
      await expect(memberReplies(page)).toHaveCount(2, { timeout: 30_000 });
      await expect(memberReplies(page).nth(0)).toHaveAccessibleName('Alpha 메시지');
      await expect(memberReplies(page).nth(0)).toContainText('Alpha의 답변입니다.');
      await expect(memberReplies(page).nth(1)).toHaveAccessibleName('Beta 메시지');
      await expect(memberReplies(page).nth(1).locator('.message-speaker')).toHaveText('Beta');
      await expect.poll(async () => (await snapshot(page)).conversations.find(item => item.groupId === group.id)?.run.status).toBe('completed');
      const conversation = (await snapshot(page)).conversations.find(item => item.groupId === group.id)!;
      expect(conversation.messages.map(message => message.speakerBotId ?? null)).toEqual([null, group.memberBotIds[0], group.memberBotIds[1]]);
      expect(provider.requests.filter(isMain).map(speakerOf)).toEqual(['Alpha', 'Beta']);

      // Reload keeps the group route; browser navigation returns to it.
      await page.reload();
      await expect(memberReplies(page)).toHaveCount(2);
      expect(new URL(page.url()).searchParams.get('group')).toBe(group.id);
      await openSidebar(page);
      await page.getByRole('complementary', { name: '봇 목록' }).getByRole('button', { name: /^Grok/ }).click();
      await expect(page.getByRole('button', { name: '봇 편집' })).toContainText('Grok');
      await page.goBack();
      await expect(main(page).getByRole('button', { name: '그룹 편집' })).toContainText('기획 팀');
      await expect(memberReplies(page)).toHaveCount(2);

      // A mention narrows who answers.
      await main(page).getByRole('button', { name: 'Beta 멘션' }).click();
      await expect(main(page).getByLabel('메시지', { exact: true })).toHaveValue('@Beta ');
      await main(page).getByLabel('메시지', { exact: true }).fill('@Beta 혼자 답해 주세요');
      await main(page).getByRole('button', { name: '메시지 보내기' }).click();
      await expect(memberReplies(page)).toHaveCount(3, { timeout: 30_000 });
      await expect(memberReplies(page).nth(2)).toHaveAccessibleName('Beta 메시지');

      if (viewport.width > 760) {
        // Reorder members; the next message is answered Beta first.
        await main(page).getByRole('button', { name: '그룹 편집' }).click();
        const edit = page.getByRole('dialog', { name: '그룹 편집' });
        await edit.getByRole('button', { name: 'Beta 위로' }).click();
        await edit.getByLabel('그룹 이름', { exact: true }).fill('출시 팀');
        await edit.getByRole('button', { name: '그룹 저장' }).click();
        await expect(edit).toBeHidden();
        await expect(page.getByRole('region', { name: '그룹' }).getByRole('button', { name: /^출시 팀/ })).toBeVisible();
        await main(page).getByLabel('메시지', { exact: true }).fill('순서를 바꿨어요');
        await main(page).getByRole('button', { name: '메시지 보내기' }).click();
        await expect(memberReplies(page)).toHaveCount(5, { timeout: 30_000 });
        await expect(memberReplies(page).nth(3)).toHaveAccessibleName('Beta 메시지');
        await expect(memberReplies(page).nth(4)).toHaveAccessibleName('Alpha 메시지');

        // Stopping the first speaker also cancels the queued member.
        await expect.poll(async () => (await snapshot(page)).conversations.find(item => item.id === conversation.id)?.run.status).toBe('completed');
        await main(page).getByRole('button', { name: '그룹 편집' }).click();
        await page.getByRole('dialog', { name: '그룹 편집' }).getByRole('button', { name: 'Alpha 위로' }).click();
        await page.getByRole('dialog', { name: '그룹 편집' }).getByRole('button', { name: '그룹 저장' }).click();
        holdAlpha = true;
        const before = provider.requests.filter(isMain).length;
        await main(page).getByLabel('메시지', { exact: true }).fill('중간에 멈출게요');
        await main(page).getByRole('button', { name: '메시지 보내기' }).click();
        await expect(main(page).getByRole('status').filter({ hasText: '다음 차례: Beta' })).toBeVisible({ timeout: 30_000 });
        await expect.poll(() => provider.requests.filter(isMain).slice(before).map(speakerOf), { timeout: 30_000 }).toEqual(['Alpha']);
        await main(page).getByRole('button', { name: '작업 중단' }).click();
        await expect(main(page).getByText('작업을 중단했습니다.')).toBeVisible({ timeout: 30_000 });
        release();
        await page.waitForTimeout(1500);
        const stopped = (await snapshot(page)).conversations.find(item => item.id === conversation.id)!;
        expect(stopped.run.status).toBe('cancelled');
        expect(stopped.groupRound).toBeUndefined();
        expect(provider.requests.filter(isMain).slice(before).map(speakerOf)).toEqual(['Alpha']);

        // Deleting the group keeps its bots and leaves the group route.
        page.once('dialog', confirm => confirm.accept());
        await main(page).getByRole('button', { name: '그룹 편집' }).click();
        await page.getByRole('dialog', { name: '그룹 편집' }).getByRole('button', { name: '그룹 삭제' }).click();
        await expect(page.getByRole('region', { name: '그룹' }).getByRole('button', { name: /^출시 팀/ })).toHaveCount(0);
        const after = await snapshot(page);
        expect(after.groups ?? []).toEqual([]);
        expect(after.bots.map(bot => bot.name)).toEqual(['Grok', 'Alpha', 'Beta']);
        expect(new URL(page.url()).searchParams.get('group')).toBeNull();
      }
      expect(errors).toEqual([]);
    } finally {
      release();
      await provider.close();
      await app.stop();
    }
  });
}
