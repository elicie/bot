import { test, expect, type Page } from '@playwright/test';
import { startTestApp } from './app-fixture';
import { startMockProvider } from './mock-provider';
import type { AppState } from '../shared/types';

const snapshot = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());

const answer = [
  '질량-에너지 등가: $E=mc^2$ 입니다.',
  '',
  '정답은 **6월(25)**입니다. 출처는 **“백서”**이고 **굵게**만 써도 됩니다.',
  '',
  '$$',
  '\\int_0^1 x\\,dx = \\frac{1}{2}',
  '$$',
  '',
  '잘못된 수식 $\\frac{1}{$ 은 그대로 보여요.',
  '',
  '링크 수식 $\\href{javascript:alert(1)}{click}$ 은 실행되지 않아요.',
  '',
  '```mermaid',
  'graph TD',
  '  A[시작] --> B{확인}',
  '  B --> C[끝]',
  '```',
  '',
  '```mermaid',
  'notADiagram',
  '  this is not valid',
  '```',
  '',
  '<img src=x onerror="window.__injected=1"> <script>window.__injected=2</script>',
  '',
  '```js',
  'const price = "$5 and $10";',
  '```',
].join('\n');

for (const viewport of [{ width: 1280, height: 900 }, { width: 390, height: 844 }]) {
  test(`assistant math and Mermaid diagrams render safely and lazily at ${viewport.width}px`, async ({ page }, testInfo) => {
    const app = await startTestApp();
    const provider = await startMockProvider({ reply: () => ({ text: answer }) });
    const errors: string[] = [];
    const scripts: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    page.on('dialog', dialog => { errors.push(`dialog: ${dialog.message()}`); void dialog.dismiss(); });
    page.on('request', request => scripts.push(request.url()));
    await page.setViewportSize(viewport);
    try {
      await page.goto(app.url);
      await page.evaluate(async baseUrl => {
        const response = await fetch('/api/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ baseUrl, model: 'local-test-model', workspace: '' }) });
        if (!response.ok) throw new Error(await response.text());
      }, provider.baseUrl);
      await expect(page.getByRole('heading', { name: 'Grok', exact: true })).toBeVisible();
      expect(scripts.filter(url => /katex|mermaid/i.test(url))).toEqual([]);

      await page.getByLabel('메시지', { exact: true }).fill('수식과 다이어그램을 보여줘');
      await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
      await expect.poll(async () => (await snapshot(page)).conversations[0]?.run.status, { timeout: 30_000 }).toBe('completed');
      const message = page.getByRole('article', { name: 'Grok 메시지', exact: true }).last();

      // KaTeX: inline and display math, an invalid formula shown as its source, no trusted commands.
      await expect(message.locator('.katex').first()).toBeVisible();
      await expect(message.locator('.katex-display')).toHaveCount(1);
      await expect(message.locator('.katex-display annotation')).toHaveText('\\int_0^1 x\\,dx = \\frac{1}{2}');
      await expect(message.locator('.katex annotation').first()).toHaveText('E=mc^2');
      await expect(message.locator('.katex-error').first()).toContainText('\\frac{1}{');
      await expect(message.locator('a[href^="javascript"]')).toHaveCount(0);

      // Bold closes before Hangul even after punctuation (CommonMark alone leaves the asterisks).
      await expect(message.locator('strong')).toHaveText(['6월(25)', '“백서”', '굵게']);
      await expect(message.getByText(/\*\*/)).toHaveCount(0);

      // Mermaid: the valid diagram becomes an image; the invalid one falls back to code.
      const diagram = message.getByRole('img', { name: /^Mermaid 다이어그램/ });
      await expect(diagram).toBeVisible({ timeout: 20_000 });
      await expect.poll(() => diagram.evaluate(element => (element as HTMLImageElement).naturalWidth)).toBeGreaterThan(0);
      await expect(message.getByText('다이어그램을 표시할 수 없어 코드로 보여드려요.', { exact: true })).toBeVisible();
      await expect(message.locator('.diagram-fallback code')).toContainText('notADiagram');
      await page.screenshot({ path: testInfo.outputPath(`markdown-math-${viewport.width}.png`), fullPage: true });
      expect(scripts.some(url => /katex/i.test(url))).toBe(true);
      expect(scripts.some(url => /mermaid/i.test(url))).toBe(true);

      // Raw HTML is never rendered or executed; code keeps literal dollars.
      await expect(message.locator('img[src="x"], script')).toHaveCount(0);
      expect(await page.evaluate(() => (window as unknown as { __injected?: number }).__injected)).toBeUndefined();
      await expect(message.locator('pre code').filter({ hasText: 'const price' })).toHaveText('const price = "$5 and $10";');
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
      await page.reload();
      await expect(page.getByRole('article', { name: 'Grok 메시지', exact: true }).last().locator('.katex-display')).toHaveCount(1);
      await expect(page.getByRole('img', { name: /^Mermaid 다이어그램/ })).toBeVisible({ timeout: 20_000 });
      expect(errors).toEqual([]);
    } finally {
      await app.stop();
      await provider.close();
    }
  });
}
