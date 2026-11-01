import { test, expect, type Page } from '@playwright/test';
import { startTestApp } from './app-fixture';
import { startMockProvider, type MockCompletion } from './mock-provider';
import type { AppState } from '../shared/types';

const snapshot = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());
const routine = { name: '아침 브리핑', prompt: '오늘 할 일을 정리해줘', schedule: { kind: 'daily', time: '09:30', timezone: 'Asia/Seoul' } };

// Routines are set up by asking the bot in chat; each save waits for a one-use approval.
test('a bot creates a routine from chat after approval and the card opens the routines view', async ({ page }) => {
  test.skip(process.env.GROK_BOT_BACKEND !== 'rust', 'Chat-managed routines are implemented by the Rust server.');
  const app = await startTestApp();
  let stage = 0;
  const provider = await startMockProvider({ reply: request => {
    if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: 'Routine fixture' };
    const plan: MockCompletion[] = [
      { tool: { name: 'search_tool', arguments: { query: 'workspace routines', limit: 4 } } },
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__save_routine', tool_input: routine } } },
      { text: '매일 아침 9시 30분 브리핑 루틴을 만들었어요.' },
    ];
    return plan[stage++] ?? { text: 'Complete.' };
  } });
  try {
    await page.goto(app.url);
    await page.evaluate(async baseUrl => {
      const response = await fetch('/api/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ baseUrl, model: 'local-test-model', workspace: '' }) });
      if (!response.ok) throw Error(await response.text());
    }, provider.baseUrl);
    await page.getByLabel('메시지', { exact: true }).fill('매일 아침 9시 반에 브리핑 루틴 만들어줘');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    const approval = page.getByRole('region', { name: '작업 승인 요청', exact: true });
    await expect(approval).toContainText('루틴을 저장하도록 허용할까요?');
    await expect(approval).toContainText('매일 09:30 (Asia/Seoul)');
    expect((await snapshot(page)).routines).toHaveLength(0);
    await approval.getByRole('button', { name: 'Save routine once', exact: true }).click();
    const log = page.getByRole('log', { name: '대화 메시지', exact: true });
    await expect(log).toContainText('브리핑 루틴을 만들었어요');
    await expect(log).toContainText('생성됨 루틴 ·');
    const saved = (await snapshot(page)).routines;
    expect(saved).toHaveLength(1);
    expect(saved[0]).toMatchObject({ name: routine.name, enabled: true, schedule: routine.schedule });
    await log.getByRole('button', { name: `${routine.name} 루틴 열기`, exact: true }).click();
    await expect(page).toHaveURL(/view=routines/);
    await expect(page.getByText(routine.name, { exact: true }).first()).toBeVisible();
  } finally { await app.stop(); await provider.close(); }
});
