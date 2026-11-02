import { test, expect } from '@playwright/test';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';
import type { Bot, Conversation } from '../shared/types';
import { startTestApp } from './app-fixture';

test.use({ locale: 'ko-KR' });

type SeedState = { bots: Bot[]; conversations: Conversation[] };
const seedTime = '2026-11-02T00:00:00Z';

async function seedState(dataDir: string, populate: (state: SeedState) => void) {
  // Let the actual Rust backend create the current disk schema, then seed only disposable data
  // after shutdown so the fixture never imports the removed Node store or races a state write.
  const initial = await startTestApp({ env: { GROK_BOT_DATA_DIR: dataDir } });
  await initial.stop();
  const file = join(dataDir, 'state.json');
  const state = JSON.parse(await readFile(file, 'utf8')) as SeedState;
  populate(state);
  await writeFile(file, JSON.stringify(state), { mode: 0o600 });
}

function addBot(state: SeedState, name: string) {
  const bot: Bot = { ...state.bots[0], id: randomUUID(), name, color: '#75a9c0', instructions: '', shape: 'blob', pinned: false, createdAt: seedTime };
  state.bots.push(bot);
  return bot;
}

const image = '/api/generated-images/12345678-1234-1234-1234-123456789abc';
const examples = [
  { name: '수식', text: '\\(23 = 20 + 3\\)으로 나누어 계산...', preview: '23 = 20 + 3으로 나누어 계산...' },
  { name: '마크다운', text: '# **굵게**\n> [안내](https://example.com)와 `inline`', preview: '굵게 안내와 inline' },
  { name: '코드', text: '```ts\nconst value = "**raw**";\n```\n결과 확인', preview: '코드 결과 확인' },
  { name: '차트', text: '```visualization\n<svg><text>차트 내용</text></svg>\n```', preview: '차트' },
  { name: '이미지', text: `![생성 결과](${image})`, preview: '이미지' },
  { name: '이미지 링크', text: image, preview: '이미지' },
  { name: 'HTML', text: '<div>보이는 내용</div>', preview: '메시지' },
  { name: '링크 수식', text: '[price](https://example.com/?currency=$) and $x_i * y_j$.', preview: 'price and x_i * y_j.' },
];

test('sidebar previews show plain text and short media labels without changing stored messages', async ({ page }, testInfo) => {
  const dataDir = await mkdtemp(join(tmpdir(), 'grok-bot-sidebar-previews-'));
  let app: Awaited<ReturnType<typeof startTestApp>> | undefined;
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  try {
    await seedState(dataDir, state => {
      state.bots[0].shape = 'blob';
      for (const example of examples) {
        const bot = addBot(state, example.name);
        state.conversations.push({
          id: randomUUID(), botId: bot.id, title: example.name, updatedAt: seedTime, run: { id: '', status: 'completed' },
          messages: [{ id: randomUUID(), role: 'assistant', text: example.text, tools: [], createdAt: seedTime }],
        });
      }
    });
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

test('large history stays usable across fresh SSE snapshots and composer input', async ({ page }, testInfo) => {
  const dataDir = await mkdtemp(join(tmpdir(), 'grok-bot-preview-history-'));
  let app: Awaited<ReturnType<typeof startTestApp>> | undefined;
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  try {
    let bot!: Bot;
    const sources = Array.from({ length: 120 }, (_, index) => `**대화 ${index}** \\(23 = 20 + 3\\)\n\n${'내용과 [문서](https://example.com)입니다. '.repeat(100)}`);
    sources[117] += '긴 답변 '.repeat(50_000);
    sources[118] = '> '.repeat(4096) + '중첩된 인용문';
    sources[119] = '\\('.repeat(30_000);
    await seedState(dataDir, state => {
      bot = addBot(state, '긴 대화');
      for (const [index, text] of sources.entries()) {
        state.conversations.push({
          id: randomUUID(), botId: bot.id, title: `대화 ${index}`, updatedAt: seedTime, run: { id: '', status: 'completed' },
          messages: [{ id: randomUUID(), role: 'assistant', text, tools: [], createdAt: seedTime }],
        });
      }
    });
    app = await startTestApp({ env: { GROK_BOT_DATA_DIR: dataDir } });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto(`${app.url}?bot=${bot.id}`);
    await expect(page.getByRole('heading', { name: '긴 대화', exact: true })).toBeVisible();
    const history = page.getByRole('dialog', { name: '대화 기록', exact: true });
    const timings: number[] = [];
    for (let pass = 0; pass < 3; pass++) {
      const elapsed = await page.evaluate(() => {
        const start = performance.now();
        document.querySelector<HTMLButtonElement>('button[aria-label="대화 기록"]')!.click();
        return new Promise<number>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve(performance.now() - start))));
      });
      timings.push(elapsed);
      await expect(history.locator('.history-item')).toHaveCount(120);
      await expect(history.locator('.history-item > span:last-of-type').first()).toContainText('대화 0');
      expect(await history.locator('.history-item > span:last-of-type').evaluateAll(elements => Math.max(...elements.map(element => element.textContent?.length ?? 0)))).toBeLessThanOrEqual(280);
      // A real server update delivers fresh message objects to all 120 open history rows.
      await page.evaluate(async ({ botId, pass }) => {
        const response = await fetch(`/api/bots/${botId}`, { method: 'PATCH', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ name: `긴 대화 ${pass + 1}` }) });
        if (!response.ok) throw new Error(await response.text());
      }, { botId: bot.id, pass });
      await expect(history).toContainText(`긴 대화 ${pass + 1}의 대화`);
      if (pass === 2) await page.screenshot({ path: testInfo.outputPath('history-previews.png') });
      await history.getByRole('button', { name: '대화 기록 닫기', exact: true }).click();
    }
    const composer = page.getByLabel('메시지', { exact: true });
    await composer.pressSequentially('미리보기를 다시 계산하지 않는 입력');
    await expect(composer).toHaveValue('미리보기를 다시 계산하지 않는 입력');
    const saved = await page.evaluate(async () => (await fetch('/api/state')).json());
    expect(saved.conversations.map((conversation: { messages: { text: string }[] }) => conversation.messages[0].text)).toEqual(sources);
    const timingPath = testInfo.outputPath('history-frame-times.json');
    await writeFile(timingPath, JSON.stringify({ historyOpenToSecondFrameMs: timings }));
    await testInfo.attach('history-frame-times.json', { path: timingPath, contentType: 'application/json' });
    expect(errors).toEqual([]);
  } finally {
    await app?.stop();
    await rm(dataDir, { recursive: true, force: true });
  }
});
