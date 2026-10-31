import { test, expect, chromium, type Page } from '@playwright/test';
import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { startTestApp } from './app-fixture';
import { startMockProvider } from './mock-provider';
import type { AppState } from '../shared/types';

async function state(page: Page): Promise<AppState> {
  return page.evaluate(async () => (await fetch('/api/state')).json());
}
async function settingsWithoutWorkspace(page: Page, baseUrl: string) {
  await page.getByRole('button', { name: '설정', exact: true }).click();
  await page.getByRole('button', { name: '모델 설정', exact: true }).first().click();
  await page.getByLabel('Base URL', { exact: true }).fill(baseUrl);
  await page.getByLabel('Model ID', { exact: true }).fill('local-test-model');
  await page.getByLabel('API key', { exact: true }).fill('network-test-fixture-key');
  await page.getByRole('button', { name: '설정 저장', exact: true }).click();
  await expect(page.getByText('설정을 저장했습니다', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: '설정 닫기', exact: true }).click();
}

test('LAN HTTP users create a bot, chat without a workspace, manage routines, and open execution history', async ({}, testInfo) => {
  const app = await startTestApp({ network: true });
  const provider = await startMockProvider({ reply: request => ({ text: request.tools?.some(tool => tool.function.name === 'search_tool') ? '봇과 루틴의 실제 모델 응답입니다.' : 'Local routine title' }) });
  const browser = await chromium.launch({ channel: 'chrome', headless: true, args: ['--no-sandbox', '--no-proxy-server', '--host-resolver-rules=MAP grok-bot.test 127.0.0.1'] });
  const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
  const page = await context.newPage();
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  try {
    await page.goto(app.url);
    expect(await page.evaluate(() => window.isSecureContext)).toBe(false);
    expect(await page.evaluate(() => typeof crypto.randomUUID)).toBe('undefined');
    await settingsWithoutWorkspace(page, provider.baseUrl);
    expect((await state(page)).settings.workspace).toBe('');
    await page.getByRole('button', { name: '봇 추가', exact: true }).click();
    await page.getByLabel('봇 이름', { exact: true }).fill('루틴 도우미');
    await page.getByLabel('봇 지침', { exact: true }).fill('사용자의 한국어 요청에 답하세요.');
    await page.getByRole('dialog', { name: '봇 추가', exact: true }).getByRole('button', { name: '봇 추가', exact: true }).click();
    await expect(page.getByRole('heading', { name: '루틴 도우미', exact: true })).toBeVisible();
    await page.getByLabel('메시지', { exact: true }).fill('작업 폴더 없이 대화해 줘');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    await expect(page.getByRole('log').getByText('봇과 루틴의 실제 모델 응답입니다.', { exact: true })).toBeVisible({ timeout: 30_000 });
    const bot = (await state(page)).bots.find(bot => bot.name === '루틴 도우미')!;
    await page.getByRole('button', { name: '루틴', exact: true }).click();
    await expect(page.getByText('등록된 루틴이 없습니다.', { exact: true })).toBeVisible();
    await page.getByRole('button', { name: '루틴 만들기', exact: true }).first().click();
    await page.getByLabel('루틴 이름', { exact: true }).fill('반복 점검');
    await page.getByLabel('실행할 봇', { exact: true }).selectOption(bot.id);
    await page.getByLabel('실행할 지시', { exact: true }).fill('반복 점검 결과를 알려 줘');
    await page.getByLabel('실행 일정', { exact: true }).selectOption('interval');
    await page.getByLabel('반복 간격(분)', { exact: true }).fill('5');
    await page.getByRole('button', { name: '루틴 저장', exact: true }).click();
    await expect(page.getByText('반복 점검', { exact: true }).first()).toBeVisible();
    const routine = (await state(page)).routines.find(routine => routine.name === '반복 점검')!;
    expect(routine.botId).toBe(bot.id);
    expect(routine.schedule).toEqual({ kind: 'interval', minutes: 5 });
    await page.getByRole('button', { name: '일시정지', exact: true }).click();
    await expect.poll(async () => (await state(page)).routines.find(item => item.id === routine.id)?.enabled).toBe(false);
    await page.getByRole('button', { name: '다시 시작', exact: true }).click();
    await expect.poll(async () => (await state(page)).routines.find(item => item.id === routine.id)?.enabled).toBe(true);
    await page.getByRole('button', { name: '루틴 편집', exact: true }).click();
    await page.getByLabel('루틴 이름', { exact: true }).fill('매일 점검');
    await page.getByLabel('실행 일정', { exact: true }).selectOption('daily');
    await page.getByLabel('실행 시각', { exact: true }).fill('09:30');
    await page.getByLabel('시간대', { exact: true }).fill('Asia/Seoul');
    await page.getByRole('button', { name: '루틴 저장', exact: true }).click();
    await expect.poll(async () => (await state(page)).routines.find(item => item.id === routine.id)?.schedule).toEqual({ kind: 'daily', time: '09:30', timezone: 'Asia/Seoul' });
    await page.getByRole('button', { name: '지금 실행', exact: true }).click();
    await expect.poll(async () => (await state(page)).routineRuns.find(run => run.routineId === routine.id)?.result?.status, { timeout: 30_000 }).toBe('completed');
    await page.getByRole('button', { name: '대화 보기', exact: true }).first().click();
    await expect(page.getByRole('article', { name: '내 메시지', exact: true }).getByText('반복 점검 결과를 알려 줘', { exact: true })).toBeVisible();
    await expect(page.getByRole('log').getByText('봇과 루틴의 실제 모델 응답입니다.', { exact: true })).toBeVisible();
    await page.getByLabel('메시지', { exact: true }).fill('루틴 후속 대화');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    await expect.poll(async () => {
      const snapshot = await state(page);
      return snapshot.conversations.find(conversation => conversation.id === snapshot.routineRuns[0].conversationId)?.messages.filter(message => message.role === 'assistant' && message.text).length;
    }).toBe(2);
    expect((await state(page)).routineRuns[0].result?.status).toBe('completed');
    await expect.poll(async () => {
      const snapshot = await state(page);
      return snapshot.conversations.find(conversation => conversation.id === snapshot.routineRuns[0].conversationId)?.run.status;
    }, { timeout: 30_000 }).toBe('completed');
    await page.getByRole('button', { name: '루틴', exact: true }).click();
    await page.reload();
    await expect(page.getByText('매일 점검', { exact: true }).first()).toBeVisible();
    await page.screenshot({ path: testInfo.outputPath('routines-desktop.png'), fullPage: true });
    await page.setViewportSize({ width: 390, height: 844 });
    await expect(page.getByText('매일 점검', { exact: true }).first()).toBeVisible();
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
    await page.screenshot({ path: testInfo.outputPath('routines-mobile.png'), fullPage: true });
    page.once('dialog', dialog => dialog.accept());
    await page.getByRole('button', { name: '루틴 삭제', exact: true }).click();
    await expect.poll(async () => (await state(page)).routines.length).toBe(0);
    expect((await state(page)).routineRuns.length).toBe(1);
    await page.setViewportSize({ width: 1440, height: 1000 });
    await page.getByRole('button', { name: '설정', exact: true }).click();
    await page.getByRole('button', { name: '로컬 파일 설정', exact: true }).click();
    await page.getByLabel('로컬 작업 폴더 경로', { exact: true }).fill(app.workspace);
    await page.getByRole('button', { name: '설정 저장', exact: true }).click();
    await expect(page.getByText('설정을 저장했습니다', { exact: true })).toBeVisible();
    await page.getByRole('button', { name: '설정 닫기', exact: true }).click();
    await page.getByRole('button', { name: /루틴 도우미/ }).first().click();
    await page.getByRole('button', { name: '컴퓨터 열기', exact: true }).click();
    await page.getByRole('button', { name: '파일 notes.txt', exact: true }).click();
    const editor = page.getByLabel('파일 내용', { exact: true });
    await expect(editor).toHaveValue('original local content\n');
    await editor.fill('edited over LAN HTTP\n');
    await page.getByRole('button', { name: '파일 저장', exact: true }).click();
    await expect(page.getByText('저장 완료', { exact: true })).toBeVisible();
    expect(await readFile(join(app.workspace, 'notes.txt'), 'utf8')).toBe('edited over LAN HTTP\n');
    expect(errors).toEqual([]);
  } finally {
    await browser.close();
    await app.stop();
    await provider.close();
  }
});
