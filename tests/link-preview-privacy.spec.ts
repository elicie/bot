import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import { test, expect } from '@playwright/test';
import { startTestApp } from './app-fixture';
import { startMockProvider } from './mock-provider';

test('preview rendering stays safe at 320px and failed opt-out stays paused in this tab', async ({ page }) => {
  test.skip(process.env.GROK_BOT_BACKEND !== 'rust', 'Link previews require the Rust server.');
  const hits: string[] = [];
  const title = '길이가아주긴문서제목'.repeat(25);
  const server = createServer((request, response) => {
    hits.push(request.url ?? '');
    response.writeHead(200, { 'Content-Type': 'text/html' }).end(`<title>${title}</title><meta name="description" content="${'long-unbroken-description'.repeat(80)}"><meta property="og:image" content="data:image/svg+xml,invalid">`);
  });
  await new Promise<void>(done => server.listen(0, '127.0.0.1', done));
  const origin = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
  const app = await startTestApp({ env: { GROK_BOT_LINK_PREVIEW_TEST_ORIGINS: JSON.stringify([origin]) } });
  const provider = await startMockProvider({ reply: () => ({ text: `읽을 자료: [긴 문서](${origin}/long)` }) });
  const errors: string[] = [];
  const external: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  page.on('request', request => { if (/^https?:/.test(request.url()) && new URL(request.url()).origin !== app.url) external.push(request.url()); });
  await page.setViewportSize({ width: 320, height: 760 });
  try {
    await page.goto(app.url);
    await page.evaluate(async baseUrl => {
      for (const [path, body] of [
        ['/api/settings', { baseUrl, model: 'local-test-model', workspace: '' }],
        ['/api/link-preview/settings', { enabled: true }],
      ] as const) {
        const response = await fetch(path, { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify(body) });
        if (!response.ok) throw new Error(await response.text());
      }
    }, provider.baseUrl);
    await expect(page.getByRole('heading', { name: 'Grok', exact: true })).toBeVisible();
    await page.getByLabel('메시지', { exact: true }).fill('긴 문서 보여줘');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    const card = page.locator('.link-preview-card');
    await expect(card).toContainText(title);
    await expect(card.locator('img')).toHaveCount(0);
    await card.scrollIntoViewIfNeeded();
    await card.focus();
    await expect(card).toBeFocused();
    expect(await card.evaluate(element => element.scrollWidth <= element.clientWidth)).toBe(true);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    expect(hits).toEqual(['/long']);

    // Simulate failed persistence, not a failed download. UI must not resume its requests on error.
    await page.route('**/api/link-preview/settings', route => route.fulfill({ status: 500, contentType: 'application/json', body: JSON.stringify({ error: 'Fixture persistence failure' }) }));
    await page.getByRole('button', { name: '봇 목록 열기', exact: true }).click();
    await page.getByRole('button', { name: '설정', exact: true }).click();
    const dialog = page.getByRole('dialog', { name: '설정', exact: true });
    const toggle = dialog.getByRole('switch', { name: '링크 미리보기', exact: true });
    await toggle.scrollIntoViewIfNeeded();
    await toggle.click();
    await expect(toggle).not.toBeChecked();
    await expect(dialog.getByRole('alert')).toContainText('이 탭에서만 링크 미리보기를 꺼 두었습니다.');
    await expect(card).toHaveCount(0);
    expect(await page.evaluate(async () => (await (await fetch('/api/state')).json()).linkPreviewsEnabled)).toBe(true);
    await dialog.getByRole('button', { name: '설정 닫기', exact: true }).click();
    await expect(card).toHaveCount(0);
    expect(hits).toEqual(['/long']);
    expect(external).toEqual([]);
    expect(errors).toEqual([]);
  } finally {
    await app.stop();
    await provider.close();
    server.closeAllConnections();
    await new Promise<void>(done => server.close(() => done()));
  }
});
