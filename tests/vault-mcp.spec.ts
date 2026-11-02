import { test, expect, type Locator, type Page } from '@playwright/test';
import { createServer, type IncomingMessage, type ServerResponse } from 'node:http';
import type { AddressInfo } from 'node:net';
import { networkInterfaces } from 'node:os';
import type { AppState } from '../shared/types';
import type { McpConfigDocument, SecretInput } from '../shared/vault';
import { startTestApp } from './app-fixture';
import { startMockProvider, type MockCompletion, type MockProviderRequest } from './mock-provider';

// Vault secrets and custom MCP servers are implemented by the Rust server only.
test.skip(process.env.GROK_BOT_BACKEND !== 'rust', 'Vault and custom MCP servers run against the Rust server.');

const SECRET = 'e2e-vault-secret-value-31337';
const snapshot = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());
const details = (page: Page) => page.getByRole('complementary', { name: '대화 세부 정보', exact: true });

/** A real streamable-HTTP MCP peer; optional held discovery exposes command-queue blocking. */
async function startMcpFixture(options: { holdToolsList?: boolean } = {}) {
  const authorization: string[] = [];
  const methods: string[] = [];
  const calls: unknown[] = [];
  let releaseToolsList!: () => void;
  const toolsListGate = new Promise<void>(resolve => { releaseToolsList = resolve; });
  const server = createServer(async (req: IncomingMessage, res: ServerResponse) => {
    if (req.method !== 'POST') { res.writeHead(405).end(); return; }
    authorization.push(String(req.headers.authorization ?? ''));
    let body = '';
    for await (const part of req) body += part.toString();
    const request = JSON.parse(body) as { id?: number | string; method: string; params?: Record<string, unknown> };
    methods.push(request.method);
    if (request.id === undefined) { res.writeHead(202).end(); return; }
    if (request.method === 'tools/list' && options.holdToolsList) {
      await toolsListGate;
      if (res.destroyed) return;
    }
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
  return { url, authorization, methods, calls, releaseToolsList, close: () => new Promise<void>(resolve => {
    releaseToolsList();
    server.closeAllConnections();
    server.close(() => resolve());
  }) };
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

type McpJson = { mcpServers: Record<string, { command?: string; args?: string[]; env?: Record<string, string>; url?: string; headers?: Record<string, string> }> };
type OAuthUiStatus = { status: 'not_connected' | 'awaiting_login' | 'connected' | 'login_required' | 'error' | 'unavailable'; canLogin: boolean };

async function setupOAuthUi(page: Page, url: string) {
  await page.goto(url);
  const botId = (await snapshot(page)).bots[0].id;
  const path = `/api/bots/${botId}/mcp-servers`;
  await vaultRequest(page, path, 'PUT', { json: JSON.stringify({ mcpServers: { remote: { url: 'https://example.invalid/mcp' } } }) });
  await page.getByRole('button', { name: '봇 편집', exact: true }).first().click();
  const row = details(page).getByRole('listitem', { name: 'MCP 서버 remote', exact: true });
  await expect(row).toBeVisible();
  return { path, row, authState: row.getByRole('status', { name: 'remote 인증 상태', exact: true }) };
}

/** Real Rust store requests, including setup and concurrent edits; only failure responses are routed. */
async function vaultRequest<T>(page: Page, path: string, method = 'GET', body?: unknown, expectedStatus = 200): Promise<T> {
  const response = await page.evaluate(async ({ path, method, body }) => {
    const result = await fetch(path, {
      method,
      headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    return { status: result.status, text: await result.text() };
  }, { path, method, body });
  expect(response.status, response.text).toBe(expectedStatus);
  return JSON.parse(response.text) as T;
}

async function readMcp(page: Page, botId: string) {
  const document = await vaultRequest<McpConfigDocument>(page, `/api/bots/${botId}/mcp-servers`);
  return { ...document, parsed: JSON.parse(document.json) as McpJson };
}

async function openMcpForm(page: Page) {
  await details(page).getByRole('button', { name: '사용자 지정 MCP 추가', exact: true }).click();
  const form = page.getByRole('dialog', { name: 'MCP 서버 추가', exact: true });
  await expect(form).toBeVisible();
  return form;
}

async function openMcpJson(page: Page) {
  await details(page).getByRole('button', { name: 'MCP 서버 JSON', exact: true }).click();
  const editor = page.getByRole('dialog', { name: 'MCP 서버 JSON', exact: true });
  await expect(editor.getByLabel('MCP 서버 JSON', { exact: true })).toBeEnabled();
  return editor;
}

function watchVaultWrites(page: Page) {
  const secretPosts: SecretInput[] = [];
  const configPuts: Array<{ json: string; secrets?: SecretInput[] }> = [];
  const probes: Array<{ json: string; secrets: SecretInput[] }> = [];
  page.on('request', request => {
    const path = new URL(request.url()).pathname;
    if (request.method() === 'POST' && path === '/api/secrets') secretPosts.push(request.postDataJSON() as SecretInput);
    if (request.method() === 'PUT' && /\/mcp-servers$/.test(path)) configPuts.push(request.postDataJSON() as { json: string; secrets?: SecretInput[] });
    if (request.method() === 'POST' && /\/mcp-servers\/test$/.test(path)) probes.push(request.postDataJSON() as { json: string; secrets: SecretInput[] });
  });
  return { secretPosts, configPuts, probes };
}

/** Navigate naturally, rather than making an otherwise unreachable control focused with DOM APIs. */
async function tabTo(page: Page, target: Locator) {
  for (let step = 0; step < 60; step++) {
    if (await target.evaluate(element => element === document.activeElement)) return;
    await page.keyboard.press('Tab');
  }
  await expect(target).toBeFocused();
}

async function expectNoFormOverflow(page: Page, form: Locator) {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  expect(await form.evaluate(element => {
    const bounds = element.getBoundingClientRect();
    return bounds.left >= -1 && bounds.right <= window.innerWidth + 1 && element.scrollWidth <= element.clientWidth && [...element.querySelectorAll('input, select, button')].every(control => {
      const box = control.getBoundingClientRect();
      return box.left >= bounds.left - 1 && box.right <= bounds.right + 1;
    });
  })).toBe(true);
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
    await details(page).getByRole('button', { name: 'MCP 서버 JSON', exact: true }).click();
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

test('form: HTTP probe is ephemeral, save uses vault references, JSON and form append preserve enabled servers, and chat approves one credentialed call', async ({ page }, info) => {
  const app = await startTestApp();
  const provider = await startMockProvider({ reply });
  const fixture = await startMcpFixture();
  const writes = watchVaultWrites(page);
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  try {
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto(app.url);
    await configure(page, provider.baseUrl);
    const botId = (await snapshot(page)).bots[0].id;
    await page.getByRole('button', { name: '봇 편집', exact: true }).first().click();
    const form = await openMcpForm(page);
    await expect(form.getByRole('note')).toContainText('새 서버는 꺼진 상태로 저장돼요');
    await form.getByLabel('종류', { exact: true }).selectOption('http');
    await form.getByLabel('서버 이름', { exact: true }).fill('fixture');
    await form.getByLabel('서버 URL', { exact: true }).fill(fixture.url);
    await form.getByRole('button', { name: '헤더 추가', exact: true }).click();
    const header = form.getByRole('group', { name: '헤더 1', exact: true });
    await header.getByLabel('헤더 이름', { exact: true }).fill('Authorization');
    await expect(header.getByLabel('값 선택', { exact: true })).toHaveValue('@new');
    await expect(header.getByLabel('새 시크릿 이름', { exact: true })).toHaveValue('MCP_FIXTURE_TOKEN');
    await header.getByLabel('새 시크릿 이름', { exact: true }).fill('mcp_fixture_token');
    await expect(header.getByLabel('새 시크릿 이름', { exact: true })).toHaveValue('MCP_FIXTURE_TOKEN');
    await header.getByLabel('새 시크릿 값', { exact: true }).fill(SECRET);
    await expect(header.getByLabel('새 시크릿 값', { exact: true })).toHaveAttribute('type', 'password');
    await header.getByLabel('접두사 (선택)', { exact: true }).fill('Bearer ');

    const probeResponse = page.waitForResponse(response => response.request().method() === 'POST' && response.url().endsWith(`/api/bots/${botId}/mcp-servers/test`));
    await form.getByRole('button', { name: '연결 테스트', exact: true }).click();
    const tested = await probeResponse;
    expect(tested.status()).toBe(200);
    expect(await tested.json()).toEqual({ toolCount: 1 });
    await expect(form.getByRole('status')).toHaveText('연결됐어요. 도구 1개를 확인했어요. 서버는 아직 추가되지 않았어요.');
    expect(writes.probes).toHaveLength(1);
    expect(writes.probes[0].secrets).toEqual([{ name: 'MCP_FIXTURE_TOKEN', value: SECRET, botId }]);
    expect(JSON.parse(writes.probes[0].json)).toEqual({ mcpServers: { fixture: { url: fixture.url, headers: { Authorization: 'Bearer ${secret:MCP_FIXTURE_TOKEN}' } } } });
    expect(writes.probes[0].json).not.toContain(SECRET);
    // The fixture is initialize-era: modern HTTP discovery may fall back, but never call a tool.
    expect(fixture.methods.filter(method => !method.startsWith('notifications/'))).toEqual(['server/discover', 'initialize', 'tools/list']);
    expect(fixture.calls).toEqual([]);
    expect(fixture.authorization.every(value => value === `Bearer ${SECRET}`)).toBe(true);
    expect(writes.secretPosts).toEqual([]);
    expect(writes.configPuts).toEqual([]);
    expect((await snapshot(page)).secrets ?? []).toEqual([]);
    expect((await snapshot(page)).mcpServers ?? []).toEqual([]);
    expect((await readMcp(page, botId)).parsed).toEqual({ mcpServers: {} });
    expect(provider.requests).toEqual([]);
    await page.screenshot({ path: info.outputPath('mcp-form-probed-not-saved.png') });

    // Testing is not enabling. One atomic PUT saves the bot-scoped secret and disabled server.
    await form.getByRole('button', { name: '서버 추가', exact: true }).click();
    await expect(form).toBeHidden();
    expect(writes.secretPosts).toEqual([]);
    expect(writes.configPuts).toHaveLength(1);
    expect(writes.configPuts[0].secrets).toEqual([{ name: 'MCP_FIXTURE_TOKEN', value: SECRET, botId }]);
    expect(writes.configPuts[0].json).not.toContain(SECRET);
    const server = details(page).getByRole('listitem', { name: 'MCP 서버 fixture', exact: true });
    const toggle = server.getByRole('switch', { name: 'fixture 사용', exact: true });
    await expect(server).toContainText('HTTP');
    await expect(toggle).not.toBeChecked();
    const saved = await readMcp(page, botId);
    expect(saved.parsed).toEqual({ mcpServers: { fixture: { url: fixture.url, headers: { Authorization: 'Bearer ${secret:MCP_FIXTURE_TOKEN}' } } } });
    expect(saved.json).not.toContain(SECRET);
    const savedState = await snapshot(page);
    expect(savedState.secrets).toEqual([expect.objectContaining({ name: 'MCP_FIXTURE_TOKEN', botId })]);
    expect(savedState.mcpServers).toEqual([expect.objectContaining({ name: 'fixture', enabled: false, secretRefs: ['MCP_FIXTURE_TOKEN'] })]);
    expect(JSON.stringify(savedState)).not.toContain(SECRET);
    expect(JSON.stringify(savedState)).not.toContain(fixture.url);
    await toggle.click();
    await expect(toggle).toBeChecked();
    const fixtureId = (await snapshot(page)).mcpServers!.find(item => item.name === 'fixture')!.id;

    // The advanced editor reads the exact form document. Appending there keeps the enabled row.
    const editor = await openMcpJson(page);
    const json = editor.getByLabel('MCP 서버 JSON', { exact: true });
    expect(JSON.parse(await json.inputValue())).toEqual(saved.parsed);
    await json.fill(JSON.stringify({ mcpServers: { ...saved.parsed.mcpServers, 'from-json': { command: 'node', args: ['--version'] } } }, null, 2));
    await editor.getByRole('button', { name: '저장', exact: true }).click();
    await expect(editor).toBeHidden();
    await expect(toggle).toBeChecked();
    await expect(details(page).getByRole('switch', { name: 'from-json 사용', exact: true })).not.toBeChecked();

    // Conversely, the form appends to JSON-created rows and can select an existing vault value.
    const selected = await openMcpForm(page);
    await selected.getByLabel('종류', { exact: true }).selectOption('http');
    await selected.getByLabel('서버 이름', { exact: true }).fill('from-vault');
    await selected.getByLabel('서버 URL', { exact: true }).fill(fixture.url);
    await selected.getByRole('button', { name: '헤더 추가', exact: true }).click();
    const reused = selected.getByRole('group', { name: '헤더 1', exact: true });
    await reused.getByLabel('헤더 이름', { exact: true }).fill('Authorization');
    await expect(reused.getByLabel('값 선택', { exact: true })).toHaveValue('');
    await reused.getByLabel('값 선택', { exact: true }).selectOption('MCP_FIXTURE_TOKEN');
    await expect(reused.getByLabel('새 시크릿 값', { exact: true })).toHaveCount(0);
    await reused.getByLabel('접두사 (선택)', { exact: true }).fill('Token  ');
    const selectedStart = fixture.authorization.length;
    await selected.getByRole('button', { name: '연결 테스트', exact: true }).click();
    await expect(selected.getByRole('status')).toContainText('도구 1개를 확인했어요');
    expect(writes.probes[1].secrets).toEqual([]);
    expect(fixture.authorization.slice(selectedStart).length).toBeGreaterThan(0);
    expect(fixture.authorization.slice(selectedStart).every(value => value === `Token  ${SECRET}`)).toBe(true);
    expect(fixture.calls).toHaveLength(0);
    expect((await snapshot(page)).mcpServers?.some(item => item.name === 'from-vault')).toBe(false);
    await selected.getByRole('button', { name: '서버 추가', exact: true }).click();
    await expect(selected).toBeHidden();
    expect(writes.secretPosts).toEqual([]);
    expect(writes.configPuts).toHaveLength(3);
    expect(writes.configPuts.at(-1)?.secrets ?? []).toEqual([]);
    const merged = await readMcp(page, botId);
    expect(merged.parsed).toEqual({ mcpServers: {
      fixture: saved.parsed.mcpServers.fixture,
      'from-json': { command: 'node', args: ['--version'] },
      'from-vault': { url: fixture.url, headers: { Authorization: 'Token  ${secret:MCP_FIXTURE_TOKEN}' } },
    } });
    expect((await snapshot(page)).mcpServers).toEqual(expect.arrayContaining([
      expect.objectContaining({ id: fixtureId, name: 'fixture', enabled: true }),
      expect.objectContaining({ name: 'from-json', enabled: false }),
      expect.objectContaining({ name: 'from-vault', enabled: false, secretRefs: ['MCP_FIXTURE_TOKEN'] }),
    ]));
    const reflected = await openMcpJson(page);
    expect(JSON.parse(await reflected.getByLabel('MCP 서버 JSON', { exact: true }).inputValue())).toEqual(merged.parsed);
    expect(await reflected.getByLabel('MCP 서버 JSON', { exact: true }).inputValue()).not.toContain(SECRET);
    await reflected.getByRole('button', { name: '취소', exact: true }).click();

    const chatStart = fixture.authorization.length;
    await page.getByRole('textbox', { name: '메시지', exact: true }).fill('Please look it up');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    const approval = page.getByRole('region', { name: '작업 승인 요청', exact: true });
    await expect(approval).toContainText('MCP 서버 fixture: lookup', { timeout: 30_000 });
    expect(fixture.calls).toHaveLength(0);
    await approval.getByRole('button', { name: 'Run once', exact: true }).click();
    await expect(page.getByText('사용자 지정 MCP 조회를 마쳤습니다.', { exact: true })).toBeVisible({ timeout: 30_000 });
    expect(fixture.calls).toHaveLength(1);
    expect(fixture.calls[0]).toEqual(expect.objectContaining({ name: 'lookup', arguments: { query: 'e2e' } }));
    expect(fixture.authorization.slice(chatStart).length).toBeGreaterThan(0);
    expect(fixture.authorization.slice(chatStart).every(value => value === `Bearer ${SECRET}`)).toBe(true);
    expect(JSON.stringify(provider.requests)).not.toContain(SECRET);
    expect(JSON.stringify(await snapshot(page))).not.toContain(SECRET);
    expect((await readMcp(page, botId)).json).not.toContain(SECRET);
    expect(errors).toEqual([]);
  } finally {
    await app.stop();
    await provider.close();
    await fixture.close();
  }
});

test('responsiveness: a pending HTTP probe does not delay browser message send or tool approval beyond two seconds', async ({ page }) => {
  const app = await startTestApp();
  const provider = await startMockProvider({ reply });
  const fixture = await startMcpFixture();
  const stalled = await startMcpFixture({ holdToolsList: true });
  const probePage = await page.context().newPage();
  let probeSettled = false;
  let probeResult: Promise<{ status: number; body: unknown }> | undefined;
  try {
    await page.goto(app.url);
    await configure(page, provider.baseUrl);
    const botId = (await snapshot(page)).bots[0].id;
    const path = `/api/bots/${botId}/mcp-servers`;
    await vaultRequest(page, path, 'PUT', { json: JSON.stringify({ mcpServers: { fixture: { url: fixture.url } } }) });
    await vaultRequest(page, `${path}/fixture`, 'PATCH', { enabled: true });

    // Warm the engine and create the conversation outside the latency assertion.
    const message = page.getByRole('textbox', { name: '메시지', exact: true });
    const send = page.getByRole('button', { name: '메시지 보내기', exact: true });
    await message.fill('Warm up the responsiveness fixture');
    await send.click();
    await expect(page.getByText('보관함 테스트 응답입니다.', { exact: true })).toBeVisible({ timeout: 30_000 });
    await message.fill('Please look it up');
    await expect(send).toBeEnabled();

    // A second browser page makes a real fetch; its peer deliberately holds tools/list open.
    await probePage.goto(app.url);
    probeResult = probePage.evaluate(async ({ path, url }) => {
      const response = await fetch(`${path}/test`, {
        method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' },
        body: JSON.stringify({ json: JSON.stringify({ mcpServers: { pending: { url } } }), secrets: [] }),
      });
      return { status: response.status, body: await response.json() as unknown };
    }, { path, url: stalled.url }).then(result => {
      probeSettled = true;
      return result;
    }, error => {
      probeSettled = true;
      return { status: 0, body: { error: String(error) } };
    });
    await expect.poll(() => stalled.methods, { timeout: 5_000 }).toContain('tools/list');
    expect(probeSettled).toBe(false);

    await message.fill('Please look it up');
    const sentAt = performance.now();
    const [sent] = await Promise.all([
      page.waitForResponse(response => response.request().method() === 'POST' && /\/api\/conversations\/[^/]+\/messages$/.test(new URL(response.url()).pathname), { timeout: 2_000 }),
      send.click(),
    ]);
    expect(sent.status()).toBe(202);
    expect(performance.now() - sentAt).toBeLessThan(2_000);
    expect(probeSettled).toBe(false);
    const approval = page.getByRole('region', { name: '작업 승인 요청', exact: true });
    await expect(approval).toContainText('MCP 서버 fixture: lookup', { timeout: 4_000 });
    expect(fixture.calls).toHaveLength(0);
    expect(probeSettled).toBe(false);

    const approvedAt = performance.now();
    const [approved] = await Promise.all([
      page.waitForResponse(response => response.request().method() === 'POST' && /\/api\/conversations\/[^/]+\/permissions\/[^/]+$/.test(new URL(response.url()).pathname), { timeout: 2_000 }),
      approval.getByRole('button', { name: 'Run once', exact: true }).click(),
    ]);
    expect(approved.status()).toBe(200);
    expect(performance.now() - approvedAt).toBeLessThan(2_000);
    expect(probeSettled).toBe(false);
    expect(stalled.calls).toHaveLength(0);

    stalled.releaseToolsList();
    expect(await probeResult).toEqual({ status: 200, body: { toolCount: 1 } });
    await expect(page.getByText('사용자 지정 MCP 조회를 마쳤습니다.', { exact: true })).toBeVisible({ timeout: 30_000 });
    expect(fixture.calls).toHaveLength(1);
    expect((await snapshot(page)).mcpServers?.map(server => server.name)).toEqual(['fixture']);
    expect((await snapshot(page)).secrets ?? []).toEqual([]);
  } finally {
    stalled.releaseToolsList();
    if (probeResult) await probeResult;
    await probePage.close();
    await app.stop();
    await provider.close();
    await fixture.close();
    await stalled.close();
  }
});

test('form retry: a failed atomic MCP PUT saves no secrets, retains the password draft, and retries once alongside enabled and externally saved servers', async ({ page }) => {
  const app = await startTestApp();
  const writes = watchVaultWrites(page);
  try {
    await page.goto(app.url);
    const botId = (await snapshot(page)).bots[0].id;
    const path = `/api/bots/${botId}/mcp-servers`;
    const baseline = { command: 'node', args: ['--version'] };
    await vaultRequest(page, path, 'PUT', { json: JSON.stringify({ mcpServers: { baseline } }) });
    await vaultRequest(page, `${path}/baseline`, 'PATCH', { enabled: true });
    await page.getByRole('button', { name: '봇 편집', exact: true }).first().click();
    await expect(details(page).getByRole('switch', { name: 'baseline 사용', exact: true })).toBeChecked();
    const baselineId = (await snapshot(page)).mcpServers!.find(server => server.name === 'baseline')!.id;
    const form = await openMcpForm(page);
    await form.getByLabel('종류', { exact: true }).selectOption('http');
    await form.getByLabel('서버 이름', { exact: true }).fill('retry');
    await form.getByLabel('서버 URL', { exact: true }).fill('https://example.invalid/mcp');
    await form.getByRole('button', { name: '헤더 추가', exact: true }).click();
    const header = form.getByRole('group', { name: '헤더 1', exact: true });
    await header.getByLabel('헤더 이름', { exact: true }).fill('Authorization');
    await header.getByLabel('새 시크릿 이름', { exact: true }).fill('RETRY_TOKEN');
    await header.getByLabel('새 시크릿 값', { exact: true }).fill(SECRET);
    await header.getByLabel('접두사 (선택)', { exact: true }).fill('Bearer ');

    // Intercept the failure only: no secret POST is allowed; the successful retry uses Rust storage.
    const putsBefore = writes.configPuts.length;
    const secret = { name: 'RETRY_TOKEN', value: SECRET, botId };
    let failures = 0;
    await page.route(`**${path}`, async route => {
      if (route.request().method() === 'PUT' && failures === 0) {
        failures++;
        await route.fulfill({ status: 500, json: { error: 'Injected MCP save failure' } });
      } else await route.continue();
    });
    await form.getByRole('button', { name: '서버 추가', exact: true }).click();
    await expect(form.getByRole('alert')).toContainText('서버가 추가되지 않았어요');
    await expect(form.getByRole('alert')).toContainText('Injected MCP save failure');
    await expect(form.getByRole('alert')).not.toContainText(SECRET);
    await expect(form.getByRole('status')).toHaveCount(0);
    await expect(header.getByLabel('값 선택', { exact: true })).toHaveValue('@new');
    await expect(header.getByLabel('새 시크릿 이름', { exact: true })).toHaveValue('RETRY_TOKEN');
    await expect(header.getByLabel('새 시크릿 값', { exact: true })).toHaveValue(SECRET);
    await expect(header.getByLabel('새 시크릿 값', { exact: true })).toHaveAttribute('type', 'password');
    await expect(form.getByLabel('서버 이름', { exact: true })).toHaveValue('retry');
    expect(failures).toBe(1);
    expect(writes.secretPosts).toEqual([]);
    expect(writes.configPuts).toHaveLength(putsBefore + 1);
    expect(writes.configPuts.at(-1)?.secrets).toEqual([secret]);
    expect(writes.configPuts.at(-1)?.json).not.toContain(SECRET);
    expect((await snapshot(page)).secrets ?? []).toEqual([]);
    expect((await snapshot(page)).mcpServers).toEqual([expect.objectContaining({ id: baselineId, name: 'baseline', enabled: true })]);
    expect((await readMcp(page, botId)).parsed).toEqual({ mcpServers: { baseline } });

    // A different client saves while this dirty dialog is still open. Retry must re-read, not replace.
    const external = { url: 'https://external.invalid/mcp', headers: { 'X-External': 'non-secret-marker' } };
    const latest = await readMcp(page, botId);
    await vaultRequest(page, path, 'PUT', { json: JSON.stringify({ mcpServers: { ...latest.parsed.mcpServers, external } }) });
    await vaultRequest(page, `${path}/external`, 'PATCH', { enabled: true });
    const externalId = (await snapshot(page)).mcpServers!.find(server => server.name === 'external')!.id;
    await expect(form).toBeVisible();
    await expect(form.getByLabel('서버 URL', { exact: true })).toHaveValue('https://example.invalid/mcp');
    await expect(header.getByLabel('새 시크릿 값', { exact: true })).toHaveValue(SECRET);
    expect((await snapshot(page)).mcpServers).toEqual(expect.arrayContaining([expect.objectContaining({ id: baselineId, name: 'baseline', enabled: true })]));
    await form.getByRole('button', { name: '서버 추가', exact: true }).click();
    await expect(form).toBeHidden();
    expect(writes.secretPosts).toEqual([]);
    expect(writes.probes).toHaveLength(0);
    expect(writes.configPuts).toHaveLength(putsBefore + 3);
    expect(writes.configPuts.filter(input => (input.secrets?.length ?? 0) > 0).map(input => input.secrets)).toEqual([[secret], [secret]]);
    const merged = await readMcp(page, botId);
    expect(merged.parsed).toEqual({ mcpServers: {
      baseline,
      external,
      retry: { url: 'https://example.invalid/mcp', headers: { Authorization: 'Bearer ${secret:RETRY_TOKEN}' } },
    } });
    expect((await snapshot(page)).mcpServers).toEqual(expect.arrayContaining([
      expect.objectContaining({ id: baselineId, name: 'baseline', enabled: true }),
      expect.objectContaining({ id: externalId, name: 'external', enabled: true }),
      expect.objectContaining({ name: 'retry', enabled: false, secretRefs: ['RETRY_TOKEN'] }),
    ]));
    expect((await snapshot(page)).secrets).toEqual([expect.objectContaining({ name: 'RETRY_TOKEN', botId })]);
    expect(merged.json).not.toContain(SECRET);
    expect(JSON.stringify(await snapshot(page))).not.toContain(SECRET);
    const editor = await openMcpJson(page);
    expect(JSON.parse(await editor.getByLabel('MCP 서버 JSON', { exact: true }).inputValue())).toEqual(merged.parsed);
    await editor.getByRole('button', { name: '취소', exact: true }).click();
  } finally { await app.stop(); }
});

test('form validation: local commands, reserved or duplicate names, URLs, credentials, and case-insensitive headers fail inline before any request', async ({ page }) => {
  const app = await startTestApp();
  try {
    await page.goto(app.url);
    const botId = (await snapshot(page)).bots[0].id;
    const path = `/api/bots/${botId}/mcp-servers`;
    const taken = { url: 'https://existing.invalid/mcp' };
    await vaultRequest(page, '/api/secrets', 'POST', { name: 'VALIDATION_TOKEN', value: SECRET }, 201);
    await vaultRequest(page, path, 'PUT', { json: JSON.stringify({ mcpServers: { taken } }) });
    await vaultRequest(page, `${path}/taken`, 'PATCH', { enabled: true });
    const writes = watchVaultWrites(page);
    await page.getByRole('button', { name: '봇 편집', exact: true }).first().click();
    await expect(details(page).getByRole('switch', { name: 'taken 사용', exact: true })).toBeChecked();
    await expect(details(page).getByRole('list', { name: '시크릿 목록', exact: true })).toContainText('VALIDATION_TOKEN');
    const form = await openMcpForm(page);
    const testConnection = form.getByRole('button', { name: '연결 테스트', exact: true });
    await form.getByRole('button', { name: '서버 추가', exact: true }).click();
    await expect(form.getByRole('alert')).toContainText('입력 내용을 확인해 주세요');
    await expect(form.getByLabel('서버 이름', { exact: true })).toHaveAttribute('aria-invalid', 'true');
    await expect(form.getByLabel('명령', { exact: true })).toHaveAttribute('aria-invalid', 'true');
    await form.getByLabel('명령', { exact: true }).fill('node');
    for (const name of ['workspace-new', 'bad__name', 'Uppercase', 'taken']) {
      await form.getByLabel('서버 이름', { exact: true }).fill(name);
      await testConnection.click();
      await expect(form.getByLabel('서버 이름', { exact: true })).toHaveAttribute('aria-invalid', 'true');
    }
    await expect(form.getByText('같은 이름의 서버가 있어요. 다른 이름을 입력하세요.', { exact: true })).toBeVisible();
    await form.getByLabel('서버 이름', { exact: true }).fill('local-form');
    for (const command of ['./server.js', 'node ${secret:VALIDATION_TOKEN}']) {
      await form.getByLabel('명령', { exact: true }).fill(command);
      await testConnection.click();
      await expect(form.getByLabel('명령', { exact: true })).toHaveAttribute('aria-invalid', 'true');
    }
    await form.getByLabel('명령', { exact: true }).fill('node');
    await form.getByRole('button', { name: '인자 추가', exact: true }).click();
    await form.getByLabel('인자 1', { exact: true }).fill('${secret:VALIDATION_TOKEN}');
    await testConnection.click();
    await expect(form.getByLabel('인자 1', { exact: true })).toHaveAttribute('aria-invalid', 'true');
    await form.getByLabel('인자 1', { exact: true }).fill('--label=two words');
    await form.getByRole('button', { name: '인자 추가', exact: true }).click();
    await form.getByLabel('인자 2', { exact: true }).fill('file "quoted name".js');
    await form.getByRole('button', { name: '환경 변수 추가', exact: true }).click();
    const env = form.getByRole('group', { name: '환경 변수 1', exact: true });
    await env.getByLabel('환경 변수 이름', { exact: true }).fill('1TOKEN');
    await testConnection.click();
    await expect(env.getByText('영문·_로 시작하는 환경 변수 이름을 입력하세요.', { exact: true })).toBeVisible();
    await env.getByLabel('환경 변수 이름', { exact: true }).fill('TOKEN');
    await testConnection.click();
    await expect(env.getByText('보관함 시크릿을 선택하세요.', { exact: true })).toBeVisible();
    await env.getByLabel('값 선택', { exact: true }).selectOption('VALIDATION_TOKEN');

    // The transport switch retains the local draft while validating only the visible transport.
    await form.getByLabel('종류', { exact: true }).selectOption('http');
    for (const url of ['ftp://example.invalid/mcp', 'https://user:pass@example.invalid/mcp', 'https://example.invalid/mcp#fragment']) {
      await form.getByLabel('서버 URL', { exact: true }).fill(url);
      await testConnection.click();
      await expect(form.getByLabel('서버 URL', { exact: true })).toHaveAttribute('aria-invalid', 'true');
    }
    await form.getByLabel('서버 URL', { exact: true }).fill('https://example.invalid/mcp');
    await form.getByRole('button', { name: '헤더 추가', exact: true }).click();
    const firstHeader = form.getByRole('group', { name: '헤더 1', exact: true });
    await firstHeader.getByLabel('헤더 이름', { exact: true }).fill('Authorization');
    await firstHeader.getByLabel('값 선택', { exact: true }).selectOption('VALIDATION_TOKEN');
    await form.getByRole('button', { name: '헤더 추가', exact: true }).click();
    const secondHeader = form.getByRole('group', { name: '헤더 2', exact: true });
    await secondHeader.getByLabel('헤더 이름', { exact: true }).fill('authorization');
    await secondHeader.getByLabel('값 선택', { exact: true }).selectOption('VALIDATION_TOKEN');
    await testConnection.click();
    await expect(secondHeader.getByText('같은 이름의 항목이 있어요. HTTP 헤더는 대소문자를 구분하지 않아요.', { exact: true })).toBeVisible();
    await secondHeader.getByLabel('헤더 이름', { exact: true }).fill('invalid header');
    await testConnection.click();
    await expect(secondHeader.getByText('올바른 HTTP 헤더 이름을 입력하세요.', { exact: true })).toBeVisible();
    await secondHeader.getByLabel('헤더 이름', { exact: true }).fill('X-Extra');
    await secondHeader.getByLabel('값 선택', { exact: true }).selectOption('@new');
    await secondHeader.getByLabel('새 시크릿 이름', { exact: true }).fill('VALIDATION_TOKEN');
    await secondHeader.getByLabel('새 시크릿 값', { exact: true }).fill(SECRET);
    await testConnection.click();
    await expect(secondHeader.getByText('같은 이름의 시크릿이 있어요. 보관함에서 선택하거나 새 이름을 입력하세요.', { exact: true })).toBeVisible();
    await secondHeader.getByLabel('새 시크릿 이름', { exact: true }).fill('SECOND_TOKEN');
    await secondHeader.getByLabel('새 시크릿 값', { exact: true }).fill('short');
    await testConnection.click();
    await expect(secondHeader.getByText('시크릿 값은 줄바꿈·제어 문자 없이 8~8,192자로 입력하세요.', { exact: true })).toBeVisible();
    await secondHeader.getByLabel('새 시크릿 값', { exact: true }).fill(SECRET);
    await secondHeader.getByLabel('접두사 (선택)', { exact: true }).fill('Bearer ${secret:VALIDATION_TOKEN}');
    await testConnection.click();
    await expect(secondHeader.getByText('접두사는 시크릿 참조·제어 문자 없이 입력하세요. 전체 헤더는 8,192자 이하여야 해요.', { exact: true })).toBeVisible();
    await secondHeader.getByRole('button', { name: '헤더 2 삭제', exact: true }).click();

    await form.getByLabel('종류', { exact: true }).selectOption('stdio');
    await expect(form.getByLabel('명령', { exact: true })).toHaveValue('node');
    await expect(form.getByLabel('인자 1', { exact: true })).toHaveValue('--label=two words');
    await expect(form.getByLabel('인자 2', { exact: true })).toHaveValue('file "quoted name".js');
    const confirmation = page.waitForEvent('dialog');
    const click = testConnection.click();
    const confirm = await confirmation;
    expect(confirm.type()).toBe('confirm');
    expect(confirm.message()).toContain('서버 컴퓨터에서 입력한 명령을 실제로 실행');
    await confirm.dismiss();
    await click;
    expect(writes.probes).toEqual([]);
    expect(writes.secretPosts).toEqual([]);
    expect(writes.configPuts).toEqual([]);

    await form.getByRole('button', { name: '서버 추가', exact: true }).click();
    await expect(form).toBeHidden();
    expect(writes.secretPosts).toEqual([]);
    expect((await readMcp(page, botId)).parsed).toEqual({ mcpServers: {
      taken,
      'local-form': { command: 'node', args: ['--label=two words', 'file "quoted name".js'], env: { TOKEN: '${secret:VALIDATION_TOKEN}' } },
    } });
    await expect(details(page).getByRole('switch', { name: 'taken 사용', exact: true })).toBeChecked();
    await expect(details(page).getByRole('switch', { name: 'local-form 사용', exact: true })).not.toBeChecked();
    expect(JSON.stringify(await snapshot(page))).not.toContain(SECRET);
  } finally { await app.stop(); }
});

test('form blank arguments: empty and whitespace rows block Test and Add, focus only the invalid row, and explain how to fix it', async ({ page }) => {
  const app = await startTestApp();
  const writes = watchVaultWrites(page);
  try {
    await page.goto(app.url);
    const botId = (await snapshot(page)).bots[0].id;
    await page.getByRole('button', { name: '봇 편집', exact: true }).first().click();
    const form = await openMcpForm(page);
    await form.getByLabel('서버 이름', { exact: true }).fill('blank-args');
    await form.getByLabel('명령', { exact: true }).fill('node');
    await form.getByRole('button', { name: '인자 추가', exact: true }).click();
    const first = form.getByLabel('인자 1', { exact: true });
    await first.fill('--version');
    await form.getByRole('button', { name: '인자 추가', exact: true }).click();
    const blank = form.getByLabel('인자 2', { exact: true });
    for (const value of ['', '   ']) {
      for (const action of ['연결 테스트', '서버 추가']) {
        await blank.fill(value);
        await form.getByRole('button', { name: action, exact: true }).click();
        await expect(form).toBeVisible();
        await expect(blank).toHaveAttribute('aria-invalid', 'true');
        await expect(first).toHaveAttribute('aria-invalid', 'false');
        await expect(blank).toBeFocused();
        await expect(blank).toHaveAccessibleDescription('빈 인자 칸은 삭제하거나 값을 입력해 주세요.');
        await expect(form.getByText('빈 인자 칸은 삭제하거나 값을 입력해 주세요.', { exact: true })).toBeVisible();
        await expect(form.getByRole('alert')).not.toContainText(SECRET);
        expect(writes.probes).toEqual([]);
        expect(writes.configPuts).toEqual([]);
        expect(writes.secretPosts).toEqual([]);
      }
    }
    await blank.fill('argument with spaces');
    await expect(blank).toHaveAttribute('aria-invalid', 'false');
    await expect(first).toHaveValue('--version');
    await expect(blank).toHaveValue('argument with spaces');
    expect((await snapshot(page)).secrets ?? []).toEqual([]);
    expect((await readMcp(page, botId)).parsed).toEqual({ mcpServers: {} });
    page.once('dialog', confirm => confirm.accept());
    await form.getByRole('button', { name: '취소', exact: true }).click();
    await expect(form).toBeHidden();
  } finally { await app.stop(); }
});

test('probe field errors: invalid URLs, commands, headers, and blank arguments name the field without echoing submitted secrets or keys', async ({ page }) => {
  const app = await startTestApp();
  try {
    await page.goto(app.url);
    const botId = (await snapshot(page)).bots[0].id;
    const url = `https://private-user:${SECRET}@example.invalid/mcp`;
    const invalidHeader = `${SECRET} invalid header`;
    const cases = [
      { field: '서버 URL(url)', config: { url, headers: { Authorization: `Bearer ${SECRET}` } } },
      { field: '명령(command)', config: { command: `./${SECRET}` } },
      { field: '헤더(headers)', config: { url: 'https://example.invalid/mcp', headers: { [invalidHeader]: SECRET } } },
      { field: '인자(args)', config: { command: 'node', args: ['   ', SECRET] } },
    ];
    for (const { field, config } of cases) {
      const result = await vaultRequest<{ error: string }>(page, `/api/bots/${botId}/mcp-servers/test`, 'POST', {
        json: JSON.stringify({ mcpServers: { draft: config } }),
        secrets: [{ name: 'FIELD_TOKEN', value: SECRET, botId }],
      }, 400);
      expect(result.error).toContain(field);
      expect(result.error).not.toContain('올바른 MCP 서버 JSON을 입력해 주세요');
      expect(JSON.stringify(result)).not.toContain(SECRET);
      expect(JSON.stringify(result)).not.toContain('private-user');
      expect(JSON.stringify(result)).not.toContain(url);
      expect(JSON.stringify(result)).not.toContain(invalidHeader);
    }
    expect((await snapshot(page)).secrets ?? []).toEqual([]);
    expect((await snapshot(page)).mcpServers ?? []).toEqual([]);
    expect((await readMcp(page, botId)).parsed).toEqual({ mcpServers: {} });
  } finally { await app.stop(); }
});

test('mobile light form: every action is reachable at 390x844, keyboard cancel and Escape guard the draft, and discard persists no secrets', async ({ page }, info) => {
  const app = await startTestApp();
  const fixture = await startMcpFixture();
  const writes = watchVaultWrites(page);
  try {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(app.url);
    const botId = (await snapshot(page)).bots[0].id;
    await page.getByRole('button', { name: '봇 목록 열기', exact: true }).click();
    await page.getByRole('button', { name: '설정', exact: true }).click();
    const settings = page.getByRole('dialog', { name: '설정', exact: true });
    await settings.getByLabel('테마', { exact: true }).selectOption('light');
    await expect(page.locator('html')).toHaveAttribute('data-theme', 'light');
    await settings.getByRole('button', { name: '설정 닫기', exact: true }).click();
    await page.getByRole('button', { name: '봇 편집', exact: true }).first().click();
    const opener = details(page).getByRole('button', { name: '사용자 지정 MCP 추가', exact: true });
    const form = await openMcpForm(page);
    await expect(form.getByRole('note')).toBeVisible();
    await expect(form.getByRole('note')).toContainText('로컬 명령은 서버 컴퓨터에서 이 앱과 같은 권한');
    await expect(form.getByLabel('서버 이름', { exact: true })).toBeFocused();
    await page.keyboard.type('mobile');
    await form.getByLabel('종류', { exact: true }).selectOption('http');
    await form.getByLabel('서버 URL', { exact: true }).fill(fixture.url);
    const addHeader = form.getByRole('button', { name: '헤더 추가', exact: true });
    await tabTo(page, addHeader);
    await expect(addHeader).toBeInViewport({ ratio: 1 });
    await page.keyboard.press('Enter');
    const header = form.getByRole('group', { name: '헤더 1', exact: true });
    await header.getByLabel('헤더 이름', { exact: true }).fill('Authorization');
    await expect(header.getByLabel('새 시크릿 이름', { exact: true })).toHaveValue('MCP_MOBILE_TOKEN');
    const password = header.getByLabel('새 시크릿 값', { exact: true });
    await expect(password).toHaveAttribute('type', 'password');
    await header.getByLabel('접두사 (선택)', { exact: true }).fill('Bearer ');
    await tabTo(page, form.getByRole('button', { name: '연결 테스트', exact: true }));
    await page.keyboard.press('Enter');
    await expect(form.getByRole('alert')).toContainText('입력 내용을 확인해 주세요');
    await expect(form.getByRole('alert')).toBeInViewport({ ratio: 1 });
    await expect(password).toHaveAttribute('aria-invalid', 'true');
    await expect(password).toBeFocused();
    await expect(password).toHaveAccessibleDescription('시크릿 값은 줄바꿈·제어 문자 없이 8~8,192자로 입력하세요.');
    expect(writes.probes).toHaveLength(0);
    await password.fill(SECRET);
    for (const field of ['헤더 이름', '값 선택', '새 시크릿 이름', '새 시크릿 값', '접두사 (선택)']) {
      const input = header.getByLabel(field, { exact: true });
      await tabTo(page, input);
      await expect(input).toBeInViewport({ ratio: 1 });
    }
    await expectNoFormOverflow(page, form);

    const testConnection = form.getByRole('button', { name: '연결 테스트', exact: true });
    await tabTo(page, testConnection);
    await expect(testConnection).toBeInViewport({ ratio: 1 });
    await page.keyboard.press('Enter');
    await expect(form.getByRole('status')).toContainText('도구 1개를 확인했어요. 서버는 아직 추가되지 않았어요.');
    await expect(form.getByRole('status')).toBeInViewport({ ratio: 1 });
    expect(fixture.calls).toHaveLength(0);
    expect(fixture.authorization.every(value => value === `Bearer ${SECRET}`)).toBe(true);
    expect(writes.secretPosts).toEqual([]);
    expect(writes.configPuts).toEqual([]);

    // Exercise dynamic-row removal and every footer/header action through the real tab order.
    await tabTo(page, addHeader);
    await page.keyboard.press('Enter');
    const remove = form.getByRole('button', { name: '헤더 2 삭제', exact: true });
    await tabTo(page, remove);
    await expect(remove).toBeInViewport({ ratio: 1 });
    await page.keyboard.press('Enter');
    await expect(form.getByRole('group', { name: '헤더 2', exact: true })).toHaveCount(0);
    for (const label of ['서버 추가', 'MCP 서버 추가 닫기']) {
      const action = form.getByRole('button', { name: label, exact: true });
      await tabTo(page, action);
      await expect(action).toBeInViewport({ ratio: 1 });
    }
    await expectNoFormOverflow(page, form);
    await page.screenshot({ path: info.outputPath('mcp-form-mobile-light-filled.png') });

    const answerDiscard = async (key: string, accept: boolean) => {
      const dialogReady = page.waitForEvent('dialog');
      const pressed = page.keyboard.press(key);
      const confirm = await dialogReady;
      expect(confirm.type()).toBe('confirm');
      expect(confirm.message()).toContain('입력 중인 MCP 서버를 버리고 닫을까요');
      if (accept) await confirm.accept(); else await confirm.dismiss();
      await pressed;
    };
    const cancel = form.getByRole('button', { name: '취소', exact: true });
    await tabTo(page, cancel);
    await expect(cancel).toBeInViewport({ ratio: 1 });
    await answerDiscard('Enter', false);
    await expect(form).toBeVisible();
    await expect(form.getByLabel('서버 이름', { exact: true })).toHaveValue('mobile');
    await expect(header.getByLabel('새 시크릿 값', { exact: true })).toHaveValue(SECRET);
    await answerDiscard('Escape', false);
    await expect(form).toBeVisible();
    await expect(header.getByLabel('새 시크릿 값', { exact: true })).toHaveValue(SECRET);
    await answerDiscard('Escape', true);
    await expect(form).toBeHidden();
    await expect(opener).toBeFocused();
    expect(writes.probes).toHaveLength(1);
    expect(writes.secretPosts).toEqual([]);
    expect(writes.configPuts).toEqual([]);
    expect((await snapshot(page)).secrets ?? []).toEqual([]);
    expect((await snapshot(page)).mcpServers ?? []).toEqual([]);
    expect((await readMcp(page, botId)).parsed).toEqual({ mcpServers: {} });
    const clean = await openMcpForm(page);
    await expect(clean.getByLabel('서버 이름', { exact: true })).toHaveValue('');
    await expect(clean.getByRole('group', { name: '환경 변수 1', exact: true })).toHaveCount(0);
    await page.keyboard.press('Escape');
    await expect(clean).toBeHidden();
  } finally {
    await app.stop();
    await fixture.close();
  }
});

test('OAuth UI: browser login uses an isolated popup, optional metadata stays explicit, and only confirmed status connects without enabling', async ({ page, context }) => {
  const app = await startTestApp();
  const authorizationUrl = 'https://oauth-ui.invalid/authorize?state=ui-only-marker';
  let status: OAuthUiStatus = { status: 'not_connected', canLogin: true };
  const logins: unknown[] = [];
  let logouts = 0;
  await context.route('https://oauth-ui.invalid/**', route => route.fulfill({ contentType: 'text/html', body: '<title>Mock OAuth</title><p>Mock login only</p>' }));
  await page.route(/\/mcp-servers\/remote\/oauth(?:\/(?:login|logout))?$/, async route => {
    const path = new URL(route.request().url()).pathname;
    if (path.endsWith('/login')) {
      logins.push(route.request().postDataJSON()); status = { status: 'awaiting_login', canLogin: true };
      await route.fulfill({ json: { authorizationUrl } });
    } else if (path.endsWith('/logout')) {
      expect(route.request().postDataJSON()).toEqual({}); logouts++;
      status = { status: 'not_connected', canLogin: true };
      await route.fulfill({ json: status });
    } else await route.fulfill({ json: status });
  });
  try {
    const { row, authState } = await setupOAuthUi(page, app.url);
    const login = row.getByRole('button', { name: 'remote 로그인', exact: true });
    await expect(authState).toHaveText('로그인 안 됨');
    await login.click();
    const dialog = page.getByRole('dialog', { name: 'remote 로그인', exact: true });
    await expect(dialog.getByLabel('클라이언트 ID (선택)', { exact: true })).toBeFocused();
    await expect(dialog.getByLabel('클라이언트 ID (선택)', { exact: true })).toHaveValue('');
    await expect(dialog.getByLabel('리소스 식별자 (선택)', { exact: true })).toHaveValue('');
    await expect(dialog.locator('input[type=password]')).toHaveCount(0);
    await page.keyboard.press('Escape');
    await expect(dialog).toBeHidden();
    await expect(login).toBeFocused();
    expect(logins).toEqual([]);

    await login.click();
    const popupReady = page.context().waitForEvent('page');
    await dialog.getByRole('button', { name: '브라우저에서 로그인', exact: true }).click();
    const popup = await popupReady;
    await popup.waitForURL(authorizationUrl);
    expect(await popup.evaluate(() => window.opener === null)).toBe(true);
    await expect(dialog).toBeHidden();
    await expect(authState).toHaveText('로그인 대기 중');
    await expect(row.getByRole('switch', { name: 'remote 사용', exact: true })).not.toBeChecked();
    expect(logins).toEqual([{}]);
    expect(await page.locator('body').innerText()).not.toContain('ui-only-marker');
    expect(JSON.stringify(await snapshot(page))).not.toContain('ui-only-marker');
    expect(await page.evaluate(() => JSON.stringify({ local: { ...localStorage }, session: { ...sessionStorage } }))).not.toContain('ui-only-marker');
    status = { status: 'connected', canLogin: true };
    await expect(authState).toHaveText('연결됨');
    await expect(row.getByRole('button', { name: 'remote 로그아웃', exact: true })).toBeEnabled();
    await expect(row.getByRole('switch', { name: 'remote 사용', exact: true })).not.toBeChecked();
    await popup.close();
    await row.getByRole('button', { name: 'remote 로그아웃', exact: true }).click();
    await expect(authState).toHaveText('로그인 안 됨');
    expect(logouts).toBe(1);

    await page.setViewportSize({ width: 390, height: 844 });
    await login.click();
    await dialog.getByLabel('클라이언트 ID (선택)', { exact: true }).fill('public-client-id');
    await dialog.getByText('리소스 식별자가 필요한 경우', { exact: true }).click();
    await expect(dialog.getByText(/Notion의/)).toContainText('https://mcp.notion.com');
    await dialog.getByLabel('리소스 식별자 (선택)', { exact: true }).fill('https://mcp.notion.com');
    await expectNoFormOverflow(page, dialog);
    const explicitPopupReady = page.context().waitForEvent('page');
    await dialog.getByRole('button', { name: '브라우저에서 로그인', exact: true }).click();
    const explicitPopup = await explicitPopupReady;
    await explicitPopup.waitForURL(authorizationUrl);
    expect(logins[1]).toEqual({ clientId: 'public-client-id', resource: 'https://mcp.notion.com' });
    expect(await explicitPopup.evaluate(() => window.opener === null)).toBe(true);
    status = { status: 'connected', canLogin: true };
    await expect(authState).toHaveText('연결됨');
    await explicitPopup.close();
  } finally { await app.stop(); }
});

test('OAuth UI: safe failures close unused popups, blocked popups do not start login, and unavailable or LAN access disables controls', async ({ page }) => {
  const app = await startTestApp();
  const unsafeDetail = 'https://oauth-ui.invalid/authorize?state=never-render-this-marker';
  let status: OAuthUiStatus = { status: 'not_connected', canLogin: true };
  let loginPosts = 0;
  let statusGets = 0;
  await page.route(/\/mcp-servers\/remote\/oauth(?:\/login)?$/, async route => {
    if (route.request().method() === 'POST') {
      loginPosts++;
      await route.fulfill({ status: 409, json: { error: unsafeDetail } });
    } else {
      statusGets++;
      await route.fulfill({ json: { ...status, message: unsafeDetail } });
    }
  });
  try {
    const { row, authState } = await setupOAuthUi(page, app.url);
    await expect(authState).toHaveText('로그인 안 됨');
    await row.getByRole('button', { name: 'remote 로그인', exact: true }).click();
    const dialog = page.getByRole('dialog', { name: 'remote 로그인', exact: true });
    const popupReady = page.context().waitForEvent('page');
    await dialog.getByRole('button', { name: '브라우저에서 로그인', exact: true }).click();
    const popup = await popupReady;
    await expect.poll(() => popup.isClosed()).toBe(true);
    await expect(dialog.getByRole('alert')).toContainText('실행 중인 작업을 마친 뒤 다시 시도');
    expect(await page.locator('body').innerText()).not.toContain('never-render-this-marker');
    expect(loginPosts).toBe(1);
    await page.evaluate(() => { window.open = () => null; });
    await dialog.getByRole('button', { name: '브라우저에서 로그인', exact: true }).click();
    await expect(dialog.getByRole('alert')).toContainText('팝업을 허용한 뒤 다시 시도');
    expect(loginPosts).toBe(1);
    await page.keyboard.press('Escape');

    status = { status: 'error', canLogin: true };
    await row.getByRole('button', { name: 'remote 인증 상태 새로고침', exact: true }).click();
    await expect(authState).toHaveText('인증 확인 실패');
    await expect(row.getByRole('button', { name: 'remote 다시 로그인', exact: true })).toBeEnabled();
    expect(await page.locator('body').innerText()).not.toContain('never-render-this-marker');
    status = { status: 'unavailable', canLogin: false };
    await row.getByRole('button', { name: 'remote 인증 상태 새로고침', exact: true }).click();
    await expect(authState).toHaveText('로그인 지원 안 됨');
    await expect(row.getByRole('button', { name: 'remote 로그인', exact: true })).toBeDisabled();

    // The existing vault access policy is an independent gate; do not call even GET OAuth on LAN.
    await page.route('**/api/vault', route => route.fulfill({ json: { canEdit: false } }));
    const beforeReload = statusGets;
    await page.reload();
    await page.getByRole('button', { name: '봇 편집', exact: true }).first().click();
    await expect(details(page).getByText(/localhost 연결이 아니에요/)).toBeVisible();
    await expect(authState).toHaveText('로컬에서만 로그인');
    await expect(row.getByRole('button', { name: 'remote 로그인', exact: true })).toBeDisabled();
    expect(statusGets).toBe(beforeReload);
    expect(loginPosts).toBe(1);
  } finally { await app.stop(); }
});

test('OAuth UI: explicit login polling is bounded and login_required or unsupported status stops until user retry', async ({ page, context }) => {
  const app = await startTestApp();
  const authorizationUrl = 'https://oauth-ui.invalid/authorize';
  let status: OAuthUiStatus = { status: 'not_connected', canLogin: true };
  let statusGets = 0;
  await context.route('https://oauth-ui.invalid/**', route => route.fulfill({ contentType: 'text/html', body: '<title>Mock OAuth</title>' }));
  await page.route(/\/mcp-servers\/remote\/oauth(?:\/login)?$/, async route => {
    if (route.request().method() === 'POST') {
      status = { status: 'awaiting_login', canLogin: true };
      await route.fulfill({ json: { authorizationUrl } });
    } else { statusGets++; await route.fulfill({ json: status }); }
  });
  try {
    const { row, authState } = await setupOAuthUi(page, app.url);
    await expect(authState).toHaveText('로그인 안 됨');
    await page.clock.install();
    await row.getByRole('button', { name: 'remote 로그인', exact: true }).click();
    const dialog = page.getByRole('dialog', { name: 'remote 로그인', exact: true });
    const popupReady = page.context().waitForEvent('page');
    await dialog.getByRole('button', { name: '브라우저에서 로그인', exact: true }).click();
    const popup = await popupReady;
    await popup.waitForURL(authorizationUrl);
    await expect(authState).toHaveText('로그인 대기 중');
    await page.clock.fastForward(95_000);
    await expect(authState).toHaveText('로그인 확인 시간 초과');
    await expect(row.getByRole('button', { name: 'remote 다시 로그인', exact: true })).toBeEnabled();
    const afterTimeout = statusGets;
    await page.clock.fastForward(180_000);
    expect(statusGets).toBe(afterTimeout);
    status = { status: 'login_required', canLogin: true };
    await row.getByRole('button', { name: 'remote 인증 상태 새로고침', exact: true }).click();
    await expect(authState).toHaveText('로그인 필요');
    await expect(row.getByRole('button', { name: 'remote 다시 로그인', exact: true })).toBeEnabled();
    status = { status: 'awaiting_login', canLogin: false };
    await row.getByRole('button', { name: 'remote 인증 상태 새로고침', exact: true }).click();
    await expect(authState).toHaveText('로그인 대기 중');
    await expect(row.getByRole('button', { name: 'remote 로그인', exact: true })).toBeDisabled();
    const afterUnsupported = statusGets;
    await page.clock.fastForward(180_000);
    expect(statusGets).toBe(afterUnsupported);
    await popup.close();
  } finally { await app.stop(); }
});

test('OAuth UI: config identity changes discard late status and login responses and close unused blank windows', async ({ page, context }) => {
  const app = await startTestApp();
  let holdStatus = false;
  let releaseStatus = () => {};
  let releaseLogin = () => {};
  let statusStarted = () => {};
  let loginStarted = () => {};
  const statusReady = new Promise<void>(resolve => { statusStarted = resolve; });
  const loginReady = new Promise<void>(resolve => { loginStarted = resolve; });
  const statusGate = new Promise<void>(resolve => { releaseStatus = resolve; });
  const loginGate = new Promise<void>(resolve => { releaseLogin = resolve; });
  let providerVisits = 0;
  await context.route('https://oauth-ui.invalid/**', route => { providerVisits++; return route.fulfill({ contentType: 'text/html', body: '<title>Stale mock OAuth</title>' }); });
  await page.route(/\/mcp-servers\/remote\/oauth(?:\/login)?$/, async route => {
    if (route.request().method() === 'POST') {
      loginStarted(); await loginGate;
      await route.fulfill({ json: { authorizationUrl: 'https://oauth-ui.invalid/stale-login' } }).catch(() => {});
    } else if (holdStatus) {
      holdStatus = false; statusStarted(); await statusGate;
      await route.fulfill({ json: { status: 'connected', canLogin: true } }).catch(() => {});
    } else await route.fulfill({ json: { status: 'not_connected', canLogin: true } });
  });
  try {
    const { path, row, authState } = await setupOAuthUi(page, app.url);
    await expect(authState).toHaveText('로그인 안 됨');
    holdStatus = true;
    await row.getByRole('button', { name: 'remote 인증 상태 새로고침', exact: true }).click();
    await statusReady;
    await vaultRequest(page, path, 'PUT', { json: JSON.stringify({ mcpServers: { remote: { url: 'https://changed.invalid/mcp' } } }) });
    await expect(authState).toHaveText('로그인 안 됨');
    releaseStatus();
    await expect(row.getByRole('button', { name: 'remote 로그인', exact: true })).toBeEnabled();
    await expect(row.getByRole('button', { name: 'remote 로그아웃', exact: true })).toHaveCount(0);

    await row.getByRole('button', { name: 'remote 로그인', exact: true }).click();
    const dialog = page.getByRole('dialog', { name: 'remote 로그인', exact: true });
    const popupReady = page.context().waitForEvent('page');
    await dialog.getByRole('button', { name: '브라우저에서 로그인', exact: true }).click();
    const popup = await popupReady;
    await loginReady;
    await vaultRequest(page, path, 'PUT', { json: JSON.stringify({ mcpServers: { remote: { url: 'https://changed-again.invalid/mcp' } } }) });
    await expect(dialog).toBeHidden();
    await expect.poll(() => popup.isClosed()).toBe(true);
    releaseLogin();
    await expect(authState).toHaveText('로그인 안 됨');
    expect(providerVisits).toBe(0);
  } finally { releaseStatus(); releaseLogin(); await app.stop(); }
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
    await expect(details(page).getByRole('button', { name: '사용자 지정 MCP 추가', exact: true })).toBeDisabled();
    await expect(page.getByRole('dialog', { name: 'MCP 서버 추가', exact: true })).toHaveCount(0);
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
      const probe = await fetch(`/api/bots/${bot}/mcp-servers/test`, {
        method: 'POST', headers,
        body: JSON.stringify({ json: JSON.stringify({ mcpServers: { lan_probe: { url: 'http://127.0.0.1:1/mcp', headers: { Authorization: '${secret:LAN_PROBE_TOKEN}' } } } }), secrets: [{ name: 'LAN_PROBE_TOKEN', value: 'lan-probe-must-not-persist', botId: bot }] }),
      });
      return [add.status, read.status, save.status, enable.status, probe.status];
    }, { bot });
    expect(results).toEqual([403, 403, 403, 403, 403]);
    expect((await snapshot(page)).secrets?.map(secret => secret.name)).toEqual(['LAN_TOKEN']);
    expect((await snapshot(page)).mcpServers).toEqual([expect.objectContaining({ name: 'tools', enabled: false })]);
    const state = JSON.stringify(await snapshot(page));
    expect(state).not.toContain(SECRET);
    expect(state).not.toContain('server.js');
  } finally {
    await loopback.close();
    await lan.close();
    await app.stop();
  }
});
