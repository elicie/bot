import { test, expect } from '@playwright/test';
import { startTestApp } from './app-fixture';
import { startMockProvider, type MockCompletion } from './mock-provider';
import type { AppState } from '../shared/types';

const snapshot = (page: import('@playwright/test').Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());

test('live-reference layout preserves pinning, avatar creation, details tabs, settings preferences, and mobile navigation', async ({ page }, info) => {
  const app = await startTestApp();
  const provider = await startMockProvider();
  try {
    await page.setViewportSize({ width: 1040, height: 760 });
    await page.goto(app.url);
    await expect(page.getByLabel('봇 검색', { exact: true })).not.toBeVisible();
    await page.getByRole('button', { name: '검색', exact: true }).click();
    await page.getByLabel('봇 검색', { exact: true }).fill('no-bot-match');
    await expect(page.getByText('검색 결과가 없습니다.', { exact: true })).toBeVisible();
    await page.getByLabel('봇 검색', { exact: true }).press('Escape');
    await expect(page.getByRole('button', { name: '검색', exact: true })).toBeFocused();
    await expect(page.getByLabel('봇 검색', { exact: true })).not.toBeVisible();
    await expect(page.locator('.chat-header')).toHaveCSS('height', '60px');
    await expect(page.locator('.chat-identity')).toHaveCSS('border-radius', '24px');
    await expect(page.locator('.chat-stage')).toHaveCSS('background-color', 'rgb(17, 17, 17)');
    expect(await page.locator('.bot-tile .avatar').evaluate(element => element.getBoundingClientRect().width)).toBe(64);
    await page.getByRole('button', { name: '봇 추가', exact: true }).click();
    await page.getByLabel('봇 이름', { exact: true }).fill('Research');
    await page.getByRole('button', { name: '꽃형 캐릭터 모양', exact: true }).click();
    await expect(page.getByRole('button', { name: '꽃형 캐릭터 모양', exact: true })).toHaveCSS('border-color', 'rgb(183, 183, 183)');
    await expect(page.getByRole('button', { name: '방울형 캐릭터 모양', exact: true })).toHaveCSS('border-color', 'rgba(0, 0, 0, 0)');
    await page.getByRole('button', { name: '파랑 캐릭터 색상', exact: true }).click();
    await page.getByLabel('봇 지침', { exact: true }).fill('Local research bot.');
    await page.screenshot({ path: info.outputPath('reference-new-bot-desktop.png') });
    await page.getByRole('dialog', { name: '봇 추가', exact: true }).getByRole('button', { name: '봇 추가', exact: true }).click();
    await expect.poll(async () => (await snapshot(page)).bots.find(bot => bot.name === 'Research')?.shape).toBe('flower');
    await page.getByRole('button', { name: '추가 작업: Research', exact: true }).click();
    await page.getByRole('menuitem', { name: '고정', exact: true }).click();
    await expect(page.getByRole('region', { name: '고정된 봇', exact: true }).getByRole('button', { name: 'Research', exact: true })).toBeVisible();
    await page.getByRole('button', { name: '컴팩트 사이드바 전환', exact: true }).click();
    await expect(page.locator('.sidebar')).toHaveCSS('width', '88px');
    await expect(page.locator('.pinned-bots .avatar').first()).toHaveCSS('width', '40px');
    await page.getByRole('button', { name: '컴팩트 사이드바 전환', exact: true }).click();
    await page.getByRole('button', { name: '봇 편집', exact: true }).click();
    await expect(page.getByRole('tab', { name: '세부 정보', exact: true })).toHaveAttribute('aria-selected', 'true');
    await expect(page.getByText('Local research bot.', { exact: true })).toBeVisible();
    await page.getByRole('tab', { name: '라이브러리', exact: true }).click();
    await expect(page.getByText('이 대화에서 사용한 파일이 여기에 표시됩니다.', { exact: true })).toBeVisible();
    await page.getByRole('tab', { name: '컴퓨터', exact: true }).click();
    await expect(page.getByRole('region', { name: '관리형 브라우저', exact: true })).toBeVisible();
    await expect(page.getByRole('button', { name: '브라우저 시작', exact: true })).toBeVisible();
    await page.screenshot({ path: info.outputPath('reference-workspace-desktop.png') });
    await page.getByRole('button', { name: '컴퓨터 닫기', exact: true }).first().click();
    await page.getByRole('button', { name: '설정', exact: true }).click();
    const dialog = page.getByRole('dialog', { name: '설정', exact: true });
    for (const label of ['권한', '보관함', '사용량 및 청구', '업데이트']) {
      await dialog.getByRole('button', { name: label, exact: true }).click();
      await expect(dialog.getByRole('heading', { name: label, exact: true }).first()).toBeVisible();
    }
    await dialog.getByRole('button', { name: '일반', exact: true }).click();
    await page.getByLabel('테마', { exact: true }).selectOption('light');
    await expect(page.locator('html')).toHaveAttribute('data-theme', 'light');
    await page.getByLabel('테마', { exact: true }).selectOption('dark');
    await page.getByRole('button', { name: '모델 설정', exact: true }).first().click();
    await page.getByLabel('Base URL', { exact: true }).fill(provider.baseUrl);
    await page.getByRole('button', { name: '권한', exact: true }).click();
    await page.getByRole('button', { name: '모델 설정', exact: true }).first().click();
    await expect(page.getByLabel('Base URL', { exact: true })).toHaveValue(provider.baseUrl);
    page.once('dialog', dialog => dialog.accept());
    await page.getByRole('button', { name: '설정 닫기', exact: true }).click();
    await page.setViewportSize({ width: 390, height: 844 });
    await page.getByRole('button', { name: '봇 목록 열기', exact: true }).click();
    await page.getByRole('complementary', { name: '봇 목록', exact: true }).getByRole('button', { name: 'Research', exact: true }).click();
    await page.getByRole('button', { name: '봇 편집', exact: true }).click();
    await expect(page.getByRole('tab', { name: '세부 정보', exact: true })).toBeVisible();
    await page.screenshot({ path: info.outputPath('reference-details-mobile.png') });
    await page.getByRole('button', { name: '컴퓨터 닫기', exact: true }).first().click();
    expect(new URL(page.url()).hash).toBe('');
    await page.reload();
    expect((await snapshot(page)).bots.find(bot => bot.name === 'Research')?.pinned).toBe(true);
  } finally { await app.stop(); await provider.close(); }
});

test('browser approves a real bot delivery, opens the pair transcript, and preserves chat drafts through history', async ({ page }) => {
  const app = await startTestApp();
  let recipientId = '';
  let stage = 0;
  const provider = await startMockProvider({ reply: request => {
    if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: 'Bot pair fixture' };
    const replies: MockCompletion[] = [
      { tool: { name: 'search_tool', arguments: { query: 'workspace send_message', limit: 3 } } },
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__send_message', tool_input: { toBotId: recipientId, text: 'Please review this local plan.' } } } },
      { text: 'I queued the review request.' },
      { text: 'The plan is ready for review.' },
    ];
    return replies[stage++] ?? { text: 'Complete.' };
  } });
  try {
    await page.goto(app.url);
    await page.evaluate(async baseUrl => {
      const settings = await fetch('/api/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ baseUrl, model: 'local-test-model', workspace: '' }) });
      if (!settings.ok) throw Error(await settings.text());
      const bot = await fetch('/api/bots', { method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ name: 'Reviewer', instructions: 'Answer the review request.', color: '#75a9c0' }) });
      if (!bot.ok) throw Error(await bot.text());
    }, provider.baseUrl);
    recipientId = (await snapshot(page)).bots.find(bot => bot.name === 'Reviewer')!.id;
    await page.getByLabel('메시지', { exact: true }).fill('Ask Reviewer to check our plan.');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    const permission = page.getByRole('region', { name: '작업 승인 요청', exact: true });
    await expect(permission).toContainText('Please review this local plan.');
    expect((await snapshot(page)).botExchanges).toHaveLength(0);
    await permission.getByRole('button', { name: 'Send once', exact: true }).click();
    await expect.poll(async () => (await snapshot(page)).botDeliveries?.[0]?.status, { timeout: 30000 }).toBe('completed');
    await page.getByLabel('메시지', { exact: true }).fill('Unsent original chat draft');
    await page.getByRole('button', { name: '컴퓨터 열기', exact: true }).click();
    await page.getByRole('button', { name: 'Reviewer와의 대화 열기', exact: true }).click();
    await expect(page.getByRole('log', { name: '봇 간 대화 메시지', exact: true })).toContainText('Please review this local plan.');
    await expect(page.getByRole('log', { name: '봇 간 대화 메시지', exact: true })).toContainText('The plan is ready for review.');
    await expect(page.getByLabel('메시지', { exact: true })).toHaveCount(0);
    expect(new URL(page.url()).searchParams.get('view')).toBe('bot-chat');
    expect(new URL(page.url()).hash).toBe('');
    await page.goBack();
    await expect(page.getByLabel('메시지', { exact: true })).toHaveValue('Unsent original chat draft');
    await page.goForward();
    await expect(page.getByRole('button', { name: '채팅 닫기', exact: true })).toBeVisible();
    await page.reload();
    await expect(page.getByRole('log', { name: '봇 간 대화 메시지', exact: true })).toContainText('The plan is ready for review.');
    await page.getByRole('button', { name: '채팅 닫기', exact: true }).click();
    await expect(page.getByRole('log', { name: '대화 메시지', exact: true })).toContainText('I queued the review request.');
    await page.setViewportSize({ width: 390, height: 844 });
    await page.getByRole('button', { name: '봇 편집', exact: true }).click();
    await page.getByRole('button', { name: '컴퓨터 닫기', exact: true }).first().click();
    await page.getByRole('button', { name: 'Reviewer와의 대화 열기', exact: true }).click();
    await expect(page.getByRole('log', { name: '봇 간 대화 메시지', exact: true })).toBeVisible();
    await expect(page.getByRole('log', { name: '봇 간 대화 메시지', exact: true })).toContainText('The plan is ready for review.');
    await page.getByRole('button', { name: '채팅 닫기', exact: true }).click();
    await expect(page.getByLabel('메시지', { exact: true })).toBeVisible();
  } finally { await app.stop(); await provider.close(); }
});

test('library files open the actual editor and keep unsaved edits guarded across details tabs', async ({ page }, info) => {
  const app = await startTestApp();
  let stage = 0;
  const provider = await startMockProvider({ reply: request => {
    if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: 'Library fixture' };
    return [
      { tool: { name: 'search_tool', arguments: { query: 'workspace write_file', limit: 2 } } },
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__write_file', tool_input: { path: 'nested/readme.md', content: 'Approved library document\n' } } } },
      { text: 'The library document is ready.' },
    ][stage++] ?? { text: 'Complete.' };
  } });
  try {
    await page.setViewportSize({ width: 1040, height: 760 });
    await page.goto(app.url);
    await page.evaluate(async settings => {
      const response = await fetch('/api/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify(settings) });
      if (!response.ok) throw Error(await response.text());
    }, { baseUrl: provider.baseUrl, model: 'local-test-model', workspace: app.workspace });
    await page.getByLabel('메시지', { exact: true }).fill('Update the library document.');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    await page.getByRole('region', { name: '작업 승인 요청', exact: true }).getByRole('button', { name: 'Allow once', exact: true }).click();
    await expect(page.getByRole('log', { name: '대화 메시지', exact: true })).toContainText('The library document is ready.');
    await page.getByRole('button', { name: '봇 편집', exact: true }).click();
    await page.getByRole('tab', { name: '라이브러리', exact: true }).click();
    await page.getByRole('button', { name: 'nested/readme.md', exact: true }).click();
    await expect(page.getByRole('tab', { name: '컴퓨터', exact: true })).toHaveAttribute('aria-selected', 'true');
    await expect(page.getByLabel('파일 내용', { exact: true })).toHaveValue('Approved library document\n');
    await page.getByLabel('파일 내용', { exact: true }).fill('Unsaved library edit');
    page.once('dialog', dialog => dialog.dismiss());
    await page.getByRole('tab', { name: '세부 정보', exact: true }).click();
    await expect(page.getByLabel('파일 내용', { exact: true })).toHaveValue('Unsaved library edit');
    await page.getByRole('button', { name: '파일 저장', exact: true }).click();
    await expect(page.getByText('저장 완료', { exact: true })).toBeVisible();
    expect(await page.locator('.details-computer').evaluate(element => element.scrollWidth <= element.clientWidth)).toBe(true);
    expect(await page.locator('.embedded-files').evaluate(element => element.getBoundingClientRect().width <= element.parentElement!.clientWidth)).toBe(true);
    await page.screenshot({ path: info.outputPath('library-editor-desktop.png') });
    await page.setViewportSize({ width: 390, height: 844 });
    await expect(page.getByLabel('파일 내용', { exact: true })).toHaveValue('Unsaved library edit');
    await page.getByRole('tab', { name: '라이브러리', exact: true }).click();
    await page.getByRole('button', { name: 'nested/readme.md', exact: true }).click();
    await expect(page.getByLabel('파일 내용', { exact: true })).toHaveValue('Unsaved library edit');
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  } finally { await app.stop(); await provider.close(); }
});
