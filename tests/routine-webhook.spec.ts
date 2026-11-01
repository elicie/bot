import { test, expect, type Page } from '@playwright/test';
import type { AppState } from '../shared/types';
import { startTestApp } from './app-fixture';
import { startMockProvider } from './mock-provider';

const state = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());

async function mutate(page: Page, method: string, path: string, body: unknown) {
  return page.evaluate(async ({ method, path, body }) => {
    const response = await fetch(path, { method, headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify(body) });
    return { status: response.status, body: await response.json() };
  }, { method, path, body });
}

/** An external sender: plain HTTP from Node, no Origin or X-Grok-Bot headers. */
async function trigger(url: string, key: string | null, body?: string) {
  const response = await fetch(url, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', ...(key ? { Authorization: `Bearer ${key}` } : {}) },
    body,
  });
  return { status: response.status, body: await response.json().catch(() => null) };
}

test('routine webhook keys are created, shown once, rotated, and revoked; authenticated requests run the routine', async ({ page, context }, testInfo) => {
  const app = await startTestApp();
  const provider = await startMockProvider({
    reply: request => ({ text: request.tools?.some(tool => tool.function.name === 'search_tool') ? '웹훅 루틴 응답입니다.' : 'Webhook title' }),
  });
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  await context.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: app.url });
  try {
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto(app.url);
    await expect(page.getByRole('heading', { name: 'Grok', exact: true })).toBeVisible();
    expect((await mutate(page, 'PUT', '/api/settings', { baseUrl: provider.baseUrl, model: 'local-test-model', workspace: '' })).status).toBe(200);
    const bot = (await state(page)).bots[0];
    const created = await mutate(page, 'POST', '/api/routines', {
      botId: bot.id, name: '배포 알림', prompt: '웹훅 내용을 요약해 줘', schedule: { kind: 'interval', minutes: 60 }, enabled: true,
    });
    expect(created.status).toBe(201);
    const routine = (created.body as AppState).routines[0];
    const endpoint = `${app.url}/hooks/routines/${routine.id}`;

    await page.getByRole('button', { name: '루틴', exact: true }).click();
    const section = page.getByRole('region', { name: '웹훅', exact: true });
    await expect(section.getByRole('heading', { name: '웹훅이 발생할 때', exact: true })).toBeVisible();
    await expect(section.getByText(/웹훅 키가 없어요/)).toBeVisible();
    expect((await trigger(endpoint, 'gbw_guess')).status).toBe(401);

    // Create: the key is shown once with the endpoint, header line, and example request.
    await section.getByRole('button', { name: '웹훅 키 만들기', exact: true }).click();
    const keyField = section.locator('code[aria-label="웹훅 키"]');
    await expect(keyField).toHaveText(/^gbw_[0-9a-f]{64}$/);
    const key = (await keyField.textContent())!;
    await expect(section.locator('code[aria-label="POST 대상"]')).toHaveText(endpoint);
    await expect(section.locator('code[aria-label="헤더"]')).toHaveText(`Authorization: Bearer ${key}`);
    await expect(section.locator('code[aria-label="요청 예시"]')).toContainText(`curl -X POST '${endpoint}'`);
    await section.getByRole('button', { name: 'POST 대상 복사', exact: true }).click();
    await expect(section.getByRole('button', { name: 'POST 대상 복사', exact: true })).toContainText('복사됨');
    expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(endpoint);
    expect(JSON.stringify(await state(page))).not.toContain(key);
    expect((await state(page)).routines[0].webhook).toEqual({ keyHint: key.slice(-4), createdAt: expect.any(String) });
    await page.screenshot({ path: testInfo.outputPath('routine-webhook-created.png'), fullPage: true });

    // An authenticated request (JSON body) runs the routine once with the payload as data.
    expect((await trigger(endpoint, `${key}0`)).status).toBe(401);
    expect((await trigger(endpoint, null)).status).toBe(401);
    expect((await trigger(endpoint, key, 'x'.repeat(16 * 1024 + 1))).status).toBe(413);
    const accepted = await trigger(endpoint, key, JSON.stringify({ deploy: 'v1.2.3' }));
    expect(accepted.status).toBe(202);
    expect(accepted.body.run.trigger).toBe('webhook');
    expect(accepted.body.state).toBeUndefined();
    await expect.poll(async () => (await state(page)).routineRuns[0]?.result?.status, { timeout: 30_000 }).toBe('completed');
    const run = (await state(page)).routineRuns[0];
    expect(run.trigger).toBe('webhook');
    expect(run.prompt).toContain('<webhook_payload>\n{"deploy":"v1.2.3"}\n</webhook_payload>');
    await expect(page.getByRole('region', { name: '루틴 실행 기록', exact: true }).getByText(/웹훅 실행/)).toBeVisible();

    // After a reload the key is gone from the browser; only the hint remains.
    await page.reload();
    await page.getByRole('button', { name: '루틴', exact: true }).click();
    await expect(keyField).toHaveText(`gbw_…${key.slice(-4)}`);
    await expect(section.locator('code[aria-label="헤더"]')).toHaveText('Authorization: Bearer <key>');

    // Rotate: the old key stops working.
    page.once('dialog', dialog => dialog.accept());
    await section.getByRole('button', { name: '새 키로 교체', exact: true }).click();
    await expect(keyField).not.toHaveText(/…/);
    const rotated = (await keyField.textContent())!;
    expect(rotated).not.toBe(key);
    expect((await trigger(endpoint, key)).status).toBe(401);
    expect((await trigger(endpoint, rotated, 'plain text')).status).toBe(202);
    await expect.poll(async () => (await state(page)).routineRuns.filter(item => item.result?.status === 'completed').length, { timeout: 30_000 }).toBe(2);

    // Revoke.
    page.once('dialog', dialog => dialog.accept());
    await section.getByRole('button', { name: '웹훅 키 삭제', exact: true }).click();
    await expect(section.getByText(/웹훅 키가 없어요/)).toBeVisible();
    expect((await trigger(endpoint, rotated)).status).toBe(401);
    expect((await state(page)).routines[0].webhook).toBeUndefined();

    await page.setViewportSize({ width: 390, height: 844 });
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    expect(errors).toEqual([]);
  } finally {
    await app.stop();
    await provider.close();
  }
});
