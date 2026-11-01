import { test, expect } from '@playwright/test';
import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { networkInterfaces, tmpdir } from 'node:os';
import { join } from 'node:path';
import { createBackend } from '../server/app';
import { startMockProvider, type MockCompletion } from './mock-provider';
import type { AppState } from '../shared/types';

const rust = process.env.GROK_BOT_BACKEND === 'rust';
const lanAddress = () => Object.values(networkInterfaces()).flat().find(address => address?.family === 'IPv4' && !address.internal)?.address;

async function fixture() {
  const previous = process.env.NODE_ENV;
  process.env.NODE_ENV = 'test';
  const root = await mkdtemp(join(tmpdir(), 'grok-computer-browser-'));
  const workspace = join(root, 'workspace');
  await mkdir(workspace);
  await writeFile(join(workspace, 'notes.txt'), 'Local files remain available.');
  let releaseHang!: () => void;
  let signalHang!: () => void;
  const hang = new Promise<void>(resolve => { signalHang = resolve; });
  const release = new Promise<void>(resolve => { releaseHang = resolve; });
  const site = createServer(async (request, response) => {
    if (request.url === '/hang') { signalHang(); await release; }
    response.setHeader('Content-Type', 'text/html');
    response.end('<!doctype html><title>Computer browser fixture</title><body style="margin:24px;font:16px sans-serif"><label>Query <input aria-label="Query" id="query"></label><button id="count" onclick="this.textContent=\'Count \'+(++window.count)">Count 0</button><button onclick="this.textContent=document.querySelector(\'#query\').value">Show query</button><script>window.count=0</script></body>');
  });
  await new Promise<void>(resolve => site.listen(0, '127.0.0.1', resolve));
  const siteOrigin = `http://127.0.0.1:${(site.address() as AddressInfo).port}`;
  // The Rust server reads its loopback fixture origin from a variable honoured only by test builds
  // started with NODE_ENV=test. It listens on every interface so a LAN request can be checked.
  let backend: Awaited<ReturnType<typeof createBackend>> | undefined;
  let app: Awaited<ReturnType<typeof import('./app-fixture')['startTestApp']>> | undefined;
  let url: string;
  let lanUrl: string | undefined;
  if (rust) {
    const { startTestApp } = await import('./app-fixture');
    const hostname = lanAddress();
    if (!hostname) throw Error('This verification requires a network interface');
    app = await startTestApp({ network: true, hostname, env: { GROK_BOT_COMPUTER_TEST_ORIGINS: siteOrigin } });
    lanUrl = app.url;
    url = `http://127.0.0.1:${new URL(app.url).port}`;
  } else {
    backend = await createBackend({ dataDir: join(root, 'data'), dev: true, routines: { autoStart: false }, computer: { allowTestOrigins: [siteOrigin] } });
    await new Promise<void>(resolve => backend!.server.listen(0, '127.0.0.1', resolve));
    url = `http://127.0.0.1:${(backend.server.address() as AddressInfo).port}`;
  }
  return { backend, url, lanUrl, siteOrigin, workspace, hang, releaseHang, close: async () => {
    releaseHang();
    await backend?.close();
    await app?.stop();
    site.closeAllConnections();
    await new Promise<void>(resolve => site.close(() => resolve()));
    await rm(root, { recursive: true, force: true });
    if (previous === undefined) delete process.env.NODE_ENV; else process.env.NODE_ENV = previous;
  } };
}

const snapshot = (page: import('@playwright/test').Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());

async function clickComputer(page: import('@playwright/test').Page, x: number, y: number) {
  const image = page.getByRole('img', { name: '서버 관리형 브라우저의 현재 화면', exact: true });
  const bounds = await image.boundingBox();
  if (!bounds) throw Error('Missing browser preview');
  await image.click({ position: { x: x / 960 * bounds.width, y: y / 600 * bounds.height } });
}

test('managed computer starts, navigates, clicks, types, recovers from a blocked destination, and interrupts navigation in Chrome', async ({ page }, info) => {
  const f = await fixture();
  try {
    await page.setViewportSize({ width: 1040, height: 760 });
    await page.goto(f.url);
    await page.getByRole('button', { name: '컴퓨터 열기', exact: true }).click();
    await page.getByLabel('컴퓨터 허용 사이트', { exact: true }).fill(f.siteOrigin);
    await page.getByRole('button', { name: '브라우저 시작', exact: true }).click();
    await expect(page.getByLabel('컴퓨터 웹 주소', { exact: true })).toBeVisible();
    const oldPreview = await page.getByRole('img', { name: '서버 관리형 브라우저의 현재 화면', exact: true }).getAttribute('src');
    let releaseImage!: () => void;
    const imageGate = new Promise<void>(resolve => { releaseImage = resolve; });
    await page.route('**/api/computer/preview?*', async route => { await imageGate; await route.continue(); });
    await page.getByLabel('컴퓨터 웹 주소', { exact: true }).fill(f.siteOrigin);
    await page.getByRole('button', { name: '주소로 이동', exact: true }).click();
    await expect(page.getByText(f.siteOrigin + '/', { exact: true })).toBeVisible();
    await expect(page.getByRole('button', { name: '브라우저 화면 클릭', exact: true })).toBeDisabled();
    expect((await page.request.get(f.url + oldPreview)).status()).toBe(409);
    releaseImage();
    await expect(page.getByRole('button', { name: '브라우저 화면 클릭', exact: true })).toBeEnabled();
    await clickComputer(page, 100, 35);
    await page.getByLabel('컴퓨터 입력 텍스트', { exact: true }).fill('Browser-typed text');
    await page.getByRole('button', { name: '입력', exact: true }).click();
    await expect(page.getByLabel('컴퓨터 입력 텍스트', { exact: true })).toHaveValue('');
    await clickComputer(page, 350, 35);
    await page.getByRole('button', { name: '브라우저 확대', exact: true }).click();
    await page.screenshot({ path: info.outputPath('managed-browser-expanded.png') });
    await page.keyboard.press('Escape');
    await expect(page.locator('.computer-expanded')).toHaveCount(0);
    await page.getByLabel('컴퓨터 웹 주소', { exact: true }).fill('http://127.0.0.1:1/private');
    await page.getByRole('button', { name: '주소로 이동', exact: true }).click();
    await expect(page.getByRole('alert')).toContainText('not allowed');
    await page.getByRole('button', { name: '화면 새로고침', exact: true }).click();
    await expect(page.getByRole('img', { name: '서버 관리형 브라우저의 현재 화면', exact: true })).toBeVisible();
    await page.getByLabel('컴퓨터 웹 주소', { exact: true }).fill(f.siteOrigin + '/hang');
    await page.getByRole('button', { name: '주소로 이동', exact: true }).click();
    await Promise.race([f.hang, new Promise((_, reject) => setTimeout(() => reject(Error('Navigation did not reach the hanging fixture')), 5000))]);
    await page.getByRole('button', { name: '브라우저 중지', exact: true }).click();
    await expect(page.getByRole('button', { name: '브라우저 시작', exact: true })).toBeEnabled();
    await expect(page.getByLabel('컴퓨터 웹 주소', { exact: true })).toHaveCount(0);
  } finally { await f.close(); }
});

test('real source-engine computer observation waits for one-use approval and keeps manual controls locked during the run', async ({ page }) => {
  const f = await fixture();
  let stage = 0;
  const provider = await startMockProvider({ reply: request => {
    if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: 'Computer approval fixture' };
    const plan: MockCompletion[] = [
      { tool: { name: 'search_tool', arguments: { query: 'workspace computer_observe', limit: 2 } } },
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__computer_observe', tool_input: {} } } },
      { text: 'I observed the managed page after approval.' },
    ];
    return plan[stage++] ?? { text: 'Complete.' };
  } });
  try {
    await page.goto(f.url);
    await page.evaluate(async settings => {
      const response = await fetch('/api/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify(settings) });
      if (!response.ok) throw Error(await response.text());
    }, { baseUrl: provider.baseUrl, model: 'local-test-model', workspace: f.workspace });
    await page.getByRole('button', { name: '컴퓨터 열기', exact: true }).click();
    await page.getByLabel('컴퓨터 허용 사이트', { exact: true }).fill(f.siteOrigin);
    await page.getByRole('button', { name: '브라우저 시작', exact: true }).click();
    await expect(page.getByLabel('컴퓨터 웹 주소', { exact: true })).toBeVisible();
    await page.getByLabel('컴퓨터 웹 주소', { exact: true }).fill(f.siteOrigin);
    await page.getByRole('button', { name: '주소로 이동', exact: true }).click();
    await expect(page.getByText(f.siteOrigin + '/', { exact: true })).toBeVisible();
    await page.getByLabel('메시지', { exact: true }).fill('Observe this managed page.');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    const approval = page.getByRole('region', { name: '작업 승인 요청', exact: true });
    await expect(approval).toContainText('Observe managed browser');
    await expect(page.getByRole('button', { name: '주소로 이동', exact: true })).toBeDisabled();
    expect(provider.requests.some(request => JSON.stringify(request.messages).includes('Computer browser fixture'))).toBe(false);
    const pending = (await snapshot(page)).conversations.find(conversation => conversation.run.permission)!.run.permission!;
    expect(pending.localOnly).toBe(true);
    if (f.backend) {
      const conversation = f.backend.store.conversations.find(conversation => conversation.run.permission)!;
      await expect(f.backend.runtime.resolve(conversation.id, pending.id, pending.options[0].id, false)).rejects.toThrow('loopback');
    } else {
      const conversation = (await snapshot(page)).conversations.find(conversation => conversation.run.permission)!;
      const remote = await fetch(`${f.lanUrl}/api/conversations/${conversation.id}/permissions/${pending.id}`, { method: 'POST', headers: { Origin: f.lanUrl!, 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ optionId: pending.options[0].id }) });
      expect(remote.status).toBe(403);
      expect(await remote.text()).toContain('loopback');
    }
    await approval.getByRole('button', { name: 'Allow once', exact: true }).click();
    await expect(page.getByRole('log', { name: '대화 메시지', exact: true })).toContainText('I observed the managed page after approval.');
    expect(provider.requests.some(request => request.messages.some(message => message.role === 'tool' && String(message.content).includes('Computer browser fixture')))).toBe(true);
    expect(provider.requests.some(request => request.messages.some(message => message.role === 'tool' && String(message.content).includes('"origins"')))).toBe(false);
    await expect(page.getByRole('button', { name: '주소로 이동', exact: true })).toBeEnabled();
    let signalOrdinary!: () => void;
    let finish!: () => void;
    const began = new Promise<void>(resolve => { signalOrdinary = resolve; });
    const held = new Promise<void>(resolve => { finish = resolve; });
    const slow = await startMockProvider({ reply: async request => {
      if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: 'Ordinary chat' };
      signalOrdinary(); await held; return { text: 'Ordinary response' };
    } });
    try {
      await page.evaluate(async baseUrl => {
        const state = await (await fetch('/api/state')).json();
        const response = await fetch('/api/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ baseUrl, model: 'local-test-model', workspace: state.settings.workspace }) });
        if (!response.ok) throw Error(await response.text());
      }, slow.baseUrl);
      await page.getByLabel('메시지', { exact: true }).fill('An ordinary chat with no computer tools.');
      await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
      await began;
      await page.getByRole('button', { name: '작업 중단', exact: true }).click();
      await expect(page.getByRole('button', { name: '주소로 이동', exact: true })).toBeEnabled();
      await expect(page.getByLabel('컴퓨터 웹 주소', { exact: true })).toBeVisible();
    } finally { finish(); await slow.close(); }
    await page.getByRole('button', { name: '브라우저 중지', exact: true }).click();
    await expect(page.getByRole('button', { name: '브라우저 시작', exact: true })).toBeEnabled();
  } finally { await provider.close(); await f.close(); }
});

test('a network HTTP client gets a clear local-only computer state while its workspace editor stays usable', async ({ browser }) => {
  const { startTestApp } = await import('./app-fixture');
  const hostname = lanAddress();
  if (!hostname) throw Error('This LAN verification requires a network interface');
  const app = await startTestApp({ network: true, hostname });
  const context = await browser.newContext();
  const page = await context.newPage();
  try {
    await page.goto(app.url);
    expect(await page.evaluate(() => window.isSecureContext)).toBe(false);
    await page.getByRole('button', { name: '설정', exact: true }).click();
    await page.getByRole('button', { name: '모델 설정', exact: true }).first().click();
    await page.getByLabel('Base URL', { exact: true }).fill('http://127.0.0.1:8000/v1');
    await page.getByLabel('Model ID', { exact: true }).fill('local-fixture-model');
    await page.getByRole('button', { name: '로컬 파일 설정', exact: true }).click();
    await page.getByLabel('로컬 작업 폴더 경로', { exact: true }).fill(app.workspace);
    await page.getByRole('button', { name: '설정 저장', exact: true }).click();
    await expect(page.getByText('설정을 저장했습니다', { exact: true })).toBeVisible();
    await page.getByRole('button', { name: '설정 닫기', exact: true }).click();
    const state = await snapshot(page);
    const denied = await page.request.get(`${app.url}/api/computer?owner=${state.bots[0].id}`);
    expect(denied.status()).toBe(403);
    await page.getByRole('button', { name: '컴퓨터 열기', exact: true }).click();
    await expect(page.getByText('이 서버의 로컬 연결에서 사용할 수 있어요', { exact: true })).toBeVisible();
    await expect(page.getByRole('button', { name: '브라우저 시작', exact: true })).toBeDisabled();
    await page.getByRole('button', { name: '파일 notes.txt', exact: true }).click();
    await expect(page.getByLabel('파일 내용', { exact: true })).toHaveValue('original local content\n');
    await page.getByLabel('파일 내용', { exact: true }).fill('File editing remains available.');
    await page.getByRole('button', { name: '파일 저장', exact: true }).click();
    await expect(page.getByText('저장 완료', { exact: true })).toBeVisible();
  } finally { await context.close(); await app.stop(); }
});
