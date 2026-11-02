import { test, expect } from '@playwright/test';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';
import { Store } from '../server/store';
import { startTestApp } from './app-fixture';

test.use({ locale: 'ko-KR' });

const image = '/api/generated-images/12345678-1234-1234-1234-123456789abc';
const examples = [
  { name: '수식', text: '\\(23 = 20 + 3\\)으로 나누어 계산...', preview: '23 = 20 + 3으로 나누어 계산...' },
  { name: '마크다운', text: '# **굵게**\n> [안내](https://example.com)와 `inline`', preview: '굵게 안내와 inline' },
  { name: '코드', text: '```ts\nconst value = "**raw**";\n```\n결과 확인', preview: '코드 결과 확인' },
  { name: '차트', text: '```visualization\n<svg><text>차트 내용</text></svg>\n```', preview: '차트' },
  { name: '이미지', text: `![생성 결과](${image})`, preview: '이미지' },
  { name: '이미지 링크', text: image, preview: '이미지' },
];

test('sidebar previews show plain text and short media labels without changing stored messages', async ({ page }, testInfo) => {
  const dataDir = await mkdtemp(join(tmpdir(), 'grok-bot-sidebar-previews-'));
  let app: Awaited<ReturnType<typeof startTestApp>> | undefined;
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  try {
    // Seed ordinary persisted conversations, shared by the Node and Rust backends; no model needed.
    const store = await Store.open(dataDir, { now: () => new Date('2026-11-02T00:00:00Z') });
    await store.updateBot(store.bots[0].id, { shape: 'blob' });
    for (const example of examples) {
      const bot = await store.addBot({ name: example.name, color: '#75a9c0', instructions: '', shape: 'blob', pinned: false });
      const conversation = await store.addConversation(bot.id);
      conversation.messages.push({ id: randomUUID(), role: 'assistant', text: example.text, tools: [], createdAt: conversation.updatedAt });
      conversation.run.status = 'completed';
    }
    await store.changed();
    app = await startTestApp({ env: { GROK_BOT_DATA_DIR: dataDir } });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.emulateMedia({ colorScheme: 'dark' });
    await page.addInitScript(() => {
      localStorage.setItem('grok-bot-sidebar-width', '400');
      localStorage.setItem('grok-bot-sidebar-compact', 'false');
      localStorage.setItem('grok-bot.browser-preferences', JSON.stringify({ theme: 'dark', spellcheck: true }));
    });
    await page.goto(app.url);
    const sidebar = page.getByRole('complementary', { name: '봇 목록', exact: true });
    const preview = (name: string) => sidebar.locator(`.bot-row[title="${name}"] small`);
    await expect(page.getByRole('heading', { name: 'Grok', exact: true })).toBeVisible();
    await expect(preview('이미지 링크')).toBeVisible();
    await page.evaluate(() => document.fonts.ready);
    await page.screenshot({ path: testInfo.outputPath('sidebar-previews.png') });
    for (const example of examples) await expect(preview(example.name)).toHaveText(example.preview);
    await page.reload();
    for (const example of examples) await expect(preview(example.name)).toHaveText(example.preview);
    const saved = await page.evaluate(async () => (await fetch('/api/state')).json());
    expect(saved.conversations.map((conversation: { messages: { text: string }[] }) => conversation.messages[0].text)).toEqual(examples.map(example => example.text));
    // Full messages keep their existing Markdown rendering, separate from the preview copy.
    await sidebar.locator('.bot-row[title="마크다운"]').click();
    await expect(page.getByRole('article', { name: '마크다운 메시지', exact: true }).locator('strong')).toHaveText('굵게');
    await page.setViewportSize({ width: 390, height: 844 });
    await page.getByRole('button', { name: '봇 목록 열기', exact: true }).click();
    for (const example of examples) await expect(preview(example.name)).toHaveText(example.preview);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    expect(errors).toEqual([]);
  } finally {
    await app?.stop();
    await rm(dataDir, { recursive: true, force: true });
  }
});
