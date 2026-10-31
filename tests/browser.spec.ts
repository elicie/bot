import { test, expect, type Page } from '@playwright/test';
import { readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { startTestApp } from './app-fixture';
import { startMockProvider, type MockProviderRequest, type MockCompletion } from './mock-provider';

function reply(request: MockProviderRequest): MockCompletion {
  if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: 'Local test conversation' };
  const last = request.messages.findLastIndex(message => message.role === 'user' && JSON.stringify(message.content).includes('fixture'));
  if (last < 0) return { text: 'OpenAI 호환 대화 응답이 도착했습니다.' };
  const text = JSON.stringify(request.messages[last].content);
  if (text.includes('slow')) return { text: '중단 검증 응답' };
  const calls = request.messages.slice(last + 1).flatMap(message => message.tool_calls ?? []) as Array<{ function?: { name?: string } }>;
  if (!calls.some(call => call.function?.name === 'search_tool')) return { tool: { name: 'search_tool', arguments: { query: 'workspace write_file', limit: 3 } } };
  if (!calls.some(call => call.function?.name === 'use_tool')) return { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__write_file', tool_input: { path: 'notes.txt', content: text.includes('denied') ? 'denied content\n' : 'approved by browser\n' } } } };
  return { text: '브라우저 파일 작업 처리가 끝났습니다.' };
}

async function configure(page: Page, baseUrl: string, workspace: string) {
  await page.getByRole('button', { name: '설정', exact: true }).click();
  await page.getByRole('button', { name: '모델 설정', exact: true }).first().click();
  await page.getByRole('textbox', { name: 'Base URL', exact: true }).fill(baseUrl);
  await page.getByLabel('Model ID', { exact: true }).fill('local-test-model');
  await page.getByRole('textbox', { name: 'API key', exact: true }).fill('browser-test-secret');
  await page.getByRole('button', { name: '연결 테스트', exact: true }).click();
  await expect(page.getByText(/연결 성공/)).toBeVisible();
  await page.getByRole('button', { name: '로컬 파일 설정', exact: true }).click();
  await page.getByRole('textbox', { name: '로컬 작업 폴더 경로', exact: true }).fill(workspace);
  await page.getByRole('button', { name: '설정 저장', exact: true }).click();
  await expect(page.getByText('설정을 저장했습니다')).toBeVisible();
  await page.getByRole('button', { name: '설정 닫기', exact: true }).click();
}

async function send(page: Page, text: string) {
  await page.getByRole('textbox', { name: '메시지', exact: true }).fill(text);
  await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
}

test('browser configures provider, edits local files, approves and denies AI writes, restores history, and works on mobile', async ({ page }, testInfo) => {
  const app = await startTestApp();
  const provider = await startMockProvider({ reply });
  const consoleErrors: string[] = [];
  page.on('pageerror', error => consoleErrors.push(error.message));
  try {
    await page.setViewportSize({ width: 1440, height: 1000 });
    await page.goto(app.url);
    await expect(page.getByRole('heading', { name: 'Grok', exact: true })).toBeVisible();
    await configure(page, provider.baseUrl, app.workspace);
    await page.getByRole('button', { name: '컴퓨터 열기', exact: true }).click();
    await page.getByRole('button', { name: '폴더 empty', exact: true }).click();
    await expect(page.getByText('이 폴더는 비어 있습니다.', { exact: true })).toBeVisible();
    await page.getByRole('button', { name: '작업 폴더 루트', exact: true }).click();
    await page.getByRole('button', { name: '파일 binary.bin', exact: true }).click();
    await expect(page.getByRole('alert')).toContainText(/Binary/);
    await page.getByRole('button', { name: '폴더 nested', exact: true }).click();
    await page.getByRole('button', { name: '파일 readme.md', exact: true }).click();
    await expect(page.getByRole('textbox', { name: '파일 내용', exact: true })).toHaveValue('# Local workspace\n');
    await page.getByRole('button', { name: '작업 폴더 루트', exact: true }).click();
    await page.getByRole('button', { name: '파일 notes.txt', exact: true }).click();
    const editor = page.getByRole('textbox', { name: '파일 내용', exact: true });
    await editor.fill('manually saved in browser\n');
    await page.getByRole('button', { name: '파일 저장', exact: true }).click();
    await expect(page.getByText('저장 완료', { exact: true })).toBeVisible();
    expect(await readFile(join(app.workspace, 'notes.txt'), 'utf8')).toBe('manually saved in browser\n');
    await writeFile(join(app.workspace, 'notes.txt'), 'external editor wins\n');
    await editor.fill('stale browser content\n');
    await page.getByRole('button', { name: '파일 저장', exact: true }).click();
    await expect(page.getByText(/파일이 다른 곳에서 변경되었습니다/)).toBeVisible();
    expect(await readFile(join(app.workspace, 'notes.txt'), 'utf8')).toBe('external editor wins\n');
    page.once('dialog', dialog => dialog.accept());
    await page.getByRole('button', { name: '파일 새로고침', exact: true }).click();
    await expect(editor).toHaveValue('external editor wins\n');
    await send(page, 'fixture approved write');
    await expect(page.getByRole('region', { name: '작업 승인 요청', exact: true })).toBeVisible({ timeout: 30_000 });
    expect(await readFile(join(app.workspace, 'notes.txt'), 'utf8')).toBe('external editor wins\n');
    await page.getByRole('button', { name: 'Allow once', exact: true }).click();
    await expect(page.getByText('작업 완료', { exact: true })).toBeVisible({ timeout: 30_000 });
    expect(await readFile(join(app.workspace, 'notes.txt'), 'utf8')).toBe('approved by browser\n');
    await expect(editor).toHaveValue('approved by browser\n');
    await send(page, 'fixture denied write');
    await expect(page.getByRole('region', { name: '작업 승인 요청', exact: true })).toBeVisible({ timeout: 30_000 });
    await page.getByRole('button', { name: 'Deny', exact: true }).click();
    await expect(page.getByText('작업 완료', { exact: true })).toBeVisible({ timeout: 30_000 });
    expect(await readFile(join(app.workspace, 'notes.txt'), 'utf8')).toBe('approved by browser\n');
    await expect(editor).toHaveValue('approved by browser\n');
    await page.getByRole('button', { name: '컴퓨터 닫기', exact: true }).last().click();
    const firstConversationHash = new URL(page.url()).hash;
    await page.reload();
    await expect(page.getByRole('log', { name: '대화 메시지' }).getByText('fixture denied write', { exact: true })).toBeVisible();
    await page.getByRole('button', { name: '봇 추가', exact: true }).click();
    await page.getByRole('textbox', { name: '봇 이름', exact: true }).fill('파일 도우미');
    await page.getByRole('textbox', { name: '봇 지침', exact: true }).fill('로컬 파일 작업을 도와주세요.');
    await page.getByRole('dialog', { name: '봇 추가' }).getByRole('button', { name: '봇 추가', exact: true }).click();
    await expect(page.getByRole('heading', { name: '파일 도우미', exact: true })).toBeVisible();
    await send(page, '새 봇 연결 확인');
    await expect(page.getByRole('log').getByText('OpenAI 호환 대화 응답이 도착했습니다.', { exact: true })).toBeVisible({ timeout: 30_000 });
    await page.getByRole('textbox', { name: '봇 검색', exact: true }).fill('존재하지않는봇');
    await expect(page.getByText('검색 결과가 없습니다.')).toBeVisible();
    await page.getByRole('button', { name: '검색 지우기', exact: true }).click();
    await page.goto(`${app.url}/${firstConversationHash}`);
    await expect(page.getByRole('log').getByText('fixture denied write', { exact: true })).toBeVisible();
    await page.screenshot({ path: testInfo.outputPath('desktop-chat.png'), fullPage: true });
    await page.getByRole('button', { name: '설정', exact: true }).click();
    await page.getByRole('button', { name: '모델 설정', exact: true }).first().click();
    await expect(page.getByRole('textbox', { name: 'API key', exact: true })).toHaveValue('');
    await expect(page.getByText('키 저장됨', { exact: true })).toBeVisible();
    await page.screenshot({ path: testInfo.outputPath('desktop-settings.png'), fullPage: true });
    await page.getByRole('button', { name: '설정 닫기', exact: true }).click();
    await page.setViewportSize({ width: 390, height: 844 });
    await page.getByRole('button', { name: '봇 목록 열기', exact: true }).click();
    await expect(page.getByRole('textbox', { name: '봇 검색', exact: true })).toBeVisible();
    await page.keyboard.press('Escape');
    await expect(page.getByRole('button', { name: '봇 목록 열기', exact: true })).toBeFocused();
    await expect(page.getByRole('textbox', { name: '봇 검색', exact: true })).not.toBeVisible();
    await page.getByRole('button', { name: '컴퓨터 열기', exact: true }).click();
    await page.getByRole('button', { name: '파일 notes.txt', exact: true }).click();
    await expect(editor).toHaveValue('approved by browser\n');
    await page.screenshot({ path: testInfo.outputPath('mobile-files.png'), fullPage: true });
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
    await page.getByRole('button', { name: '컴퓨터 닫기', exact: true }).last().click();
    expect(consoleErrors).toEqual([]);
    expect(provider.requests.some(request => request.model === 'local-test-model')).toBe(true);
  } finally {
    await app.stop();
    await provider.close();
  }
});

test('browser cancels an active run and shows provider failure without changing local files', async ({ page }) => {
  const app = await startTestApp();
  const provider = await startMockProvider({ delayMs: 20_000, reply });
  try {
    await page.goto(app.url);
    await configure(page, provider.baseUrl, app.workspace);
    await send(page, 'fixture slow cancellation');
    await expect(page.getByRole('button', { name: '작업 중단', exact: true })).toBeVisible();
    await page.getByRole('button', { name: '작업 중단', exact: true }).click();
    await expect(page.getByText('작업을 중단했습니다.', { exact: true })).toBeVisible();
    expect(await readFile(join(app.workspace, 'notes.txt'), 'utf8')).toBe('original local content\n');
    await provider.close();
    await page.getByRole('button', { name: '설정', exact: true }).click();
    await page.getByRole('button', { name: '모델 설정', exact: true }).first().click();
    await page.getByRole('button', { name: '연결 테스트', exact: true }).click();
    await expect(page.getByRole('alert')).toContainText(/Provider|서버|연결/);
    await page.getByRole('button', { name: '설정 닫기', exact: true }).click();
    await send(page, '연결이 끊긴 모델 확인');
    await expect(page.getByRole('alert')).toBeVisible({ timeout: 30_000 });
    expect(await readFile(join(app.workspace, 'notes.txt'), 'utf8')).toBe('original local content\n');
  } finally {
    await app.stop();
    await provider.close();
  }
});
