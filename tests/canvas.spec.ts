import { test, expect, type Page } from '@playwright/test';
import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import type { AppState, CanvasInput, Conversation } from '../shared/types';
import { startTestApp } from './app-fixture';
import { startMockProvider, type MockProviderRequest, type MockCompletion } from './mock-provider';

const paragraph = '이 문단은 기능의 의도와 사용 방법을 자세하고 명확하게 설명하기 위해 작성한 제품 안내입니다.';
const shortened = '기능과 사용 방법을 안내합니다.';
const original = `# 함께 쓰는 문서\n\n${paragraph}\n\n마지막 문단은 그대로 유지합니다. 😀`;
async function api(page: Page, url: string, path: string, body: unknown, method = 'POST') {
  const response = await page.request.fetch(url + path, { method, headers: { Origin: url, 'X-Grok-Bot': '1', 'Content-Type': 'application/json' }, data: body });
  expect(response.ok(), await response.text()).toBe(true);
  return response.json();
}
const editor = (page: Page) => page.getByRole('textbox', { name: '캔버스 내용', exact: true });
const panel = (page: Page) => page.getByRole('complementary', { name: '대화 캔버스', exact: true });
async function send(page: Page, text: string) {
  await page.getByRole('textbox', { name: '메시지', exact: true }).fill(text);
  await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
}
async function open(page: Page) { await page.getByRole('button', { name: '캔버스 열기', exact: true }).click(); await expect(editor(page)).toBeVisible(); }
async function save(page: Page, content = original) {
  await editor(page).fill(content);
  await page.getByRole('button', { name: '캔버스 저장', exact: true }).click();
  await expect(panel(page).getByText(/버전 \d+ · 저장됨/)).toBeVisible();
}
async function create(page: Page, url: string, baseUrl: string) {
  const state: AppState = await api(page, url, '/api/settings', { baseUrl, model: 'local-test-model', workspace: '', apiKey: 'canvas-e2e-key' }, 'PUT');
  const { conversation }: { conversation: Conversation } = await api(page, url, '/api/conversations', { botId: state.bots[0].id });
  await page.goto(`${url}/?bot=${state.bots[0].id}&conversation=${conversation.id}`);
  return conversation;
}
function replyFor(url: string) {
  return async (request: MockProviderRequest): Promise<MockCompletion> => {
    if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: 'Canvas test conversation' };
    const start = request.messages.findLastIndex(message => message.role === 'user' && JSON.stringify(message.content).includes('canvas-fixture'));
    if (start < 0) return { text: '문서 편집을 도와드릴게요.' };
    const prompt = JSON.stringify(request.messages[start].content);
    const calls = request.messages.slice(start + 1).flatMap(message => message.tool_calls ?? []) as Array<{ function?: { name?: string; arguments?: string } }>;
    if (!calls.some(call => call.function?.name === 'search_tool')) return { tool: { name: 'search_tool', arguments: { query: 'workspace read_canvas replace_canvas_range write_file', limit: 5 } } };
    const used = calls.filter(call => call.function?.name === 'use_tool').map(call => JSON.parse(call.function!.arguments ?? '{}').tool_name);
    if (!used.includes('workspace__read_canvas')) return { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__read_canvas', tool_input: {} } } };
    const state: AppState = await (await fetch(url + '/api/state')).json();
    const active = state.conversations.find(item => item.run.status === 'running' || item.run.status === 'awaiting_permission');
    if (!active?.canvas) return { text: '캔버스가 없습니다.' };
    if (prompt.includes('export')) {
      if (!used.includes('workspace__write_file')) return { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__write_file', tool_input: { path: '안내.md', content: active.canvas.content, hash: '' } } } };
    } else if (!used.includes('workspace__replace_canvas_range')) {
      return { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__replace_canvas_range', tool_input: { revision: active.canvas.revision, oldText: paragraph, replacement: shortened } } } };
    }
    return { text: '요청 처리를 마쳤습니다.' };
  };
}

test('canvas edits, approved Bot range changes, undo, conflicts, download and approved workspace export', async ({ page }, testInfo) => {
  const app = await startTestApp();
  const provider = await startMockProvider({ reply: replyFor(app.url) });
  const errors: string[] = []; page.on('pageerror', error => errors.push(error.message));
  try {
    await page.setViewportSize({ width: 1040, height: 760 });
    const conversation = await create(page, app.url, provider.baseUrl);
    await open(page);
    await page.getByRole('textbox', { name: '캔버스 제목', exact: true }).fill('안내.md');
    await save(page);
    await page.getByRole('button', { name: '마크다운 미리보기', exact: true }).click();
    await expect(page.getByRole('heading', { name: '함께 쓰는 문서', exact: true })).toBeVisible();
    await page.getByRole('button', { name: '마크다운 미리보기', exact: true }).click();
    // No workspace is connected; an in-app edit still uses a fresh approval.
    await send(page, 'canvas-fixture shorten');
    await expect(page.getByRole('region', { name: '작업 승인 요청', exact: true })).toBeVisible({ timeout: 60000 });
    await expect(editor(page)).toHaveValue(original);
    await page.getByRole('button', { name: 'Replace canvas once', exact: true }).click();
    await expect(editor(page)).toHaveValue(original.replace(paragraph, shortened));
    await expect(page.getByText('작업 완료', { exact: true })).toBeVisible();
    await page.getByRole('button', { name: '이전 버전으로 되돌리기', exact: true }).click();
    await expect(editor(page)).toHaveValue(original);
    await expect(panel(page).getByText('버전 3 · 저장됨')).toBeVisible();
    await page.getByRole('button', { name: '캔버스 버전 기록', exact: true }).click();
    await page.getByLabel('캔버스 버전', { exact: true }).selectOption('2');
    await expect(editor(page)).toHaveValue(original.replace(paragraph, shortened));
    await expect(editor(page)).toHaveAttribute('readonly', '');
    await page.getByLabel('캔버스 버전', { exact: true }).selectOption('');
    // A remote saved version never silently overwrites a dirty local draft.
    await editor(page).fill(original + '\n내 초안');
    const body: CanvasInput = { revision: 3, title: '안내.md', language: 'markdown', content: original + '\n다른 창' };
    await api(page, app.url, `/api/conversations/${conversation.id}/canvas`, body, 'PUT');
    await expect(panel(page).getByText('저장된 내용이나 보호 설정이 변경되었습니다. 내 초안은 유지됩니다.')).toBeVisible();
    await expect(editor(page)).toHaveValue(original + '\n내 초안');
    await expect(page.getByRole('button', { name: '캔버스 저장', exact: true })).toBeDisabled();
    page.once('dialog', dialog => dialog.accept());
    await page.getByRole('button', { name: '최신 버전 다시 열기', exact: true }).click();
    await expect(editor(page)).toHaveValue(body.content);
    // Editing during the human review makes its original revision stale; approving cannot overwrite.
    await send(page, 'canvas-fixture stale');
    await expect(page.getByRole('region', { name: '작업 승인 요청', exact: true })).toBeVisible({ timeout: 60000 });
    await save(page, original + '\n승인 중 내 편집');
    await page.getByRole('button', { name: 'Replace canvas once', exact: true }).click();
    await expect(page.getByText('작업 완료', { exact: true })).toBeVisible();
    await expect(editor(page)).toHaveValue(original + '\n승인 중 내 편집');
    const downloadPromise = page.waitForEvent('download');
    await page.getByRole('button', { name: '캔버스 파일로 다운로드', exact: true }).click();
    const download = await downloadPromise;
    expect(download.suggestedFilename()).toBe('안내.md');
    expect(await readFile((await download.path())!, 'utf8')).toBe(original + '\n승인 중 내 편집');
    // Configure only our disposable workspace, then use the existing model file approval.
    await api(page, app.url, '/api/settings', { baseUrl: provider.baseUrl, model: 'local-test-model', workspace: app.workspace }, 'PUT');
    await page.getByRole('button', { name: '작업 폴더에 저장 요청', exact: true }).click();
    await expect(page.getByRole('textbox', { name: '메시지', exact: true })).toHaveValue(/기존 파일 쓰기 승인/);
    await page.getByRole('textbox', { name: '메시지', exact: true }).fill('canvas-fixture export');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    await expect(page.getByRole('region', { name: '작업 승인 요청', exact: true })).toBeVisible({ timeout: 60000 });
    await page.getByRole('button', { name: 'Allow once', exact: true }).click();
    await expect(page.getByText('작업 완료', { exact: true })).toBeVisible();
    expect(await readFile(join(app.workspace, '안내.md'), 'utf8')).toBe(original + '\n승인 중 내 편집');
    await page.reload(); await open(page);
    await expect(editor(page)).toHaveValue(original + '\n승인 중 내 편집');
    await page.screenshot({ path: testInfo.outputPath('canvas-desktop.png') });
    expect(errors).toEqual([]);
  } finally { await app.stop(); await provider.close(); }
});

test('mobile canvas keeps its draft when reviewing chat, rejects discarded navigation, and safely previews code', async ({ page }) => {
  const app = await startTestApp(); const provider = await startMockProvider();
  try {
    await page.setViewportSize({ width: 390, height: 844 });
    await create(page, app.url, provider.baseUrl);
    await open(page); await save(page);
    await editor(page).fill(original + '\n아직 저장하지 않은 초안');
    await page.getByRole('button', { name: '대화로 돌아가기', exact: true }).click();
    await expect(panel(page)).not.toBeVisible();
    await page.getByRole('button', { name: '캔버스 열기', exact: true }).click();
    await expect(editor(page)).toHaveValue(original + '\n아직 저장하지 않은 초안');
    page.once('dialog', dialog => dialog.dismiss());
    await page.getByRole('button', { name: '캔버스 닫기', exact: true }).last().click();
    await expect(editor(page)).toBeVisible();
    await page.getByRole('button', { name: '캔버스 저장', exact: true }).click();
    await expect(panel(page).getByText('버전 2 · 저장됨')).toBeVisible();
    await page.getByLabel('캔버스 형식', { exact: true }).selectOption('html');
    await save(page, '<script>window.canvasExecuted = true</script>');
    await expect(page.getByRole('button', { name: '마크다운 미리보기', exact: true })).toBeDisabled();
    expect(await page.evaluate(() => 'canvasExecuted' in window)).toBe(false);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    await page.setViewportSize({ width: 320, height: 740 });
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    await page.keyboard.press('Escape');
    await expect(panel(page)).not.toBeVisible();
  } finally { await app.stop(); await provider.close(); }
});

test('secret projection changes refresh the same revision without saving masked placeholders', async ({ page }) => {
  const app = await startTestApp(); const provider = await startMockProvider();
  try {
    await create(page, app.url, provider.baseUrl); await open(page);
    const content = 'protected-value 문서의 본문';
    await save(page, content);
    const configureKey = (apiKey: string) => api(page, app.url, '/api/settings', { baseUrl: provider.baseUrl, model: 'local-test-model', workspace: '', apiKey }, 'PUT');
    await configureKey('protected-value');
    await expect(editor(page)).toHaveValue('[redacted] 문서의 본문');
    await expect(editor(page)).toHaveAttribute('readonly', '');
    await configureKey('different-fixture-key');
    await expect(editor(page)).toHaveValue(content);
    await expect(editor(page)).not.toHaveAttribute('readonly', '');
    await save(page, content + ' 수정');
    await expect(panel(page).getByText('버전 2 · 저장됨')).toBeVisible();
    await editor(page).fill(content + ' 저장하지 않은 초안');
    await configureKey('protected-value');
    await expect(editor(page)).toHaveValue('[redacted] 문서의 본문 수정');
    await expect(page.getByRole('button', { name: '캔버스 저장', exact: true })).toBeDisabled();
    await configureKey('different-fixture-key');
    await expect(editor(page)).toHaveValue(content + ' 저장하지 않은 초안');
  } finally { await app.stop(); await provider.close(); }
});

test('group and thread canvases keep their exact conversation ownership', async ({ page }) => {
  test.skip(process.env.GROK_BOT_BACKEND !== 'rust', 'Group routes belong to the Rust backend during migration.');
  const app = await startTestApp(); const provider = await startMockProvider();
  try {
    await page.setViewportSize({ width: 1440, height: 900 });
    const parent = await create(page, app.url, provider.baseUrl);
    await send(page, '스레드에 나눌 메시지'); await expect(page.getByText('작업 완료', { exact: true })).toBeVisible();
    const state: AppState = await (await page.request.get(app.url + '/api/state')).json();
    const message = state.conversations.find(item => item.id === parent.id)!.messages[0];
    const { conversation: thread }: { conversation: Conversation } = await api(page, app.url, `/api/conversations/${parent.id}/threads`, { messageId: message.id });
    await page.goto(`${app.url}/?bot=${parent.botId}&conversation=${parent.id}&thread=${thread.id}`);
    await page.getByRole('complementary', { name: '스레드', exact: true }).getByRole('button', { name: '캔버스 열기', exact: true }).click();
    await save(page, '스레드만의 캔버스');
    await page.getByRole('button', { name: '대화로 돌아가기', exact: true }).click();
    await expect(page.getByRole('textbox', { name: '스레드 메시지', exact: true })).toHaveCount(1);
    await expect(page.getByRole('textbox', { name: '스레드 메시지', exact: true })).toBeVisible();
    const bots: AppState = await api(page, app.url, '/api/bots', { name: '다른 Bot' });
    const { group } = await api(page, app.url, '/api/groups', { name: '편집 팀', memberBotIds: bots.bots.slice(0, 2).map(bot => bot.id) });
    const { conversation: groupChat }: { conversation: Conversation } = await api(page, app.url, `/api/groups/${group.id}/conversations`, {});
    await page.goto(`${app.url}/?group=${group.id}&conversation=${groupChat.id}`);
    await open(page); await save(page, '그룹만의 캔버스');
    const final: AppState = await (await page.request.get(app.url + '/api/state')).json();
    expect(final.conversations.find(item => item.id === thread.id)?.canvas?.content).toBe('스레드만의 캔버스');
    expect(final.conversations.find(item => item.id === groupChat.id)?.canvas?.content).toBe('그룹만의 캔버스');
    expect(final.conversations.find(item => item.id === parent.id)?.canvas).toBeUndefined();
  } finally { await app.stop(); await provider.close(); }
});

test('canvas creation before the first message binds to a persistent conversation', async ({ page }) => {
  const app = await startTestApp();
  try {
    await page.goto(app.url);
    await page.getByRole('textbox', { name: '메시지', exact: true }).fill('아직 보내지 않은 대화 초안');
    await open(page);
    await editor(page).fill('모델이나 작업 폴더 연결 없이 저장합니다.');
    await editor(page).press('Control+s');
    await expect(panel(page).getByText('버전 1 · 저장됨')).toBeVisible();
    await expect(editor(page)).toBeFocused();
    expect(new URL(page.url()).searchParams.get('conversation')).toBeTruthy();
    await page.getByRole('button', { name: '대화로 돌아가기', exact: true }).click();
    await expect(page.getByRole('textbox', { name: '메시지', exact: true })).toHaveValue('아직 보내지 않은 대화 초안');
    await page.reload(); await open(page);
    await expect(editor(page)).toHaveValue('모델이나 작업 폴더 연결 없이 저장합니다.');
  } finally { await app.stop(); }
});
