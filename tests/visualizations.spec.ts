import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import { test, expect, type Page } from '@playwright/test';
import { startTestApp } from './app-fixture';
import { startMockProvider } from './mock-provider';
import type { AppState } from '../shared/types';

const snapshot = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());

/** Records every connection and request: no visualization may reach it, not even to connect. */
async function startSink() {
  const hits: string[] = [];
  const server = createServer((request, response) => { hits.push(request.url ?? ''); response.end('sink'); });
  let connections = 0;
  server.on('connection', () => { connections++; });
  await new Promise<void>(resolve => server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
  return {
    hits, origin,
    connections: () => connections,
    close: () => new Promise<void>(resolve => { server.closeAllConnections(); server.close(() => resolve()); }),
  };
}

/** `sink` must see nothing at all; `away` is the target of scripted navigations (see below). */
function answer(sink: string, away: string): string {
  return [
    '격리된 미리보기입니다.',
    '',
    '```html',
    '<!DOCTYPE html><html><head><title>샌드박스 확인</title><style>body{font:14px sans-serif}</style></head><body>',
    '<h1>격리 확인</h1><pre id="out">running</pre>',
    `<img src="${sink}/image.png" alt="">`,
    '<script>',
    '(async () => {',
    '  const result = {};',
    '  try { result.parent = typeof parent.document.title; } catch (error) { result.parent = "blocked"; }',
    '  try { result.storage = typeof localStorage.length; } catch (error) { result.storage = "blocked"; }',
    '  try { result.cookie = typeof document.cookie; } catch (error) { result.cookie = "blocked"; }',
    `  for (const [name, url] of [["api", "/api/state"], ["sink", "${sink}/fetch"]]) {`,
    '    try { const response = await fetch(url); result[name] = "status " + response.status; } catch (error) { result[name] = "blocked"; }',
    '  }',
    `  try { result.popup = window.open("${sink}/popup") === null ? "blocked" : "opened"; } catch (error) { result.popup = "blocked"; }`,
    `  try { top.location.href = "${sink}/top"; } catch (error) {}`,
    '  parent.postMessage({ channel: "grok-bot-visualization", nonce: "00000000000000000000000000000000", type: "error", message: "forged" }, "*");',
    '  document.getElementById("out").textContent = JSON.stringify(result);',
    '})();',
    '</script></body></html>',
    '```',
    '',
    '```visualization',
    '<div id="chart"></div>',
    '<script>',
    'const data = [4, 8, 15, 16, 23, 42];',
    'const svg = d3.select("#chart").append("svg").attr("width", 320).attr("height", 120);',
    'svg.selectAll("rect").data(data).join("rect").attr("x", (d, i) => i * 50).attr("y", d => 120 - d * 2.5).attr("width", 40).attr("height", d => d * 2.5).attr("fill", "#4f8ef7");',
    '</script>',
    '```',
    '',
    '```html',
    '<title>오류 시각화</title><p>before</p><script>missingFunction();</script>',
    '```',
    '',
    '```html',
    `<title>링크 시각화</title><p id="stay">still here</p><a id="away" href="${sink}/link"><span>away</span></a>`,
    '<script>setTimeout(() => document.querySelector("#away span").click(), 100);</script>',
    '```',
    '',
    '```html',
    `<title>이동 시각화</title><p>leaving</p><script>setTimeout(() => { location.href = "${away}/navigate"; }, 100);</script>`,
    '```',
    '',
    '```html',
    `<title>새로고침 시각화</title><meta http-equiv="refresh" content="0;url=${away}/refresh"><p>leaving</p>`,
    '```',
    '',
    '```svg',
    '<svg xmlns="http://www.w3.org/2000/svg" width="120" height="60"><circle cx="30" cy="30" r="20" fill="tomato"/></svg>',
    '```',
  ].join('\n');
}

async function setUp(page: Page, text: string, viewport: { width: number; height: number }) {
  const app = await startTestApp();
  const provider = await startMockProvider({ reply: () => ({ text }) });
  await page.setViewportSize(viewport);
  await page.goto(app.url);
  await page.evaluate(async baseUrl => {
    const response = await fetch('/api/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ baseUrl, model: 'local-test-model', workspace: '' }) });
    if (!response.ok) throw new Error(await response.text());
  }, provider.baseUrl);
  await expect(page.getByRole('heading', { name: 'Grok', exact: true })).toBeVisible();
  await page.getByLabel('메시지', { exact: true }).fill('시각화를 보여줘');
  await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
  await expect.poll(async () => (await snapshot(page)).conversations[0]?.run.status, { timeout: 30_000 }).toBe('completed');
  const message = page.getByRole('article', { name: 'Grok 메시지', exact: true }).last();
  return { app, provider, message };
}

for (const viewport of [{ width: 1280, height: 900 }, { width: 390, height: 844 }]) {
  test(`visualizations run sandboxed, report errors, and open full screen at ${viewport.width}px`, async ({ page }, testInfo) => {
    const sink = await startSink();
    const away = await startSink();
    const errors: string[] = [];
    const frameRequests: string[] = [];
    // The failing visualization's own error is expected; anything else is not.
    page.on('pageerror', error => { if (error.message !== 'missingFunction is not defined') errors.push(error.message); });
    page.on('popup', popup => errors.push(`popup: ${popup.url()}`));
    page.on('request', request => { if (request.frame() !== page.mainFrame() && /^https?:/.test(request.url())) frameRequests.push(request.url()); });
    const { app, provider, message } = await setUp(page, answer(sink.origin, away.origin), viewport);
    try {
      const appUrl = page.url();
      const probe = message.getByRole('figure', { name: '샌드박스 확인', exact: true });
      await probe.scrollIntoViewIfNeeded();
      const probeFrame = probe.locator('iframe');
      await expect(probeFrame).toHaveAttribute('sandbox', 'allow-scripts');
      expect(await probeFrame.getAttribute('srcdoc')).toMatch(/^<!DOCTYPE html><html><head><meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob:;/);
      const out = probeFrame.contentFrame().locator('#out');
      await expect(out).not.toHaveText('running');
      const result = JSON.parse((await out.textContent())!);
      // No access to the app's document, storage, or cookies; no API, network, popups.
      expect(result).toEqual({ parent: 'blocked', storage: 'blocked', cookie: 'blocked', api: 'blocked', sink: 'blocked', popup: 'blocked' });
      // A forged message without this frame's nonce changed nothing.
      await expect(probe.getByRole('alert')).toHaveCount(0);
      await expect(probe).toContainText(/외부 리소스 요청 \d+개를 차단했어요/);
      expect(page.url()).toBe(appUrl);

      // d3 is bundled (no network) for ```visualization.
      const chart = message.getByRole('figure', { name: '시각화', exact: true });
      await chart.scrollIntoViewIfNeeded();
      const chartFrame = chart.locator('iframe');
      await expect(chartFrame.contentFrame().locator('rect')).toHaveCount(6);
      // A message posted by the page itself with the right nonce is ignored too (not from the frame).
      const nonce = /var nonce = "([0-9a-f]+)"/.exec((await chartFrame.getAttribute('srcdoc'))!)![1];
      await page.evaluate(nonce => window.postMessage({ channel: 'grok-bot-visualization', nonce, type: 'error', message: 'forged by page' }, '*'), nonce);
      await page.waitForTimeout(300);
      await expect(chart.getByRole('alert')).toHaveCount(0);
      await expect(chartFrame.contentFrame().locator('rect')).toHaveCount(6);

      // Code view stays available.
      await chart.getByRole('button', { name: '코드 보기', exact: true }).click();
      await expect(chart.locator('pre code')).toContainText('d3.select("#chart")');
      await expect(chart.locator('iframe')).toHaveCount(0);
      await chart.getByRole('button', { name: '미리보기', exact: true }).click();
      await expect(chartFrame.contentFrame().locator('rect')).toHaveCount(6);

      // Full screen: Escape on the controls and inside the frame both close it; focus returns.
      const enlarge = chart.getByRole('button', { name: '시각화 확대', exact: true });
      await enlarge.click();
      const viewer = page.getByRole('dialog', { name: '시각화 미리보기', exact: true });
      await expect(viewer).toBeVisible();
      await expect(viewer.getByRole('button', { name: '시각화 미리보기 닫기', exact: true })).toBeFocused();
      await expect(viewer.locator('iframe').contentFrame().locator('rect')).toHaveCount(6);
      await page.screenshot({ path: testInfo.outputPath(`visualization-viewer-${viewport.width}.png`) });
      await page.keyboard.press('Escape');
      await expect(viewer).toHaveCount(0);
      await expect(enlarge).toBeFocused();
      await enlarge.click();
      await viewer.locator('iframe').contentFrame().locator('svg').click({ position: { x: 5, y: 5 } });
      await page.keyboard.press('Escape');
      await expect(viewer).toHaveCount(0);
      await enlarge.click();
      await viewer.getByRole('button', { name: '시각화 미리보기 닫기', exact: true }).click();
      await expect(viewer).toHaveCount(0);

      // Script errors and navigation attempts use the original's messages.
      const failing = message.getByRole('figure', { name: '오류 시각화', exact: true });
      await failing.scrollIntoViewIfNeeded();
      await expect(failing.getByRole('alert')).toContainText('이 시각화에서 스크립트 오류가 발생했어요');
      await expect(failing.getByRole('alert')).toContainText('missingFunction is not defined');
      // Link clicks are cancelled inside the frame; the visualization stays and says so.
      const linked = message.getByRole('figure', { name: '링크 시각화', exact: true });
      await linked.scrollIntoViewIfNeeded();
      await expect(linked.getByRole('note')).toContainText('시각화 안의 링크는 열 수 없어요.');
      await expect(linked.locator('iframe').contentFrame().locator('#stay')).toHaveText('still here');
      await expect(linked.getByRole('alert')).toHaveCount(0);
      // Scripted navigations and meta refresh are refused by the app page's `frame-src 'none'`.
      // Known Chromium gap: it may connect to (never request from) the target first, so those use
      // their own server, which must see no request.
      expect(await page.evaluate(() => document.querySelector('meta[http-equiv="Content-Security-Policy"]')?.getAttribute('content'))).toBe("frame-src 'none'");
      for (const name of ['이동 시각화', '새로고침 시각화']) {
        const leaving = message.getByRole('figure', { name, exact: true });
        await leaving.scrollIntoViewIfNeeded();
        await expect(leaving.getByRole('alert')).toContainText('이 시각화를 불러올 수 없어요');
        await expect(leaving.locator('iframe')).toHaveCount(0);
      }

      const vector = message.getByRole('figure', { name: 'SVG 미리보기', exact: true });
      await vector.scrollIntoViewIfNeeded();
      await expect(vector.locator('iframe').contentFrame().locator('circle')).toHaveCount(1);
      await page.screenshot({ path: testInfo.outputPath(`visualizations-${viewport.width}.png`), fullPage: true });

      await page.waitForTimeout(500);
      expect(sink.hits).toEqual([]);
      expect(sink.connections()).toBe(0);
      expect(away.hits).toEqual([]);
      expect(frameRequests).toEqual([]);
      expect(page.url()).toBe(appUrl);
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
      expect(errors).toEqual([]);
    } finally {
      await app.stop();
      await provider.close();
      await sink.close();
      await away.close();
    }
  });

  test(`a visualization that does not start within 10 seconds shows the timeout at ${viewport.width}px`, async ({ page }) => {
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    // A finite busy loop (11 s): the frame's load event comes too late.
    const text = ['```html', '<title>느린 시각화</title><script>const end = Date.now() + 11000; while (Date.now() < end) {}</script><p>done</p>', '```'].join('\n');
    const { app, provider, message } = await setUp(page, text, viewport);
    try {
      const slow = message.getByRole('figure', { name: '느린 시각화', exact: true });
      await expect(slow.getByRole('status')).toContainText('시각화를 시작하는 중');
      await expect(slow.getByRole('alert')).toContainText('이 시각화를 시작하는 데 10초 넘게 걸렸습니다', { timeout: 14_000 });
      await expect(slow.locator('iframe')).toHaveCount(0);
      await expect(slow.getByRole('button', { name: '다시 시도', exact: true })).toBeVisible();
      expect(errors).toEqual([]);
    } finally {
      await app.stop();
      await provider.close();
    }
  });
}
