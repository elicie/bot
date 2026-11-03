import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import { mkdir, readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { test, expect, type Page, type TestInfo } from '@playwright/test';
import type { AppState } from '../shared/types';
import { startTestApp } from './app-fixture';
import { startMockProvider } from './mock-provider';

const baseline = process.env.LINK_PREVIEW_BASELINE === '1';
// Explicit optional extension also lets this scenario compile against an untouched baseline.
const snapshot = (page: Page): Promise<AppState & { linkPreviewsEnabled?: boolean }> => page.evaluate(async () => (await fetch('/api/state')).json());
const title = '봇의 진행 상황을 한눈에';
const description = '진행 중인 작업과 완료한 항목을 대화 안에서 확인하는 방법을 소개합니다.';

async function previewSite() {
  const hits: string[] = [];
  const image = await readFile(new URL('../docs/evidence/issue-6/after-desktop.png', import.meta.url));
  const server = createServer((request, response) => {
    hits.push(request.url ?? '');
    if (request.url === '/thumbnail.png') {
      response.writeHead(200, { 'Content-Type': 'image/png' }).end(image);
    } else if (request.url === '/guide') {
      response.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' }).end(`<!doctype html><html><head>
        <title>Fallback title</title><meta property="og:title" content="${title}">
        <meta property="og:description" content="${description}"><meta property="og:image" content="/thumbnail.png">
        </head><body>Public fixture article. No browser should visit this page to make a card.</body></html>`);
    } else if (request.url === '/fallback') {
      response.writeHead(200, { 'Content-Type': 'text/html' }).end('<title>로컬 작업 환경 안내</title><meta name="description" content="작업 폴더를 연결하고 파일을 안전하게 저장하세요.">');
    } else if (request.url === '/third') {
      response.writeHead(200, { 'Content-Type': 'text/html' }).end('<meta property="og:title" content="출처 확인"><meta property="og:description" content="&lt;img src=x onerror=alert(1)&gt;실행되지 않는 텍스트">');
    } else if (request.url === '/fourth') {
      response.writeHead(200, { 'Content-Type': 'text/html' }).end('<title>Fourth card must never be requested</title>');
    } else {
      response.writeHead(404).end();
    }
  });
  await new Promise<void>(done => server.listen(Number(process.env.LINK_PREVIEW_FIXTURE_PORT ?? 0), '127.0.0.1', done));
  const origin = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
  return { origin, hits, close: async () => {
    server.closeAllConnections();
    await new Promise<void>(done => server.close(() => done()));
  } };
}

async function configureAndSend(page: Page, appUrl: string, providerUrl: string) {
  await page.goto(appUrl);
  await page.evaluate(async baseUrl => {
    const response = await fetch('/api/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ baseUrl, model: 'local-test-model', workspace: '' }) });
    if (!response.ok) throw new Error(await response.text());
    const state = await response.json();
    const bot = await fetch(`/api/bots/${state.bots[0].id}`, { method: 'PATCH', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ shape: 'blob' }) });
    if (!bot.ok) throw new Error(await bot.text());
  }, providerUrl);
  await expect(page.getByRole('heading', { name: 'Grok', exact: true })).toBeVisible();
  await page.getByLabel('메시지', { exact: true }).fill('작업 진행 상황을 확인할 수 있는 자료를 알려줘');
  await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
  await expect.poll(async () => (await snapshot(page)).conversations[0]?.run.status, { timeout: 30_000 }).toBe('completed');
  await expect(page.getByRole('article', { name: 'Grok 메시지', exact: true }).last()).toContainText('참고할 자료');
}

async function setPreviewSwitch(page: Page, enabled: boolean) {
  const menu = page.getByRole('button', { name: '봇 목록 열기', exact: true });
  if (await menu.isVisible()) await menu.click();
  await page.getByRole('button', { name: '설정', exact: true }).click();
  const dialog = page.getByRole('dialog', { name: '설정', exact: true });
  await dialog.getByRole('button', { name: '일반', exact: true }).click();
  const toggle = dialog.getByRole('switch', { name: '링크 미리보기', exact: true });
  await toggle.scrollIntoViewIfNeeded();
  await expect(toggle).toBeEnabled();
  // Enabling becomes checked only after the server commits the opt-in (not optimistically).
  if (await toggle.isChecked() !== enabled) await toggle.click();
  await expect.poll(async () => (await snapshot(page)).linkPreviewsEnabled).toBe(enabled);
  await expect(toggle).toBeChecked({ checked: enabled });
  await dialog.getByRole('button', { name: '설정 닫기', exact: true }).click();
}

async function capture(page: Page, info: TestInfo, name: string) {
  const evidence = process.env.GROK_BOT_LINK_PREVIEW_EVIDENCE_DIR;
  if (evidence) await mkdir(resolve(evidence), { recursive: true });
  await page.mouse.move(0, 0); // Match the unhovered UI state across revisions/viewports.
  await page.screenshot({ path: evidence ? resolve(evidence, name) : info.outputPath(name) });
}

for (const viewport of [{ width: 1040, height: 760 }, { width: 390, height: 844 }]) {
  test(`link preview visual comparison at ${viewport.width}px`, async ({ page }, info) => {
    const site = await previewSite();
    const app = await startTestApp({ env: { GROK_BOT_LINK_PREVIEW_TEST_ORIGINS: JSON.stringify([site.origin]) } });
    const provider = await startMockProvider({ reply: () => ({ text: `참고할 자료를 찾았습니다.\n\n[${title}](${site.origin}/guide)` }) });
    const errors: string[] = [];
    const external: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    page.on('request', request => { if (/^https?:/.test(request.url()) && new URL(request.url()).origin !== app.url) external.push(request.url()); });
    await page.setViewportSize(viewport);
    try {
      await configureAndSend(page, app.url, provider.baseUrl);
      expect(site.hits).toEqual([]);
      if (!baseline) {
        expect((await snapshot(page)).linkPreviewsEnabled).toBe(false);
        await expect(page.locator('.link-preview-card')).toHaveCount(0);
        await setPreviewSwitch(page, true);
        await expect(page.locator('.link-preview-card')).toHaveCount(1);
        await expect(page.locator('.link-preview-card')).toContainText(description);
        const image = page.locator('.link-preview-card img');
        await expect(image).toHaveAttribute('src', /^\/api\/generated-images\/[0-9a-f-]{36}$/);
        await expect.poll(() => image.evaluate((element: HTMLImageElement) => element.complete && element.naturalWidth)).toBeGreaterThan(0);
      }
      await page.getByRole('article', { name: 'Grok 메시지', exact: true }).last().scrollIntoViewIfNeeded();
      await capture(page, info, `${baseline ? 'before' : 'after'}-${viewport.width}.png`);
      const menu = page.getByRole('button', { name: '봇 목록 열기', exact: true });
      if (await menu.isVisible()) await menu.click();
      await page.getByRole('button', { name: '설정', exact: true }).click();
      const settings = page.getByRole('dialog', { name: '설정', exact: true });
      await settings.getByRole('button', { name: '일반', exact: true }).click();
      await settings.locator('.settings-scroll').evaluate(element => { element.scrollTop = element.scrollHeight; });
      await capture(page, info, `${baseline ? 'before' : 'after'}-settings-${viewport.width}.png`);
      await settings.getByRole('button', { name: '설정 닫기', exact: true }).click();
      if (!baseline) {
        // Same answer/state in the alternate theme: no additional metadata or thumbnail request.
        const hits = [...site.hits];
        await page.evaluate(() => { document.documentElement.dataset.theme = 'light'; });
        await capture(page, info, `after-${viewport.width}-light.png`);
        expect(site.hits).toEqual(hits);
      }
      expect(external).toEqual([]);
      expect(errors).toEqual([]);
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    } finally {
      await app.stop();
      await provider.close();
      await site.close();
    }
  });
}

test('opt-in previews are capped, deduplicated, same-origin, cached and disabled durably', async ({ page }) => {
  test.skip(baseline, 'The pre-feature baseline has no preview setting.');
  const site = await previewSite();
  const answer = [
    '참고할 자료입니다.',
    `[${title}](${site.origin}/guide#one)`,
    `[같은 자료](${site.origin}/guide#two)`,
    `[로컬 작업 환경 안내](${site.origin}/fallback)`,
    `[출처 확인](${site.origin}/third)`,
    `[개수 제한 밖](${site.origin}/fourth)`,
    `![이미지는 링크가 아님](${site.origin}/image-target.png)`,
    `\`${site.origin}/inline-code\``,
    '> ' + site.origin + '/quoted',
    '```text\n' + site.origin + '/code-fence\n```',
  ].join('\n\n');
  const app = await startTestApp({ env: { GROK_BOT_LINK_PREVIEW_TEST_ORIGINS: JSON.stringify([site.origin]) } });
  const provider = await startMockProvider({ reply: () => ({ text: answer }) });
  const external: string[] = [];
  const errors: string[] = [];
  page.on('request', request => { if (/^https?:/.test(request.url()) && new URL(request.url()).origin !== app.url) external.push(request.url()); });
  page.on('pageerror', error => errors.push(error.message));
  try {
    await configureAndSend(page, app.url, provider.baseUrl);
    await expect(page.locator('.link-preview-card')).toHaveCount(0);
    expect(site.hits).toEqual([]);
    await setPreviewSwitch(page, true);
    const cards = page.locator('.link-preview-card');
    await expect(cards).toHaveCount(3);
    await cards.last().scrollIntoViewIfNeeded();
    await expect(cards.nth(0)).toContainText(title);
    await expect(cards.nth(1)).toContainText('작업 폴더를 연결하고 파일을 안전하게 저장하세요.');
    await expect(cards.nth(2)).toContainText('실행되지 않는 텍스트');
    await expect(cards.nth(1).locator('img')).toHaveCount(0);
    await expect(cards.locator('script')).toHaveCount(0);
    // Every card's clickable surface follows the original link safely in a new tab.
    const links = page.locator('a.link-preview-card, .link-preview-card a');
    await expect(links).toHaveCount(3);
    await expect(links.first()).toHaveAttribute('target', '_blank');
    await expect(links.first()).toHaveAttribute('rel', /noreferrer/);
    expect(site.hits.filter(path => path === '/guide')).toHaveLength(1);
    expect(site.hits.filter(path => path === '/thumbnail.png')).toHaveLength(1);
    expect(site.hits.some(path => /fourth|image-target|inline-code|quoted|code-fence/.test(path))).toBe(false);
    // Like a cross-site image/navigation, an unmarked GET cannot initiate a new server fetch.
    const unmarked = await page.request.get(`${app.url}/api/link-preview?url=${encodeURIComponent(`${site.origin}/fourth`)}`);
    expect(unmarked.status()).toBe(403);
    expect(site.hits).not.toContain('/fourth');

    const hits = [...site.hits];
    const cached = await page.evaluate(async url => (await fetch(`/api/link-preview?url=${encodeURIComponent(url)}`, { headers: { 'X-Grok-Bot': '1' } })).json(), `${site.origin}/guide#another`);
    expect(cached.title).toBe(title);
    expect(site.hits).toEqual(hits);
    await page.reload();
    await expect(page.locator('.link-preview-card')).toHaveCount(3);
    expect((await snapshot(page)).linkPreviewsEnabled).toBe(true);
    expect(site.hits).toEqual(hits);

    await setPreviewSwitch(page, false);
    await expect(page.locator('.link-preview-card')).toHaveCount(0);
    const status = await page.evaluate(async url => (await fetch(`/api/link-preview?url=${encodeURIComponent(url)}`, { headers: { 'X-Grok-Bot': '1' } })).status, `${site.origin}/guide`);
    expect(status).toBe(403);
    await page.reload();
    expect((await snapshot(page)).linkPreviewsEnabled).toBe(false);
    await expect(page.locator('.link-preview-card')).toHaveCount(0);
    expect(site.hits).toEqual(hits);
    expect(external).toEqual([]);
    expect(errors).toEqual([]);
  } finally {
    await app.stop();
    await provider.close();
    await site.close();
  }
});
