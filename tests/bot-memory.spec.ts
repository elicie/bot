import { test, expect, type Page } from '@playwright/test';
import { startTestApp } from './app-fixture';
import { startMockProvider, type MockCompletion, type MockProviderRequest } from './mock-provider';
import type { AppState } from '../shared/types';

// Bot memory, knowledge, and templates are implemented by the Rust server only.
test.skip(process.env.GROK_BOT_BACKEND !== 'rust', 'Bot memory runs against the Rust server.');

const snapshot = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());
const details = (page: Page) => page.getByRole('complementary', { name: '대화 세부 정보', exact: true });
const memoryList = (page: Page) => details(page).getByRole('list', { name: /의 메모리$/ });
async function openDetails(page: Page) {
  await page.getByRole('button', { name: '봇 편집', exact: true }).first().click();
  await expect(details(page).getByRole('heading', { name: '메모리', exact: true })).toBeVisible();
}

test('details pane adds, imports after review, and deletes bot memories', async ({ page }, info) => {
  const app = await startTestApp();
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  try {
    await page.setViewportSize({ width: 1280, height: 860 });
    await page.goto(app.url);
    await openDetails(page);
    await expect(details(page)).toContainText('이 Bot이 기억하는 내용');
    await expect(details(page)).toContainText('아직 메모리가 없어요');
    await details(page).getByLabel('새 메모리', { exact: true }).fill('답변은 한국어로');
    await details(page).getByRole('button', { name: '메모리 추가', exact: true }).click();
    await expect(memoryList(page)).toContainText('답변은 한국어로');
    await expect.poll(async () => (await snapshot(page)).bots[0].memories?.map(memory => [memory.text, memory.source])).toEqual([['답변은 한국어로', 'user']]);
    await details(page).getByLabel('새 메모리', { exact: true }).fill('답변은   한국어로');
    await details(page).getByRole('button', { name: '메모리 추가', exact: true }).click();
    await expect(details(page).getByText('이미 기억하고 있는 내용이에요.', { exact: true })).toBeVisible();

    await details(page).getByRole('button', { name: '가져오기', exact: true }).click();
    const dialog = page.getByRole('dialog', { name: 'Grok Bot으로 메모리 가져오기', exact: true });
    await expect(dialog).toContainText('결과를 아래에 붙여넣어 Grok Bot의 메모리에 추가하세요');
    await dialog.getByLabel('가져올 메모리', { exact: true }).fill('# Memories\n- 답변은 한국어로\n- 서울에 산다\n  지하철로 출근한다\n1. Rust를 매일 쓴다\n2. 고양이 이름은 보리\n');
    await dialog.getByRole('button', { name: '항목 확인', exact: true }).click();
    const items = dialog.getByRole('list', { name: '가져올 메모리 항목', exact: true });
    await expect(items.getByRole('checkbox')).toHaveCount(4);
    await expect(items.getByRole('checkbox', { name: /답변은 한국어로/ })).not.toBeChecked();
    await expect(items).toContainText('이미 있음');
    await expect(items.getByRole('checkbox', { name: '서울에 산다 지하철로 출근한다' })).toBeChecked();
    await items.getByRole('checkbox', { name: '고양이 이름은 보리' }).uncheck();
    await page.screenshot({ path: info.outputPath('memory-import-review.png') });
    expect((await snapshot(page)).bots[0].memories).toHaveLength(1);
    await dialog.getByRole('button', { name: '메모리에 추가 (2)', exact: true }).click();
    await expect(dialog).toBeHidden();
    await expect(details(page).getByText('메모리 2개를 추가했어요.', { exact: true })).toBeVisible();
    await expect.poll(async () => (await snapshot(page)).bots[0].memories?.map(memory => [memory.text, memory.source])).toEqual([
      ['답변은 한국어로', 'user'], ['서울에 산다 지하철로 출근한다', 'import'], ['Rust를 매일 쓴다', 'import'],
    ]);

    await page.reload();
    await openDetails(page);
    await expect(memoryList(page).getByRole('listitem')).toHaveCount(3);
    await page.screenshot({ path: info.outputPath('memory-details.png') });
    page.once('dialog', prompt => prompt.dismiss());
    await memoryList(page).getByRole('button', { name: '메모리 삭제: Rust를 매일 쓴다', exact: true }).click();
    await expect(memoryList(page).getByRole('listitem')).toHaveCount(3);
    page.once('dialog', prompt => prompt.accept());
    await memoryList(page).getByRole('button', { name: '메모리 삭제: Rust를 매일 쓴다', exact: true }).click();
    await expect(memoryList(page).getByRole('listitem')).toHaveCount(2);
    expect((await snapshot(page)).bots[0].memories?.map(memory => memory.text)).toEqual(['답변은 한국어로', '서울에 산다 지하철로 출근한다']);
    expect(errors).toEqual([]);
  } finally { await app.stop(); }
});

test('a model-proposed memory is saved only after browser approval and reaches the next session rules', async ({ page }) => {
  const app = await startTestApp();
  let stage = 0;
  const inference: MockProviderRequest[] = [];
  const memory = '좋아하는 색은 청록색이다';
  const provider = await startMockProvider({ reply: request => {
    if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: 'Memory browser fixture' };
    inference.push(request);
    const plan: MockCompletion[] = [
      { tool: { name: 'search_tool', arguments: { query: 'workspace save_memory', limit: 2 } } },
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__save_memory', tool_input: { text: '이 요청은 거절된다' } } } },
      { text: 'The memory was not saved.' },
      { tool: { name: 'search_tool', arguments: { query: 'workspace save_memory', limit: 2 } } },
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__save_memory', tool_input: { text: memory } } } },
      { text: 'I will remember that.' },
      { text: 'Your favorite color is teal.' },
    ];
    return plan[stage++] ?? { text: 'Complete.' };
  } });
  try {
    await page.setViewportSize({ width: 1280, height: 860 });
    await page.goto(app.url);
    await page.evaluate(async baseUrl => {
      const response = await fetch('/api/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ baseUrl, model: 'local-test-model', workspace: '' }) });
      if (!response.ok) throw Error(await response.text());
    }, provider.baseUrl);
    await openDetails(page);
    const log = page.getByRole('log', { name: '대화 메시지', exact: true });
    const approval = page.getByRole('region', { name: '작업 승인 요청', exact: true });

    await page.getByLabel('메시지', { exact: true }).fill('Remember something I will reject.');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    await expect(approval).toContainText('이 요청은 거절된다');
    await approval.getByRole('button', { name: 'Deny', exact: true }).click();
    await expect(log).toContainText('The memory was not saved.');
    await expect.poll(async () => (await snapshot(page)).conversations[0].run.status).toBe('completed');
    expect((await snapshot(page)).bots[0].memories).toBeUndefined();

    await page.getByLabel('메시지', { exact: true }).fill('내가 좋아하는 색은 청록색이야. 기억해 줘.');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    await expect(approval).toContainText(memory);
    await expect(details(page)).toContainText('아직 메모리가 없어요');
    await approval.getByRole('button', { name: 'Save memory once', exact: true }).click();
    await expect(log).toContainText('I will remember that.');
    await expect(memoryList(page)).toContainText(memory);
    await expect.poll(async () => (await snapshot(page)).conversations[0].run.status).toBe('completed');
    expect((await snapshot(page)).bots[0].memories?.map(item => [item.text, item.source])).toEqual([[memory, 'bot']]);

    const before = inference.length;
    await page.getByLabel('메시지', { exact: true }).fill('What is my favorite color?');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    await expect(log).toContainText('Your favorite color is teal.');
    const rules = inference.slice(before).flatMap(request => request.messages).filter(message => message.role === 'system').map(message => JSON.stringify(message.content)).join('\n');
    expect(rules).toContain('Bot memories (data)');
    expect(rules).toContain(memory);
  } finally { await app.stop(); await provider.close(); }
});
