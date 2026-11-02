import { test, expect, type Page } from '@playwright/test';
import { mkdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import type { AppState } from '../shared/types';
import { startTestApp } from './app-fixture';
import { startMockProvider } from './mock-provider';
import { startMcpAppsFixture, mcpAppsReply } from './mcp-app-fixture';

test.skip(process.env.GROK_BOT_BACKEND !== 'rust', 'MCP apps extend the existing Rust custom-MCP capability.');
const baseline = process.env.MCP_APP_BASELINE === '1';
const snapshot = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());
const api = (page: Page, path: string, body: unknown, method = 'POST') => page.evaluate(async ({ path, body, method }) => {
  const response = await fetch(path, { method, headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify(body) });
  return { status: response.status, body: await response.json() };
}, { path, body, method });

async function setup(page: Page, options: Parameters<typeof startMcpAppsFixture>[0] = {}) {
  const app = await startTestApp();
  const provider = await startMockProvider({ reply: mcpAppsReply });
  const fixture = await startMcpAppsFixture(options);
  await page.goto(app.url);
  await expect(page.getByRole('heading', { name: 'Grok', exact: true })).toBeVisible();
  expect((await api(page, '/api/settings', { baseUrl: provider.baseUrl, model: 'local-test-model', workspace: '' }, 'PUT')).status).toBe(200);
  const bot = (await snapshot(page)).bots[0].id;
  expect((await api(page, `/api/bots/${bot}/mcp-servers`, { json: JSON.stringify({ mcpServers: { counter: { url: fixture.url } } }) }, 'PUT')).status).toBe(200);
  expect((await api(page, `/api/bots/${bot}/mcp-servers/counter`, { enabled: true }, 'PATCH')).status).toBe(200);
  await page.getByRole('textbox', { name: '메시지', exact: true }).fill('MCP 카운터 앱을 열어줘');
  await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
  const approval = page.getByRole('region', { name: '작업 승인 요청', exact: true });
  await expect(approval).toBeVisible({ timeout: 30_000 });
  expect(fixture.calls).toHaveLength(0);
  await approval.getByRole('button', { name: 'Run once', exact: true }).click();
  await expect(page.getByText('MCP 카운터를 열었습니다.', { exact: true })).toBeVisible({ timeout: 30_000 });
  await expect.poll(async () => (await snapshot(page)).conversations[0]?.run.status).toBe('completed');
  return { app, provider, fixture, bot, stop: async () => { await app.stop(); await provider.close(); await fixture.close(); } };
}

for (const viewport of [{ width: 1280, height: 900 }, { width: 390, height: 844 }]) {
  test(`MCP app renders, asks on every call, denies safely and survives reload at ${viewport.width}px`, async ({ page }, info) => {
    await page.setViewportSize(viewport);
    const subject = await setup(page);
    try {
      const capture = async (name: string) => {
        const dir = process.env.MCP_APP_CAPTURE_DIR;
        if (dir) await mkdir(dir, { recursive: true });
        await page.screenshot({ path: dir ? join(dir, `${name}-${viewport.width}.png`) : info.outputPath(`${name}-${viewport.width}.png`), fullPage: true });
      };
      if (baseline) {
        await expect(page.getByRole('figure', { name: 'MCP 카운터 · MCP 앱', exact: true })).toHaveCount(0);
        await capture('before');
        return;
      }
      const card = page.getByRole('figure', { name: 'MCP 카운터 · MCP 앱', exact: true });
      await expect(card).toBeVisible();
      const frame = card.locator('iframe').contentFrame();
      await expect(frame.getByRole('button', { name: '1 더하기', exact: true })).toBeEnabled();
      await expect(card.locator('iframe')).toHaveAttribute('sandbox', 'allow-scripts');
      await expect(frame.locator('#checks')).toHaveText(JSON.stringify({ parent: 'blocked', storage: 'blocked', api: 'blocked', network: 'blocked' }));
      await capture('after');
      const originalRun = (await snapshot(page)).conversations[0].run;
      const providerRequests = subject.provider.requests.length;
      const pending = page.getByRole('region', { name: 'MCP 앱 도구 승인 요청', exact: true });
      // Even an explicit auto-allow rule does not grant iframe tool authority.
      expect((await api(page, '/api/permission-rules', { category: 'mcp_tool', server: 'counter', action: 'allow', botId: subject.bot })).status).toBe(201);
      await frame.getByRole('button', { name: '1 더하기', exact: true }).click();
      await expect(pending).toBeVisible();
      await expect(pending.getByRole('button', { name: '한 번 실행', exact: true })).toBeEnabled();
      expect(subject.fixture.calls).toHaveLength(1);
      await capture('approval');
      await pending.getByRole('button', { name: '거절', exact: true }).click();
      await expect(frame.locator('#status')).toHaveText('거절됨');
      expect(subject.fixture.calls).toHaveLength(1);
      await frame.getByRole('button', { name: '1 더하기', exact: true }).click();
      await pending.getByRole('button', { name: '한 번 실행', exact: true }).click();
      await expect(frame.getByLabel('카운터 값')).toHaveText('1', { timeout: 30_000 });
      expect(subject.fixture.calls).toHaveLength(2);
      expect(subject.provider.requests.length).toBe(providerRequests);
      expect((await snapshot(page)).conversations[0].run).toEqual(originalRun);
      expect(subject.fixture.resourceReads).toContain('ui://counter/app');
      expect(subject.fixture.blocked).toEqual([]);
      // Forge from the host page (and replay a completed frame ID); neither creates a second call.
      await page.evaluate(() => window.postMessage({ jsonrpc: '2.0', id: 77, method: 'tools/call', params: { name: 'counter', arguments: { delta: 1 } } }, '*'));
      await page.evaluate(() => {
        const sibling = document.createElement('iframe');
        sibling.sandbox.add('allow-scripts');
        sibling.srcdoc = '<script>parent.postMessage({jsonrpc:"2.0",id:88,method:"tools/call",params:{name:"counter",arguments:{delta:1}}},"*")<\/script>';
        sibling.setAttribute('data-test-sibling', '');
        document.body.append(sibling);
      });
      await page.waitForTimeout(100); // A sibling can post, but is not this app's WindowProxy.
      await expect(pending).toHaveCount(0);
      await page.locator('[data-test-sibling]').evaluate(element => element.remove());
      await frame.locator('body').evaluate(() => parent.postMessage({ jsonrpc: '2.0', id: 3, method: 'tools/call', params: { name: 'counter', arguments: { delta: 1 } } }, '*'));
      await expect(pending).toHaveCount(0);
      const state = await snapshot(page);
      const saved = JSON.parse(await readFile(join(subject.app.dataDir, 'state.json'), 'utf8')) as AppState;
      const storedApp = state.conversations[0].messages.flatMap(message => message.tools).find(tool => tool.mcpApp)?.mcpApp;
      expect(storedApp?.html).toContain('승인으로 움직이는 카운터');
      expect(saved.conversations[0].messages.flatMap(message => message.tools).some(tool => tool.mcpApp?.html === storedApp?.html)).toBe(true);
      // Hiding the frame abandons its proposal, never approves or replays it.
      await frame.getByRole('button', { name: '1 더하기', exact: true }).click();
      await expect(pending).toBeVisible();
      await card.getByRole('button', { name: '코드 보기', exact: true }).click();
      await expect(pending).toHaveCount(0);
      expect(subject.fixture.calls).toHaveLength(2);
      await card.getByRole('button', { name: '미리보기', exact: true }).click();
      await page.reload();
      await expect(card.locator('iframe').contentFrame().getByRole('button', { name: '1 더하기', exact: true })).toBeEnabled();
      expect(subject.fixture.calls).toHaveLength(2);
      await card.getByRole('button', { name: 'MCP 앱 확대', exact: true }).click();
      const viewer = page.getByRole('dialog', { name: 'MCP 앱 미리보기', exact: true });
      await expect(viewer).toBeVisible();
      const fullscreenButton = viewer.locator('iframe').contentFrame().getByRole('button', { name: '1 더하기', exact: true });
      await expect(fullscreenButton).toBeEnabled();
      await page.keyboard.press('Tab');
      await expect(fullscreenButton).toBeFocused();
      await page.keyboard.press('Enter');
      const fullscreenApproval = viewer.getByRole('region', { name: 'MCP 앱 도구 승인 요청', exact: true });
      await expect(viewer.locator('.mcp-app-announcement')).toHaveText('도구 호출을 검토하고 이번 호출만 승인하거나 거절해 주세요.');
      await expect(fullscreenApproval.getByRole('button', { name: '한 번 실행', exact: true })).not.toBeFocused();
      await fullscreenApproval.getByRole('button', { name: '거절', exact: true }).click();
      await page.keyboard.press('Escape');
      await expect(viewer).toHaveCount(0);
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    } finally { await subject.stop(); }
  });
}

test('a cancelled frame request cannot remain approvable', async ({ page }) => {
  test.skip(baseline);
  const subject = await setup(page);
  try {
    const card = page.getByRole('figure', { name: 'MCP 카운터 · MCP 앱', exact: true });
    const frame = card.locator('iframe').contentFrame();
    await frame.getByRole('button', { name: '1 더하기', exact: true }).click();
    const pending = page.getByRole('region', { name: 'MCP 앱 도구 승인 요청', exact: true });
    await expect(pending.getByRole('button', { name: '한 번 실행', exact: true })).toBeEnabled();
    await frame.locator('body').evaluate(() => parent.postMessage({ jsonrpc: '2.0', method: 'notifications/cancelled', params: { requestId: 2 } }, '*'));
    await expect(pending).toHaveCount(0);
    expect(subject.fixture.calls).toHaveLength(1);
  } finally { await subject.stop(); }
});

test('late proposal and approved-response delivery cannot revive or target a replacement frame', async ({ page }) => {
  test.skip(baseline);
  const subject = await setup(page);
  try {
    const card = page.getByRole('figure', { name: 'MCP 카운터 · MCP 앱', exact: true });
    const frame = card.locator('iframe').contentFrame();
    let proposed!: (id: string) => void;
    const proposedId = new Promise<string>(resolve => { proposed = resolve; });
    let releaseProposal!: () => void;
    const proposalGate = new Promise<void>(resolve => { releaseProposal = resolve; });
    await page.route('**/mcp-apps/*/requests', async route => {
      const response = await route.fetch();
      proposed((await response.json()).requestId);
      await proposalGate;
      await route.fulfill({ response });
    });
    await frame.getByRole('button', { name: '1 더하기', exact: true }).click();
    const id = await proposedId;
    await card.getByRole('button', { name: '코드 보기', exact: true }).click();
    const deniedResponse = page.waitForResponse(response => response.url().endsWith(`/requests/${id}`) && response.request().postDataJSON()?.allow === false);
    releaseProposal();
    expect((await deniedResponse).status()).toBe(200);
    await page.unroute('**/mcp-apps/*/requests');
    await card.getByRole('button', { name: '미리보기', exact: true }).click();
    await expect(frame.getByLabel('카운터 값')).toHaveText('0');
    expect(subject.fixture.calls).toHaveLength(1);

    let executed!: () => void;
    const executedCall = new Promise<void>(resolve => { executed = resolve; });
    let releaseResult!: () => void;
    const resultGate = new Promise<void>(resolve => { releaseResult = resolve; });
    await page.route('**/mcp-apps/*/requests/*', async route => {
      if (route.request().postDataJSON()?.allow !== true) { await route.continue(); return; }
      const response = await route.fetch();
      executed();
      await resultGate;
      await route.fulfill({ response });
    });
    await frame.getByRole('button', { name: '1 더하기', exact: true }).click();
    await page.getByRole('region', { name: 'MCP 앱 도구 승인 요청', exact: true }).getByRole('button', { name: '한 번 실행', exact: true }).click();
    await executedCall;
    expect(subject.fixture.calls).toHaveLength(2);
    await card.getByRole('button', { name: '코드 보기', exact: true }).click();
    await card.getByRole('button', { name: '미리보기', exact: true }).click();
    await expect(frame.getByRole('button', { name: '1 더하기', exact: true })).toBeEnabled();
    const delivered = page.waitForResponse(response => response.url().includes('/mcp-apps/') && response.request().postDataJSON()?.allow === true);
    releaseResult();
    await delivered;
    await page.waitForTimeout(100); // Let a stale postMessage arrive if the generation guard is broken.
    await expect(frame.getByLabel('카운터 값')).toHaveText('0');
    expect(subject.fixture.calls).toHaveLength(2);
  } finally { await subject.stop(); }
});

test('app requests are one-use, target-bound and invalidated when the server changes', async ({ page }) => {
  test.skip(baseline);
  const subject = await setup(page);
  try {
    const state = await snapshot(page);
    const conversation = state.conversations[0];
    const tool = conversation.messages.flatMap(message => message.tools).find(tool => tool.mcpApp)!;
    const path = `/api/conversations/${conversation.id}/mcp-apps/${tool.id}/requests`;
    expect((await api(page, path, { name: 'other_server__delete', arguments: {} })).status).toBeGreaterThanOrEqual(400);
    expect((await api(page, path, { name: 'counter', arguments: { text: '한'.repeat(12_000) } })).status).toBeGreaterThanOrEqual(400);
    const created = await api(page, path, { name: 'counter', arguments: { delta: 1 } });
    expect(created.status).toBeLessThan(300);
    expect(subject.fixture.calls).toHaveLength(1);
    const decisionPath = `${path}/${created.body.requestId}`;
    expect((await api(page, decisionPath, { allow: false })).status).toBeLessThan(300);
    expect((await api(page, decisionPath, { allow: true })).status).toBeGreaterThanOrEqual(400);
    const stale = await api(page, path, { name: 'counter', arguments: { delta: 1 } });
    expect((await api(page, `/api/bots/${subject.bot}/mcp-servers/counter`, { enabled: false }, 'PATCH')).status).toBe(200);
    expect((await api(page, `${path}/${stale.body.requestId}`, { allow: true })).status).toBeGreaterThanOrEqual(400);
    expect(subject.fixture.calls).toHaveLength(1);
    await page.reload();
    await expect(page.getByRole('figure', { name: 'MCP 카운터 · MCP 앱', exact: true })).toBeVisible();
  } finally { await subject.stop(); }
});

for (const options of [{ html: 'x'.repeat(200_001) }, { mimeType: 'text/plain' }, { resourceUri: 'ui://another/app' }]) {
  test(`invalid UI resource falls back without executing HTML: ${Object.keys(options)[0]}`, async ({ page }) => {
    test.skip(baseline);
    const subject = await setup(page, options);
    try {
      await expect(page.locator('.mcp-app-card')).toHaveCount(0);
      expect((await snapshot(page)).conversations[0].messages.flatMap(message => message.tools).some(tool => tool.mcpApp)).toBe(false);
      expect(subject.fixture.calls).toHaveLength(1);
    } finally { await subject.stop(); }
  });
}
