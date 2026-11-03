import { test, expect, type Page } from '@playwright/test';
import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { startTestApp } from './app-fixture';
import { startMockProvider, type MockCompletion, type MockProviderRequest } from './mock-provider';
import type { AppState } from '../shared/types';

const snapshot = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());
const details = (page: Page) => page.getByRole('complementary', { name: '대화 세부 정보', exact: true });
async function openDetails(page: Page) {
  await page.getByRole('button', { name: '봇 편집', exact: true }).first().click();
  await expect(details(page).getByRole('heading', { name: '지식', exact: true })).toBeVisible();
}

test('knowledge files are added from an upload or paste, previewed, read by the model, and deleted', async ({ page }, info) => {
  const app = await startTestApp();
  let stage = 0;
  const inference: MockProviderRequest[] = [];
  let fileId = '';
  const provider = await startMockProvider({ reply: request => {
    if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: 'Knowledge fixture' };
    inference.push(request);
    const plan: MockCompletion[] = [
      { tool: { name: 'search_tool', arguments: { query: 'workspace read_knowledge', limit: 2 } } },
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__read_knowledge', tool_input: { id: fileId } } } },
      { text: 'Vacation is 15 days.' },
    ];
    return plan[stage++] ?? { text: 'Complete.' };
  } });
  try {
    await page.setViewportSize({ width: 1280, height: 860 });
    await page.goto(app.url);
    await openDetails(page);
    await expect(details(page)).toContainText('파일은 Bot에 공유 컨텍스트를 제공하고 팀의 지식을 활용해 답변하도록 돕습니다');
    const files = details(page).getByRole('list', { name: /의 지식 파일$/ });

    await details(page).getByRole('button', { name: '파일 추가', exact: true }).click();
    const dialog = page.getByRole('dialog', { name: '지식 파일 추가', exact: true });
    await dialog.getByLabel('지식 파일 선택').setInputFiles({ name: 'binary.bin', mimeType: 'text/plain', buffer: Buffer.from([0xff, 0xfe, 0x00, 0x81]) });
    await expect(dialog).toContainText('UTF-8 텍스트 파일만 추가할 수 있어요.');
    const handbook = '# 팀 핸드북\n휴가는 연 15일입니다. KNOWLEDGE_BROWSER_CANARY\n';
    await dialog.getByLabel('지식 파일 선택').setInputFiles({ name: 'handbook.md', mimeType: 'text/markdown', buffer: Buffer.from(handbook) });
    await expect(dialog.getByLabel('파일 이름', { exact: true })).toHaveValue('handbook.md');
    await expect(dialog.getByLabel('파일 내용', { exact: true })).toHaveValue(handbook);
    await dialog.getByRole('button', { name: '추가', exact: true }).click();
    await expect(dialog).toBeHidden();
    await expect(files).toContainText('handbook.md');

    await details(page).getByRole('button', { name: '파일 추가', exact: true }).click();
    await dialog.getByLabel('파일 이름', { exact: true }).fill('HANDBOOK.md');
    await dialog.getByLabel('파일 내용', { exact: true }).fill('pasted');
    await dialog.getByRole('button', { name: '추가', exact: true }).click();
    await expect(dialog).toContainText('A knowledge file with this name already exists for the bot.');
    await dialog.getByLabel('파일 이름', { exact: true }).fill('faq.txt');
    await dialog.getByLabel('파일 내용', { exact: true }).fill('Q: 점심시간은?\nA: 12시');
    await dialog.getByRole('button', { name: '추가', exact: true }).click();
    await expect(files.getByRole('listitem')).toHaveCount(2);
    await details(page).getByRole('heading', { name: 'Bot 템플릿', exact: true }).scrollIntoViewIfNeeded();
    await page.screenshot({ path: info.outputPath('knowledge-details.png') });

    await files.getByRole('button', { name: 'handbook.md 내용 보기', exact: true }).click();
    const preview = page.getByRole('dialog', { name: 'handbook.md 내용', exact: true });
    await expect(preview).toContainText('휴가는 연 15일입니다.');
    await preview.getByRole('button', { name: '파일 내용 닫기', exact: true }).click();
    const state = await snapshot(page);
    fileId = state.bots[0].knowledge!.find(file => file.name === 'handbook.md')!.id;
    expect(JSON.stringify(state)).not.toContain('KNOWLEDGE_BROWSER_CANARY');

    await page.evaluate(async baseUrl => {
      const response = await fetch('/api/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ baseUrl, model: 'local-test-model', workspace: '' }) });
      if (!response.ok) throw Error(await response.text());
    }, provider.baseUrl);
    await page.getByLabel('메시지', { exact: true }).fill('휴가는 며칠이야?');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    await expect(page.getByRole('log', { name: '대화 메시지', exact: true })).toContainText('Vacation is 15 days.');
    expect(inference.some(request => request.messages.some(message => message.role === 'tool' && JSON.stringify(message.content).includes('KNOWLEDGE_BROWSER_CANARY')))).toBe(true);
    expect(inference.some(request => request.messages.some(message => message.role === 'system' && JSON.stringify(message.content).includes('faq.txt')))).toBe(true);

    await expect.poll(async () => (await snapshot(page)).conversations[0].run.status).toBe('completed');
    page.once('dialog', prompt => prompt.accept());
    await files.getByRole('button', { name: '파일 삭제: faq.txt', exact: true }).click();
    await expect(files.getByRole('listitem')).toHaveCount(1);
    expect((await snapshot(page)).bots[0].knowledge?.map(file => file.name)).toEqual(['handbook.md']);
  } finally { await app.stop(); await provider.close(); }
});

test('a bot exports to a template file and a new bot is created from it', async ({ page }) => {
  const app = await startTestApp();
  try {
    await page.setViewportSize({ width: 1280, height: 860 });
    await page.goto(app.url);
    const bot = (await snapshot(page)).bots[0];
    await page.evaluate(async id => {
      const send = async (path: string, method: string, body: unknown) => {
        const response = await fetch(path, { method, headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify(body) });
        if (!response.ok) throw Error(await response.text());
      };
      await send(`/api/bots/${id}`, 'PATCH', { name: '리뷰어', instructions: '꼼꼼하게 검토해 줘.', shape: 'heart', color: '#22c55e' });
      await send(`/api/bots/${id}/skills`, 'POST', { name: 'review', description: 'Use to review.', instructions: 'Check every item.' });
      await send(`/api/bots/${id}/memories`, 'POST', { texts: ['팀은 Rust를 쓴다'] });
      await send(`/api/bots/${id}/knowledge`, 'POST', { name: 'private.txt', content: 'not exported' });
    }, bot.id);
    await openDetails(page);
    await details(page).getByRole('checkbox', { name: '메모리 포함', exact: true }).check();
    const downloading = page.waitForEvent('download');
    await details(page).getByRole('button', { name: '템플릿 내보내기', exact: true }).click();
    const download = await downloading;
    expect(download.suggestedFilename()).toBe('리뷰어.grok-bot-template.json');
    const path = join(app.root, 'template.json');
    await download.saveAs(path);
    const text = await readFile(path, 'utf8');
    const template = JSON.parse(text);
    expect(template).toEqual({
      format: 'grok-bot.bot-template', version: 1,
      bot: { name: '리뷰어', color: '#22c55e', instructions: '꼼꼼하게 검토해 줘.', shape: 'heart' },
      skills: [{ name: 'review', description: 'Use to review.', instructions: 'Check every item.', enabled: true }],
      memories: [{ text: '팀은 Rust를 쓴다' }],
    });
    expect(text).not.toContain(bot.id);
    expect(text).not.toContain('not exported');

    await details(page).getByLabel('Bot 템플릿 파일 선택').setInputFiles({ name: 'bad.json', mimeType: 'application/json', buffer: Buffer.from(JSON.stringify({ ...template, id: bot.id })) });
    await expect(details(page)).toContainText('unknown field');
    await details(page).getByLabel('Bot 템플릿 파일 선택').setInputFiles(path);
    await expect.poll(async () => (await snapshot(page)).bots.length).toBe(2);
    const created = (await snapshot(page)).bots[1];
    expect(created.id).not.toBe(bot.id);
    expect([created.name, created.shape, created.skills?.[0].name, created.memories?.[0].text, created.knowledge]).toEqual(['리뷰어', 'heart', 'review', '팀은 Rust를 쓴다', undefined]);
    await expect(page).toHaveURL(new RegExp(created.id));
    await expect(details(page).getByRole('list', { name: '리뷰어의 메모리', exact: true })).toContainText('팀은 Rust를 쓴다');
    await expect(details(page)).toContainText('추가한 파일이 여기에 표시됩니다.');
  } finally { await app.stop(); }
});
