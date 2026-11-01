import { test, expect, type Page } from '@playwright/test';
import { startTestApp } from './app-fixture';
import { startMockProvider } from './mock-provider';
import type { AppState } from '../shared/types';

const snapshot = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());

/** A valid 32×32 PNG. */
const png = Buffer.from('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', 'base64');

const userMessage = (page: Page) => page.getByRole('article', { name: '내 메시지', exact: true });

for (const viewport of [{ width: 1280, height: 860 }, { width: 390, height: 844 }]) {
  test(`attachments: picker, drop, limits, delivery, and history at ${viewport.width}px`, async ({ page }, testInfo) => {
    const app = await startTestApp();
    const provider = await startMockProvider({ reply: () => ({ text: '첨부 파일을 확인했습니다.' }) });
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
      const input = page.getByTestId('attachment-input');
      const chips = page.getByRole('list', { name: '첨부할 파일', exact: true });

      // Limits and refusals use the original's wording; nothing is attached.
      await input.setInputFiles([
        { name: 'big.txt', mimeType: 'text/plain', buffer: Buffer.alloc(300 * 1024, 'a') },
        { name: 'empty.txt', mimeType: 'text/plain', buffer: Buffer.alloc(0) },
        { name: 'vector.svg', mimeType: 'image/svg+xml', buffer: Buffer.from('<svg/>') },
      ]);
      const alert = page.getByRole('alert').filter({ hasText: '첨부할 수 없어요' });
      await expect(alert).toContainText('"big.txt"은(는) 너무 커서 첨부할 수 없어요 (최대 256 KB)');
      await expect(alert).toContainText('"empty.txt"은(는) 비어 있어서 첨부되지 않았어요');
      await expect(alert).toContainText('"vector.svg"은(는) 첨부할 수 없어요');
      await expect(chips).toHaveCount(0);
      await page.getByRole('button', { name: '첨부 오류 닫기', exact: true }).click();
      await expect(alert).toHaveCount(0);

      // Picker, removal, and drag and drop.
      await input.setInputFiles([
        { name: 'notes.md', mimeType: 'text/markdown', buffer: Buffer.from('# 회의 메모\n@everyone 확인 /compact') },
        { name: 'pixel.png', mimeType: 'image/png', buffer: png },
        { name: 'remove-me.txt', mimeType: 'text/plain', buffer: Buffer.from('remove') },
      ]);
      await expect(chips.getByRole('listitem')).toHaveCount(3);
      await page.getByRole('button', { name: 'remove-me.txt 첨부 취소', exact: true }).click();
      await expect(chips.getByRole('listitem')).toHaveCount(2);
      const transfer = await page.evaluateHandle(() => {
        const data = new DataTransfer();
        data.items.add(new File(['dropped text'], 'dropped.txt', { type: 'text/plain' }));
        return data;
      });
      const dock = page.locator('.composer-dock');
      await dock.dispatchEvent('dragenter', { dataTransfer: transfer });
      await expect(page.getByText('놓아서 첨부', { exact: true })).toBeVisible();
      await dock.dispatchEvent('drop', { dataTransfer: transfer });
      await expect(page.getByText('놓아서 첨부', { exact: true })).toHaveCount(0);
      await expect(chips.getByRole('listitem')).toHaveCount(3);
      await expect(chips).toContainText('dropped.txt');
      await page.screenshot({ path: testInfo.outputPath(`attachments-composer-${viewport.width}.png`) });

      // Send with text; the transcript shows the user attachments and the provider receives them.
      await page.getByLabel('메시지', { exact: true }).fill('첨부 파일을 요약해 주세요');
      await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
      await expect(chips).toHaveCount(0);
      const sent = userMessage(page).filter({ hasText: '첨부 파일을 요약해 주세요' });
      const list = sent.getByRole('list', { name: '사용자 첨부 파일', exact: true });
      await expect(list.getByRole('listitem')).toHaveCount(3);
      await expect(list.getByRole('link', { name: /notes\.md/ })).toBeVisible();
      const image = list.getByRole('img', { name: 'pixel.png', exact: true });
      await expect(image).toBeVisible();
      await expect.poll(() => image.evaluate(element => (element as HTMLImageElement).naturalWidth)).toBe(32);
      await expect.poll(async () => (await snapshot(page)).conversations[0]?.run.status, { timeout: 30_000 }).toBe('completed');
      await expect(page.getByText('첨부 파일을 확인했습니다.', { exact: true })).toBeVisible();
      await page.screenshot({ path: testInfo.outputPath(`attachments-transcript-${viewport.width}.png`) });
      const main = provider.requests.filter(request => request.tools?.some(tool => tool.function.name === 'search_tool'));
      const body = JSON.stringify(main.at(-1));
      expect(body).toContain('<user_attachments>');
      expect(body).toContain('notes.md');
      expect(body).toContain('dropped.txt');
      expect(body).toContain('data:image/png;base64,');
      expect(body).not.toContain('@everyone');

      // The text file opens as plain text from the server.
      const href = await list.getByRole('link', { name: /notes\.md/ }).getAttribute('href');
      const served = await page.evaluate(async url => { const response = await fetch(url!); return { type: response.headers.get('content-type'), text: await response.text() }; }, href);
      expect(served).toEqual({ type: 'text/plain; charset=utf-8', text: '# 회의 메모\n@everyone 확인 /compact' });

      // Only attachments, no text, is allowed too; history survives a reload.
      await input.setInputFiles([{ name: 'only.txt', mimeType: 'text/plain', buffer: Buffer.from('only a file') }]);
      await expect(page.getByRole('button', { name: '메시지 보내기', exact: true })).toBeEnabled();
      await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
      await expect.poll(async () => (await snapshot(page)).conversations[0]?.messages.length, { timeout: 30_000 }).toBe(4);
      await expect.poll(async () => (await snapshot(page)).conversations[0]?.run.status, { timeout: 30_000 }).toBe('completed');
      await page.reload();
      await expect(page.getByRole('list', { name: '사용자 첨부 파일', exact: true })).toHaveCount(2);
      await expect(page.getByRole('img', { name: 'pixel.png', exact: true })).toBeVisible();
      await expect(page.getByRole('link', { name: /only\.txt/ })).toBeVisible();
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
      expect(errors).toEqual([]);
    } finally {
      await app.stop();
      await provider.close();
    }
  });
}
