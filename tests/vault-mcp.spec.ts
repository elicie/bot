import { test, expect, type Page } from '@playwright/test';
import { createServer, type IncomingMessage, type ServerResponse } from 'node:http';
import type { AddressInfo } from 'node:net';
import { networkInterfaces } from 'node:os';
import type { AppState } from '../shared/types';
import { startTestApp } from './app-fixture';
import { startMockProvider, type MockCompletion, type MockProviderRequest } from './mock-provider';

// Vault secrets and custom MCP servers are implemented by the Rust server only.
test.skip(process.env.GROK_BOT_BACKEND !== 'rust', 'Vault and custom MCP servers run against the Rust server.');

const SECRET = 'e2e-vault-secret-value-31337';
const snapshot = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());
const details = (page: Page) => page.getByRole('complementary', { name: '대화 세부 정보', exact: true });

/** A streamable-HTTP MCP fixture with one `lookup` tool that records request headers. */
async function startMcpFixture() {
  const authorization: string[] = [];
  const calls: unknown[] = [];
  const server = createServer(async (req: IncomingMessage, res: ServerResponse) => {
    if (req.method !== 'POST') { res.writeHead(405).end(); return; }
    authorization.push(String(req.headers.authorization ?? ''));
    let body = '';
    for await (const part of req) body += part.toString();
    const request = JSON.parse(body) as { id?: number | string; method: string; params?: Record<string, unknown> };
    if (request.id === undefined) { res.writeHead(202).end(); return; }
    const results: Record<string, unknown> = {
      initialize: { protocolVersion: request.params?.protocolVersion, capabilities: { tools: { listChanged: false } }, serverInfo: { name: 'e2e-fixture', version: '1.0.0' } },
      ping: {},
      'tools/list': { tools: [{ name: 'lookup', description: 'Look up an e2e fixture record.', inputSchema: { type: 'object', properties: { query: { type: 'string' } }, required: ['query'] } }] },
    };
    let result = results[request.method];
    if (request.method === 'tools/call') {
      calls.push(request.params);
      result = { content: [{ type: 'text', text: 'e2e-fixture-record: found' }] };
    }
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify(result === undefined ? { jsonrpc: '2.0', id: request.id, error: { code: -32601, message: 'Method not found' } } : { jsonrpc: '2.0', id: request.id, result }));
  });
  await new Promise<void>(resolve => server.listen(0, '127.0.0.1', resolve));
  const url = `http://127.0.0.1:${(server.address() as AddressInfo).port}/mcp`;
  return { url, authorization, calls, close: () => new Promise<void>(resolve => { server.closeAllConnections(); server.close(() => resolve()); }) };
}

/** search_tool, then the fixture tool, then a final answer. */
function reply(request: MockProviderRequest): MockCompletion {
  if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: 'Vault conversation' };
  const last = request.messages.findLastIndex(message => message.role === 'user' && JSON.stringify(message.content).includes('look it up'));
  if (last < 0) return { text: '보관함 테스트 응답입니다.' };
  const calls = request.messages.slice(last + 1).flatMap(message => message.tool_calls ?? []) as Array<{ function?: { name?: string } }>;
  if (!calls.some(call => call.function?.name === 'search_tool')) return { tool: { name: 'search_tool', arguments: { query: 'fixture lookup', limit: 3 } } };
  if (!calls.some(call => call.function?.name === 'use_tool')) return { tool: { name: 'use_tool', arguments: { tool_name: 'fixture__lookup', tool_input: { query: 'e2e' } } } };
  return { text: '사용자 지정 MCP 조회를 마쳤습니다.' };
}

async function configure(page: Page, baseUrl: string) {
  const status = await page.evaluate(async (baseUrl) => (await fetch('/api/settings', {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' },
    body: JSON.stringify({ baseUrl, model: 'local-test-model', workspace: '' }),
  })).status, baseUrl);
  expect(status).toBe(200);
}

async function openVault(page: Page) {
  await page.getByRole('button', { name: '설정', exact: true }).click();
  const dialog = page.getByRole('dialog', { name: '설정', exact: true });
  await dialog.getByRole('button', { name: '보관함', exact: true }).click();
  await expect(dialog.getByRole('heading', { name: /Grok Bot 보관함/ })).toBeVisible();
  return dialog;
}

test('loopback: secrets are write-only, the MCP JSON editor saves servers, and a custom tool asks before it runs', async ({ page }, info) => {
  const app = await startTestApp();
  const provider = await startMockProvider({ reply });
  const fixture = await startMcpFixture();
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  try {
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto(app.url);
    await expect(page.getByRole('heading', { name: 'Grok', exact: true })).toBeVisible();
    await configure(page, provider.baseUrl);

    // 보관함: add a secret; the value never comes back.
    const vault = await openVault(page);
    await expect(vault.getByText('아직 시크릿이 없습니다', { exact: true })).toBeVisible();
    await vault.getByRole('button', { name: '새 시크릿 추가', exact: true }).click();
    const form = page.getByRole('dialog', { name: '새 시크릿 추가', exact: true });
    await form.getByLabel('환경 변수 이름', { exact: true }).fill('api-token');
    await expect(form.getByLabel('환경 변수 이름', { exact: true })).toHaveValue('API-TOKEN');
    await expect(form.getByText('문자로 시작하는 대문자 환경 변수 이름(A-Z, 0-9, _)을 사용해 주세요')).toBeVisible();
    await form.getByLabel('환경 변수 이름', { exact: true }).fill('API_TOKEN');
    await form.getByLabel('시크릿 값', { exact: true }).fill('temporary-value-1');
    await expect(form.getByLabel('시크릿 값', { exact: true })).toHaveAttribute('type', 'password');
    await form.getByRole('button', { name: '시크릿 저장', exact: true }).click();
    await expect(form).toBeHidden();
    const row = vault.getByRole('listitem', { name: 'API_TOKEN · 모든 Bot', exact: true });
    await expect(row).toBeVisible();
    await expect(row.getByLabel('값 숨김')).toHaveText('••••••••');
    await row.getByRole('button', { name: 'API_TOKEN 교체', exact: true }).click();
    const replace = page.getByRole('dialog', { name: 'API_TOKEN 시크릿 교체', exact: true });
    await replace.getByLabel('시크릿 값', { exact: true }).fill(SECRET);
    await replace.getByRole('button', { name: '시크릿 저장', exact: true }).click();
    await expect(vault.getByText('API_TOKEN 시크릿을 교체했어요.', { exact: true })).toBeVisible();
    await vault.getByLabel('보관함 검색', { exact: true }).fill('nothing');
    await expect(vault.getByText('검색과 일치하는 시크릿이 없어요', { exact: true })).toBeVisible();
    await vault.getByLabel('보관함 검색', { exact: true }).fill('api');
    await expect(row).toBeVisible();
    await page.screenshot({ path: info.outputPath('vault-settings.png') });
    expect(JSON.stringify(await snapshot(page))).not.toContain(SECRET);
    expect(JSON.stringify(await snapshot(page))).not.toContain('temporary-value-1');
    await vault.getByRole('button', { name: '설정 닫기', exact: true }).click();

    // Details pane: MCP 서버 JSON with a secret reference; servers start disabled.
    await page.getByRole('button', { name: '봇 편집', exact: true }).first().click();
    await expect(details(page).getByRole('heading', { name: 'MCP 서버', exact: true })).toBeVisible();
    await expect(details(page).getByText('아직 사용자 지정 MCP 서버가 없어요', { exact: true })).toBeVisible();
    await details(page).getByRole('button', { name: '사용자 지정 MCP 추가', exact: true }).click();
    const editor = page.getByRole('dialog', { name: 'MCP 서버 JSON', exact: true });
    await expect(editor.getByRole('note')).toContainText('서버 컴퓨터에서 이 앱과 같은 사용자 권한으로 실행돼요');
    const json = editor.getByLabel('MCP 서버 JSON', { exact: true });
    await expect(json).toHaveValue(/"mcpServers": \{\}/);
    await json.fill('{"mcpServers": {"workspace": {"url": "http://127.0.0.1:1/mcp"}}}');
    await editor.getByRole('button', { name: '저장', exact: true }).click();
    await expect(editor.getByRole('alert')).toContainText('workspace');
    await json.fill(JSON.stringify({ mcpServers: { fixture: { url: fixture.url, headers: { Authorization: 'Bearer ${secret:API_TOKEN}' } } } }, null, 2));
    await page.screenshot({ path: info.outputPath('mcp-json-editor.png') });
    await editor.getByRole('button', { name: '저장', exact: true }).click();
    await expect(editor).toBeHidden();
    const server = details(page).getByRole('listitem', { name: 'MCP 서버 fixture', exact: true });
    await expect(server).toContainText('HTTP');
    const toggle = server.getByRole('switch', { name: 'fixture 사용', exact: true });
    await expect(toggle).not.toBeChecked();
    await toggle.click();
    await expect(toggle).toBeChecked();
    await expect.poll(async () => (await snapshot(page)).mcpServers?.map(item => [item.name, item.enabled, item.secretRefs])).toEqual([['fixture', true, ['API_TOKEN']]]);
    const state = await snapshot(page);
    expect(JSON.stringify(state)).not.toContain(fixture.url);
    await expect(details(page).getByRole('list', { name: '시크릿 목록' })).toContainText('API_TOKEN');

    // The custom tool shows an approval card, then runs with the secret header.
    await page.getByRole('textbox', { name: '메시지', exact: true }).fill('Please look it up');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    const approval = page.getByRole('region', { name: '작업 승인 요청', exact: true });
    await expect(approval).toBeVisible({ timeout: 30_000 });
    await expect(approval).toContainText('MCP 서버 fixture: lookup');
    await page.screenshot({ path: info.outputPath('mcp-approval.png') });
    expect(fixture.calls).toHaveLength(0);
    await approval.getByRole('button', { name: 'Run once', exact: true }).click();
    await expect(page.getByText('사용자 지정 MCP 조회를 마쳤습니다.', { exact: true })).toBeVisible({ timeout: 30_000 });
    expect(fixture.calls).toHaveLength(1);
    expect(fixture.authorization.length).toBeGreaterThan(0);
    expect(fixture.authorization.every(value => value === `Bearer ${SECRET}`)).toBe(true);
    expect(JSON.stringify(provider.requests)).not.toContain(SECRET);
    expect(JSON.stringify(await snapshot(page))).not.toContain(SECRET);

    // Delete the secret.
    page.once('dialog', confirm => confirm.accept());
    await details(page).getByRole('button', { name: 'API_TOKEN 삭제', exact: true }).click();
    await expect.poll(async () => (await snapshot(page)).secrets ?? []).toEqual([]);
    expect(errors).toEqual([]);
  } finally {
    await app.stop();
    await provider.close();
    await fixture.close();
  }
});

test('a LAN client sees secret names and server summaries read-only and cannot change them', async ({ browser }, info) => {
  const hostname = Object.values(networkInterfaces()).flat().find(address => address?.family === 'IPv4' && !address.internal)?.address;
  if (!hostname) throw Error('This LAN verification requires a network interface');
  const app = await startTestApp({ network: true, hostname });
  const loopback = await browser.newContext();
  const lan = await browser.newContext();
  try {
    // Set up from the server computer (loopback).
    const local = await loopback.newPage();
    await local.goto(app.url.replace(hostname, '127.0.0.1'));
    const bot = (await snapshot(local)).bots[0].id;
    const created = await local.evaluate(async ({ bot, secret }) => {
      const headers = { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' };
      const add = await fetch('/api/secrets', { method: 'POST', headers, body: JSON.stringify({ name: 'LAN_TOKEN', value: secret, botId: bot }) });
      const json = JSON.stringify({ mcpServers: { tools: { command: 'node', args: ['server.js'], env: { TOKEN: '${secret:LAN_TOKEN}' } } } });
      const save = await fetch(`/api/bots/${bot}/mcp-servers`, { method: 'PUT', headers, body: JSON.stringify({ json }) });
      const enable = await fetch(`/api/bots/${bot}/mcp-servers/tools`, { method: 'PATCH', headers, body: JSON.stringify({ enabled: true }) });
      return [add.status, save.status, enable.status];
    }, { bot, secret: SECRET });
    expect(created).toEqual([201, 200, 200]);

    const page = await lan.newPage();
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto(app.url);
    const vault = await openVault(page);
    await expect(vault.getByRole('note').filter({ hasText: 'localhost 연결이 아니에요' })).toBeVisible();
    await expect(vault.getByRole('listitem', { name: 'LAN_TOKEN · Grok', exact: true })).toBeVisible();
    await expect(vault.getByRole('button', { name: '새 시크릿 추가', exact: true })).toBeDisabled();
    await expect(vault.getByRole('button', { name: 'LAN_TOKEN 교체', exact: true })).toBeDisabled();
    await expect(vault.getByRole('button', { name: 'LAN_TOKEN 삭제', exact: true })).toBeDisabled();
    await page.screenshot({ path: info.outputPath('vault-lan.png') });
    await vault.getByRole('button', { name: '설정 닫기', exact: true }).click();

    await page.getByRole('button', { name: '봇 편집', exact: true }).first().click();
    const server = details(page).getByRole('listitem', { name: 'MCP 서버 tools', exact: true });
    await expect(server).toContainText('STDIO');
    await expect(details(page).getByRole('button', { name: 'MCP 서버 JSON', exact: true })).toBeDisabled();
    // Disabling is a tightening and stays available; enabling again is not.
    const toggle = server.getByRole('switch', { name: 'tools 사용', exact: true });
    await expect(toggle).toBeChecked();
    await toggle.click();
    await expect(toggle).not.toBeChecked();
    await expect(toggle).toBeDisabled();

    const results = await page.evaluate(async ({ bot }) => {
      const headers = { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' };
      const add = await fetch('/api/secrets', { method: 'POST', headers, body: JSON.stringify({ name: 'OTHER_TOKEN', value: 'lan-attempted-value' }) });
      const read = await fetch(`/api/bots/${bot}/mcp-servers`);
      const save = await fetch(`/api/bots/${bot}/mcp-servers`, { method: 'PUT', headers, body: JSON.stringify({ json: '{"mcpServers": {}}' }) });
      const enable = await fetch(`/api/bots/${bot}/mcp-servers/tools`, { method: 'PATCH', headers, body: JSON.stringify({ enabled: true }) });
      return [add.status, read.status, save.status, enable.status];
    }, { bot });
    expect(results).toEqual([403, 403, 403, 403]);
    const state = JSON.stringify(await snapshot(page));
    expect(state).not.toContain(SECRET);
    expect(state).not.toContain('server.js');
  } finally {
    await loopback.close();
    await lan.close();
    await app.stop();
  }
});
