import { test, expect, type Page } from '@playwright/test';
import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import type { AppState } from '../shared/types';
import { startTestApp } from './app-fixture';
import { startMockProvider, type MockCompletion, type MockProviderRequest } from './mock-provider';

/** Writes notes.txt with the text after "fixture write:" through the workspace broker. */
function reply(request: MockProviderRequest): MockCompletion {
  if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: 'Rule test conversation' };
  const last = request.messages.findLastIndex(message => message.role === 'user' && JSON.stringify(message.content).includes('fixture write:'));
  if (last < 0) return { text: '규칙 테스트 응답입니다.' };
  const content = /fixture write: ([a-z ]+)/.exec(JSON.stringify(request.messages[last].content))?.[1] ?? 'missing';
  const calls = request.messages.slice(last + 1).flatMap(message => message.tool_calls ?? []) as Array<{ function?: { name?: string } }>;
  if (!calls.some(call => call.function?.name === 'search_tool')) return { tool: { name: 'search_tool', arguments: { query: 'workspace write_file', limit: 3 } } };
  if (!calls.some(call => call.function?.name === 'use_tool')) return { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__write_file', tool_input: { path: 'notes.txt', content: `${content}\n` } } } };
  return { text: '규칙 쓰기 작업이 끝났습니다.' };
}

const state = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());

async function configure(page: Page, baseUrl: string, workspace: string) {
  const status = await page.evaluate(async ({ baseUrl, workspace }) => {
    const response = await fetch('/api/settings', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' },
      body: JSON.stringify({ baseUrl, model: 'local-test-model', workspace }),
    });
    return response.status;
  }, { baseUrl, workspace });
  expect(status).toBe(200);
}

async function openPermissions(page: Page) {
  await page.getByRole('button', { name: '설정', exact: true }).click();
  const dialog = page.getByRole('dialog', { name: '설정', exact: true });
  await dialog.getByRole('button', { name: '권한', exact: true }).click();
  await expect(dialog.getByRole('heading', { name: '권한', exact: true })).toBeVisible();
  return dialog;
}

async function send(page: Page, text: string) {
  await page.getByRole('textbox', { name: '메시지', exact: true }).fill(text);
  await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
}

test('permission rules are added, edited, and deleted in settings; allowed writes skip the card and are labeled', async ({ page }, testInfo) => {
  const app = await startTestApp();
  const provider = await startMockProvider({ reply });
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  try {
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto(app.url);
    await expect(page.getByRole('heading', { name: 'Grok', exact: true })).toBeVisible();
    await configure(page, provider.baseUrl, app.workspace);

    let dialog = await openPermissions(page);
    await expect(dialog.getByText('Grok Bot이 위험할 수 있는 작업을 실행하기 전에 얼마나 꼼꼼히 확인할지 선택해요')).toBeVisible();
    await expect(dialog.getByText(/Grok Bot은 각 작업을 실행하기 전에 확인하고 필요할 때 먼저 물어봐요/)).toBeVisible();
    await expect(dialog.getByText('묻지 않고 허용 규칙이 없어요.', { exact: true })).toBeVisible();
    await dialog.getByRole('button', { name: '묻지 않고 허용 규칙 추가', exact: true }).click();
    const editor = dialog.getByRole('group', { name: '규칙 추가', exact: true });
    await expect(editor.getByLabel('작업', { exact: true })).toBeFocused();
    await editor.getByLabel('작업', { exact: true }).selectOption('file_write');
    await expect(editor.getByLabel('적용할 Bot', { exact: true })).toHaveValue('');
    await expect(editor.getByRole('radio', { name: /묻지 않고 허용/ })).toBeChecked();
    await expect(editor.getByText('작업이 이 규칙과 일치하면 자동으로 계속해요', { exact: true })).toBeVisible();
    await page.screenshot({ path: testInfo.outputPath('permission-editor.png') });
    const saved = page.waitForResponse(response => new URL(response.url()).pathname === '/api/permission-rules' && response.request().method() === 'POST');
    await editor.getByRole('button', { name: '규칙 저장', exact: true }).click();
    expect((await saved).status()).toBe(201);
    await expect(editor).not.toBeVisible();
    const allowList = dialog.getByRole('region', { name: '묻지 않고 허용 규칙', exact: true });
    const row = allowList.getByRole('group', { name: '작업 폴더의 파일 만들기·바꾸기 · 모든 Bot', exact: true });
    await expect(row).toBeVisible();
    await expect(row.getByText('Grok Bot이 다음을 하려고 할 때:', { exact: true })).toBeVisible();
    const rules = (await state(page)).permissionRules ?? [];
    expect(rules).toEqual([{ id: expect.any(String), category: 'file_write', action: 'allow', createdAt: expect.any(String), updatedAt: expect.any(String) }]);
    // A second rule for the same scope is refused before saving.
    await dialog.getByRole('button', { name: '항상 묻기 규칙 추가', exact: true }).click();
    await editor.getByLabel('작업', { exact: true }).selectOption('file_write');
    await expect(editor.getByText(/같은 작업과 Bot에 대한 규칙이 이미 있어요/)).toBeVisible();
    await expect(editor.getByRole('button', { name: '규칙 저장', exact: true })).toBeDisabled();
    await editor.getByRole('button', { name: '규칙 수정 취소', exact: true }).click();
    await dialog.getByRole('button', { name: '설정 닫기', exact: true }).click();

    // Allowed: the write lands without an approval card and the activity says why.
    await send(page, 'fixture write: allowed by my rule');
    await expect(page.getByText('규칙 쓰기 작업이 끝났습니다.', { exact: true })).toBeVisible({ timeout: 30_000 });
    expect(await readFile(join(app.workspace, 'notes.txt'), 'utf8')).toBe('allowed by my rule\n');
    await expect(page.getByRole('region', { name: '작업 승인 요청', exact: true })).toHaveCount(0);
    const badge = page.locator('.tool-card').filter({ hasText: 'Write notes.txt' }).getByText('내 규칙으로 자동 승인됨', { exact: true });
    await expect(badge).toBeVisible();
    const tool = (await state(page)).conversations[0].messages[1].tools.find(item => item.title === 'Write notes.txt');
    expect(tool?.approvedByRule).toBe(rules[0].id);
    await page.screenshot({ path: testInfo.outputPath('permission-auto-approved.png') });

    // Edit the action to "항상 묻기": the next write asks again.
    dialog = await openPermissions(page);
    await allowList.getByLabel('규칙의 동작 편집', { exact: true }).selectOption('ask');
    const askList = dialog.getByRole('region', { name: '항상 묻기 규칙', exact: true });
    await expect(askList.getByRole('group', { name: '작업 폴더의 파일 만들기·바꾸기 · 모든 Bot', exact: true })).toBeVisible();
    await expect.poll(async () => (await state(page)).permissionRules?.[0].action).toBe('ask');
    await dialog.getByRole('button', { name: '설정 닫기', exact: true }).click();
    await send(page, 'fixture write: denied after edit');
    await expect(page.getByRole('region', { name: '작업 승인 요청', exact: true })).toBeVisible({ timeout: 30_000 });
    await page.getByRole('button', { name: 'Deny', exact: true }).click();
    await expect(page.getByText('규칙 쓰기 작업이 끝났습니다.', { exact: true })).toHaveCount(2, { timeout: 30_000 });
    expect(await readFile(join(app.workspace, 'notes.txt'), 'utf8')).toBe('allowed by my rule\n');

    // Delete the rule.
    dialog = await openPermissions(page);
    page.once('dialog', confirm => confirm.accept());
    await askList.getByRole('button', { name: '규칙 삭제', exact: true }).click();
    await expect(dialog.getByText('항상 묻기 규칙이 없어요. 규칙이 없는 작업은 기본으로 먼저 물어봐요.', { exact: true })).toBeVisible();
    await expect.poll(async () => (await state(page)).permissionRules ?? []).toEqual([]);

    // The editor fits a phone viewport.
    await page.setViewportSize({ width: 390, height: 844 });
    await dialog.getByRole('button', { name: '항상 묻기 규칙 추가', exact: true }).click();
    await expect(editor.getByRole('button', { name: '규칙 저장', exact: true })).toBeVisible();
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    await page.screenshot({ path: testInfo.outputPath('permission-editor-mobile.png') });
    expect(errors).toEqual([]);
  } finally {
    await app.stop();
    await provider.close();
  }
});

test('a client that is not on the server loopback can view and tighten rules but not allow without asking', async ({ page }) => {
  const app = await startTestApp();
  try {
    // The server decides `canAutoAllow` from the connection (enforcement is covered by the Rust
    // HTTP tests); here the editor's LAN presentation is exercised with that flag off.
    await page.route('**/api/permission-rules', route =>
      route.request().method() === 'GET' ? route.fulfill({ json: { rules: [], canAutoAllow: false } }) : route.continue());
    await page.goto(app.url);
    const dialog = await openPermissions(page);
    await expect(dialog.getByRole('note')).toContainText('localhost 연결이 아니에요');
    await expect(dialog.getByRole('button', { name: '묻지 않고 허용 규칙 추가', exact: true })).toBeDisabled();
    await dialog.getByRole('button', { name: '항상 묻기 규칙 추가', exact: true }).click();
    const editor = dialog.getByRole('group', { name: '규칙 추가', exact: true });
    await expect(editor.getByRole('radio', { name: /묻지 않고 허용/ })).toBeDisabled();
    const bot = (await state(page)).bots[0];
    await editor.getByLabel('작업', { exact: true }).selectOption('bot_message');
    await editor.getByLabel('적용할 Bot', { exact: true }).selectOption(bot.id);
    await editor.getByRole('button', { name: '규칙 저장', exact: true }).click();
    const row = dialog.getByRole('region', { name: '항상 묻기 규칙', exact: true }).getByRole('group', { name: '다른 Bot에게 메시지 보내기 · Grok', exact: true });
    await expect(row).toBeVisible();
    await expect(row.getByRole('button', { name: '규칙 삭제', exact: true })).toBeDisabled();
    await expect(row.getByRole('option', { name: '묻지 않고 허용', exact: true })).toBeDisabled();
    expect((await state(page)).permissionRules).toEqual([expect.objectContaining({ category: 'bot_message', action: 'ask', botId: bot.id })]);
  } finally {
    await app.stop();
  }
});
