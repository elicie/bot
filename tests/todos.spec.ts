import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { test, expect, type Page } from '@playwright/test';
import type { AppState, TodoItem } from '../shared/types';
import { startTestApp } from './app-fixture';
import { startMockProvider, type MockCompletion } from './mock-provider';

const snapshot = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());
const gate = () => {
  let resolve!: (reply: MockCompletion) => void;
  return { promise: new Promise<MockCompletion>(done => { resolve = done; }), release: (reply: MockCompletion) => resolve(reply) };
};
const plan = (todos: { id: string; content?: string; status?: string }[], merge = false): MockCompletion => ({ tool: { name: 'todo_write', arguments: { merge, todos } } });

for (const viewport of [{ width: 1040, height: 760 }, { width: 390, height: 844 }, { width: 320, height: 760 }]) {
  test(`latest checklist updates, collapses, persists and resets at ${viewport.width}px`, async ({ page }, testInfo) => {
    const dataDir = await mkdtemp(path.join(tmpdir(), 'grok-bot-todos-'));
    let app = await startTestApp({ env: { GROK_BOT_DATA_DIR: dataDir } });
    const initial = [
      { id: 'research', content: '요청과 자료 확인', status: 'completed' },
      { id: 'draft', content: viewport.width === 320 ? `핵심 내용 정리: ${'줄바꿈없는긴항목'.repeat(25)}` : '핵심 내용 정리', status: 'in_progress' },
      { id: 'review', content: '결과 검토', status: 'pending' },
    ];
    const gates = Array.from({ length: 5 }, gate);
    let calls = 0;
    const provider = await startMockProvider({ reply: request => {
      if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: '세 단계 작업' };
      const index = calls++;
      return index === 0 ? plan(initial) : gates[index - 1]?.promise ?? { text: '후속 답변' };
    } });
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.setViewportSize(viewport);
    try {
      await page.goto(app.url);
      await page.evaluate(async baseUrl => {
        const response = await fetch('/api/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ baseUrl, model: 'local-test-model', workspace: '' }) });
        if (!response.ok) throw new Error(await response.text());
      }, provider.baseUrl);
      await page.getByLabel('메시지', { exact: true }).fill('요청을 3단계로 나눠서 해 줘');
      await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
      const checklist = page.getByRole('region', { name: '할 일 목록', exact: true });
      await expect(checklist).toBeVisible();
      await expect(checklist.getByRole('listitem')).toHaveCount(3);
      await expect(checklist.locator('[aria-current="step"]')).toContainText('핵심 내용 정리');
      await expect(checklist).toContainText('1/3 완료');
      await page.screenshot({ path: testInfo.outputPath(`todos-active-${viewport.width}.png`) });
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
      // Reuse the app's theme contract, including system-light; never leave white-on-white text.
      await page.emulateMedia({ colorScheme: 'light' });
      for (const theme of ['light', 'system']) {
        await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
        const colors = await checklist.evaluate(element => ({
          heading: getComputedStyle(element.querySelector('strong')!).color,
          active: getComputedStyle(element.querySelector('.todo-in_progress')!).color,
          background: getComputedStyle(element).backgroundColor,
        }));
        expect(colors).toEqual({ heading: 'rgb(32, 33, 36)', active: 'rgb(24, 93, 159)', background: 'rgb(255, 255, 255)' });
      }
      await page.screenshot({ path: testInfo.outputPath(`todos-light-${viewport.width}.png`) });
      await page.evaluate(() => { document.documentElement.dataset.theme = 'dark'; });
      await page.emulateMedia({ colorScheme: 'dark' });

      // Native disclosure supports the keyboard and stays collapsed across new snapshots.
      const summary = checklist.locator('summary');
      await summary.focus();
      await page.keyboard.press('Enter');
      await expect(checklist.getByRole('list')).toBeHidden();
      const update = [{ id: 'draft', status: 'completed' }, { id: 'review', status: 'in_progress' }];
      gates[0].release(plan(update, true));
      await expect(checklist).toContainText('2/3 완료');
      await expect(checklist.getByRole('list')).toBeHidden();
      await page.keyboard.press('Enter');
      await expect(checklist.locator('[aria-current="step"]')).toContainText('결과 검토');
      gates[1].release(plan(update, true));
      await expect.poll(() => calls).toBe(4);
      await expect(checklist).toHaveCount(1);
      await expect(checklist.getByRole('listitem')).toHaveCount(3);
      await expect(page.locator('.tool-card').filter({ hasText: 'Updating plan' })).toHaveCount(0);

      const replacement = [{ id: 'only', content: '최종 확인', status: 'completed' }, { id: 'cancelled', content: '생략한 선택 단계', status: 'cancelled' }];
      gates[2].release(plan(replacement));
      await expect(checklist.getByRole('listitem')).toHaveCount(2);
      await expect(checklist).not.toContainText('요청과 자료 확인');
      await expect(checklist).toContainText('취소됨');
      gates[3].release({ text: '세 단계 결과를 검토했습니다.' });
      await expect.poll(async () => (await snapshot(page)).conversations[0]?.run.status).toBe('completed');
      const expected: TodoItem[] = replacement.map(({ content, status }) => ({ content, status: status as TodoItem['status'] }));
      await expect.poll(async () => (await snapshot(page)).conversations[0]?.run.todos).toEqual(expected);
      await page.reload();
      await expect(checklist).toContainText('최종 확인');
      await expect(checklist).toContainText('취소됨');

      // Restart the actual server with the same isolated disk state, then open the same route.
      const route = new URL(page.url()).search;
      await app.stop();
      app = await startTestApp({ env: { GROK_BOT_DATA_DIR: dataDir } });
      await page.goto(app.url + route);
      await expect(checklist).toContainText('최종 확인');
      expect((await snapshot(page)).conversations[0]?.run.todos).toEqual(expected);
      await page.getByLabel('메시지', { exact: true }).fill('다음 작업을 해 줘');
      await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
      await expect.poll(() => calls).toBe(6);
      await expect(checklist).toHaveCount(0);
      expect((await snapshot(page)).conversations[0]?.run.todos).toBeUndefined();
      // The next tool's default merge must not resurrect the previous turn's list.
      // A plain final response does not prove the active task finished: ignore cosmetic cleanup.
      gates[4].release(plan([{ id: 'new-task', content: '아직 끝내지 못한 다음 작업', status: 'in_progress' }], true));
      await expect(page.getByText('후속 답변', { exact: true })).toBeVisible();
      await expect.poll(async () => (await snapshot(page)).conversations[0]?.run.status).toBe('completed');
      await expect(checklist.getByRole('listitem')).toHaveCount(1);
      await expect(checklist).toContainText('아직 끝내지 못한 다음 작업');
      await expect(checklist).toContainText('0/1 완료');
      await expect(checklist.locator('.todo-in_progress')).toHaveCount(1);
      await expect(checklist.locator('.spin')).toHaveCount(0);
      expect(errors).toEqual([]);
    } finally {
      gates.forEach(item => item.release({ text: '테스트 종료' }));
      await app.stop();
      await provider.close();
      // The path is a fresh test-owned directory, never a configured user data directory.
      if (path.dirname(dataDir) !== tmpdir() || !path.basename(dataDir).startsWith('grok-bot-todos-')) throw new Error('Unexpected test data path');
      await rm(dataDir, { recursive: true, force: true });
    }
  });
}
