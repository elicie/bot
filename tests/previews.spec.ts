import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import { test, expect, type Page } from '@playwright/test';
import { startTestApp } from './app-fixture';
import { startMockProvider } from './mock-provider';
import type { AppState } from '../shared/types';

const snapshot = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());
/** A valid 1×1 PNG. */
const pixel = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==';
const focusInsideDialog = (page: Page) => page.evaluate(() => !!document.querySelector('dialog[open]')?.contains(document.activeElement));

for (const viewport of [{ width: 1280, height: 900 }, { width: 390, height: 844 }]) {
  test(`diagram and media previews open full screen and stay keyboard accessible at ${viewport.width}px`, async ({ page }, testInfo) => {
    const hits: string[] = [];
    const sink = createServer((request, response) => { hits.push(request.url ?? ''); response.end(); });
    await new Promise<void>(resolve => sink.listen(0, '127.0.0.1', resolve));
    const sinkOrigin = `http://127.0.0.1:${(sink.address() as AddressInfo).port}`;
    const text = [
      '```mermaid',
      'graph LR',
      '  A[시작] --> B[검토] --> C[끝]',
      '```',
      '',
      `![작은 점](data:image/png;base64,${pixel})`,
      '',
      `![원격 이미지](${sinkOrigin}/remote.png)`,
    ].join('\n');
    const app = await startTestApp();
    const provider = await startMockProvider({ reply: () => ({ text }) });
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.setViewportSize(viewport);
    try {
      await page.goto(app.url);
      await page.evaluate(async baseUrl => {
        const response = await fetch('/api/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ baseUrl, model: 'local-test-model', workspace: '' }) });
        if (!response.ok) throw new Error(await response.text());
      }, provider.baseUrl);
      await expect(page.getByRole('heading', { name: 'Grok', exact: true })).toBeVisible();
      await page.getByTestId('attachment-input').setInputFiles([{ name: 'pixel.png', mimeType: 'image/png', buffer: Buffer.from(pixel, 'base64') }]);
      await page.getByLabel('메시지', { exact: true }).fill('그림을 보여줘');
      await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
      await expect.poll(async () => (await snapshot(page)).conversations[0]?.run.status, { timeout: 30_000 }).toBe('completed');
      const message = page.getByRole('article', { name: 'Grok 메시지', exact: true }).last();

      // Diagram: click to open, zoom with the toolbar and keys, Escape closes.
      const diagram = message.getByRole('img', { name: /^Mermaid 다이어그램/ });
      await expect(diagram).toBeVisible({ timeout: 20_000 });
      await diagram.click();
      const diagramDialog = page.getByRole('dialog', { name: '다이어그램 미리보기', exact: true });
      await expect(diagramDialog).toBeVisible();
      await expect(diagramDialog.getByRole('button', { name: '다이어그램 미리보기 닫기', exact: true })).toBeFocused();
      const zoomed = diagramDialog.getByRole('img', { name: /^Mermaid 다이어그램/ });
      await expect(zoomed).toBeVisible();
      await expect(diagramDialog.locator('output')).toHaveText('맞춤');
      const fitWidth = (await zoomed.boundingBox())!.width;
      await diagramDialog.getByRole('button', { name: '확대', exact: true }).click();
      await expect(diagramDialog.locator('output')).toHaveText('125%');
      await page.keyboard.press('+');
      await expect(diagramDialog.locator('output')).toHaveText('156%');
      await page.keyboard.press('-');
      await expect(diagramDialog.locator('output')).toHaveText('125%');
      await diagramDialog.getByRole('button', { name: '축소', exact: true }).click();
      await expect(diagramDialog.locator('output')).toHaveText('100%');
      await diagramDialog.getByRole('button', { name: '화면에 맞추기', exact: true }).click();
      await expect(diagramDialog.locator('output')).toHaveText('맞춤');
      expect(Math.abs((await zoomed.boundingBox())!.width - fitWidth)).toBeLessThan(2);
      await page.screenshot({ path: testInfo.outputPath(`diagram-preview-${viewport.width}.png`) });
      await page.keyboard.press('Escape');
      await expect(diagramDialog).toHaveCount(0);

      // Keyboard: the labelled button opens it, Tab stays inside, focus returns to the button.
      const openDiagram = message.getByRole('button', { name: '다이어그램 전체 화면으로 열기', exact: true });
      await openDiagram.focus();
      await page.keyboard.press('Enter');
      await expect(diagramDialog).toBeVisible();
      for (let index = 0; index < 8; index++) {
        await page.keyboard.press('Tab');
        expect(await focusInsideDialog(page)).toBe(true);
      }
      await page.keyboard.press('Shift+Tab');
      expect(await focusInsideDialog(page)).toBe(true);
      await page.keyboard.press('Escape');
      await expect(diagramDialog).toHaveCount(0);
      await expect(openDiagram).toBeFocused();

      // Markdown images: data: images open "미디어 미리보기"; remote images stay text and are never fetched.
      const dot = message.getByRole('img', { name: '작은 점', exact: true });
      await expect(dot).toBeVisible();
      await dot.click();
      const media = page.getByRole('dialog', { name: '미디어 미리보기', exact: true });
      await expect(media).toBeVisible();
      await expect(media.getByRole('heading', { name: '작은 점', exact: true })).toBeVisible();
      await expect(media.getByRole('img', { name: '작은 점', exact: true })).toBeVisible();
      await media.getByRole('button', { name: '미디어 미리보기 닫기', exact: true }).click();
      await expect(media).toHaveCount(0);
      await expect(message).toContainText('[이미지 · 원격 이미지]');
      await expect(message.locator(`img[src^="${sinkOrigin}"]`)).toHaveCount(0);

      // A sent image attachment opens the same preview, with a download link.
      const attachment = page.getByRole('list', { name: '사용자 첨부 파일', exact: true });
      const open = attachment.getByRole('button', { name: '이미지 전체 화면으로 열기', exact: true });
      await open.focus();
      await page.keyboard.press('Enter');
      await expect(media).toBeVisible();
      await expect(media.getByRole('img', { name: 'pixel.png', exact: true })).toBeVisible();
      await expect(media.getByRole('link', { name: '미디어 다운로드', exact: true })).toHaveAttribute('download', 'pixel.png');
      await page.screenshot({ path: testInfo.outputPath(`media-preview-${viewport.width}.png`) });
      await page.keyboard.press('Escape');
      await expect(media).toHaveCount(0);
      await expect(open).toBeFocused();
      await attachment.getByRole('img', { name: 'pixel.png', exact: true }).click();
      await expect(media).toBeVisible();
      await page.keyboard.press('Escape');
      await expect(media).toHaveCount(0);

      expect(hits).toEqual([]);
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
      expect(errors).toEqual([]);
    } finally {
      await app.stop();
      await provider.close();
      await new Promise<void>(resolve => sink.close(() => resolve()));
    }
  });
}

// The provider relay turns OpenRouter-style `images` into stored, linked images.
test('provider-generated images appear in the reply and open full screen', async ({ page }) => {
  const app = await startTestApp();
  const provider = await startMockProvider({ reply: request => request.tools?.some(tool => tool.function.name === 'search_tool') ? { images: [`data:image/png;base64,${pixel}`] } : { text: '차트' } });
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  try {
    await page.goto(app.url);
    await page.evaluate(async baseUrl => {
      const response = await fetch('/api/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ baseUrl, model: 'local-test-model', workspace: '' }) });
      if (!response.ok) throw new Error(await response.text());
    }, provider.baseUrl);
    await expect(page.getByRole('heading', { name: 'Grok', exact: true })).toBeVisible();
    await page.getByLabel('메시지', { exact: true }).fill('막대 차트로 보여줘');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    await expect.poll(async () => (await snapshot(page)).conversations[0]?.run.status, { timeout: 30_000 }).toBe('completed');
    const message = page.getByRole('article', { name: 'Grok 메시지', exact: true }).last();
    const image = message.getByRole('img', { name: '생성된 이미지', exact: true });
    await expect(image).toBeVisible();
    await expect.poll(() => image.evaluate((element: HTMLImageElement) => element.complete && element.naturalWidth)).toBe(1);
    expect(await image.getAttribute('src')).toMatch(/^\/api\/generated-images\/[0-9a-f-]{36}$/);
    await image.click();
    await expect(page.getByRole('dialog')).toBeVisible();
    await page.keyboard.press('Escape');
    expect(errors).toEqual([]);
  } finally {
    await provider.close();
    await app.stop();
  }
});
