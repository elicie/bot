import { test, expect, type Dialog, type Page, type Request, type Route } from '@playwright/test';
import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import type { AppState, Bot } from '../shared/types';
import { startTestApp } from './app-fixture';

type TestApp = Awaited<ReturnType<typeof startTestApp>>;
type BotInput = Pick<Bot, 'name' | 'instructions' | 'color' | 'shape'>;
type DiskState = Pick<AppState, 'revision' | 'bots' | 'conversations' | 'routines' | 'routineRuns' | 'botExchanges' | 'botDeliveries'> & {
  version: 1;
  settings: Omit<AppState['settings'], 'hasApiKey'> & { apiKey: string };
};

const viewports = [
  { width: 1040, height: 760 },
  { width: 390, height: 844 },
  { width: 320, height: 640 },
];
const botDialog = (page: Page) => page.getByRole('dialog', { name: '봇 추가', exact: true });
const submitButton = (page: Page) => botDialog(page).getByRole('button', { name: '봇 추가', exact: true });
const botResponse = (page: Page) => page.waitForResponse(response => new URL(response.url()).pathname === '/api/bots' && response.request().method() === 'POST');
const snapshot = (page: Page): Promise<AppState> => page.evaluate(async () => {
  const response = await fetch('/api/state');
  if (!response.ok) throw new Error(await response.text());
  return response.json();
});
const diskBytes = (app: TestApp) => readFile(join(app.dataDir, 'state.json'), 'utf8');
const readDisk = async (app: TestApp): Promise<DiskState> => JSON.parse(await diskBytes(app));

function observe(page: Page) {
  const errors: string[] = [];
  const mutations: Request[] = [];
  page.on('pageerror', error => errors.push(error.message));
  page.on('request', request => {
    if (new URL(request.url()).pathname.startsWith('/api/') && !['GET', 'HEAD'].includes(request.method())) mutations.push(request);
  });
  return { errors, mutations };
}

async function openSidebar(page: Page) {
  if (page.viewportSize()!.width <= 760) await page.getByRole('button', { name: '봇 목록 열기', exact: true }).click();
}

async function openBotDialog(page: Page) {
  await openSidebar(page);
  await page.getByRole('complementary', { name: '봇 목록', exact: true }).getByRole('button', { name: '봇 추가', exact: true }).click();
  await expect(botDialog(page)).toBeVisible();
  await expect(botDialog(page).getByLabel('봇 이름', { exact: true })).toBeFocused();
}

async function expectFitsViewport(page: Page) {
  const viewport = page.viewportSize()!;
  const dialog = botDialog(page);
  const bounds = await dialog.boundingBox();
  expect(bounds).not.toBeNull();
  expect(bounds!.x).toBeGreaterThanOrEqual(0);
  expect(bounds!.y).toBeGreaterThanOrEqual(0);
  expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(viewport.width);
  expect(bounds!.y + bounds!.height).toBeLessThanOrEqual(viewport.height);
  expect(await dialog.evaluate(element => element.scrollWidth <= element.clientWidth)).toBe(true);
  await expect(submitButton(page)).toBeInViewport();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
}

function expectPublicDisk(state: AppState, disk: DiskState) {
  const { version, settings: privateSettings, ...durable } = disk;
  const { apiKey, ...settings } = privateSettings;
  expect(version).toBe(1);
  expect(state).toEqual({ ...durable, settings: { ...settings, hasApiKey: Boolean(apiKey) }, runtime: state.runtime });
}

async function expectCreated(page: Page, app: TestApp, responseState: AppState, before: AppState, diskBefore: DiskState, input: BotInput) {
  const created = responseState.bots.filter(bot => !before.bots.some(existing => existing.id === bot.id));
  expect(created).toHaveLength(1);
  const bot = created[0];
  expect(bot).toEqual({ ...input, id: expect.stringMatching(/^[\da-f]{8}(?:-[\da-f]{4}){3}-[\da-f]{12}$/i), createdAt: expect.any(String) });
  expect(new Date(bot.createdAt).toISOString()).toBe(bot.createdAt);
  expect(responseState).toEqual({ ...before, revision: before.revision + 1, bots: [...before.bots, bot] });
  await expect(botDialog(page)).not.toBeVisible();
  await expect(page.getByRole('heading', { name: input.name, exact: true })).toBeVisible();
  expect(new URL(page.url()).searchParams.get('bot')).toBe(bot.id);
  expect(new URL(page.url()).searchParams.get('conversation')).toBeNull();
  expect(new URL(page.url()).hash).toBe('');
  const state = await snapshot(page);
  const disk = await readDisk(app);
  expect(state).toEqual(responseState);
  expect(disk).toEqual({ ...diskBefore, revision: state.revision, bots: [...diskBefore.bots, bot] });
  expectPublicDisk(state, disk);
  return { state, disk };
}

async function expectDraft(page: Page, input: BotInput) {
  const dialog = botDialog(page);
  await expect(dialog.getByLabel('봇 이름', { exact: true })).toHaveValue(input.name);
  await expect(dialog.getByLabel('봇 지침', { exact: true })).toHaveValue(input.instructions);
  await expect(dialog.getByRole('button', { name: '꽃형 캐릭터 모양', exact: true })).toHaveAttribute('aria-pressed', 'true');
  await expect(dialog.getByRole('button', { name: '파랑 캐릭터 색상', exact: true })).toHaveAttribute('aria-pressed', 'true');
  await expect(dialog.getByRole('img', { name: '봇 아바타 미리보기', exact: true }).locator('path')).toHaveAttribute('fill', input.color);
}

for (const viewport of viewports) {
  test(`real bot creation persists default and customized avatars with keyboard and pointer submission at ${viewport.width}px`, async ({ page }, info) => {
    const app = await startTestApp();
    const { errors, mutations } = observe(page);
    try {
      await page.setViewportSize(viewport);
      await page.goto(app.url);
      await expect(page.getByRole('heading', { name: 'Grok', exact: true })).toBeVisible();
      const conversationResponse = page.waitForResponse(response => new URL(response.url()).pathname === '/api/conversations' && response.request().method() === 'POST');
      await page.getByRole('button', { name: '새 대화', exact: true }).click();
      expect((await conversationResponse).status()).toBe(201);
      await expect.poll(() => new URL(page.url()).searchParams.get('conversation')).not.toBeNull();
      await page.getByLabel('메시지', { exact: true }).fill('기존 대화의 미전송 초안');
      const before = await snapshot(page);
      const diskBefore = await readDisk(app);
      const bytesBefore = await diskBytes(app);
      expect(before.conversations).toHaveLength(1);
      expectPublicDisk(before, diskBefore);

      await openBotDialog(page);
      const dialog = botDialog(page);
      const name = dialog.getByLabel('봇 이름', { exact: true });
      await expect(name).toHaveValue('');
      await expect(name).toHaveAttribute('required', '');
      await expect(submitButton(page)).toBeDisabled();
      await expect(dialog.getByRole('button', { name: '방울형 캐릭터 모양', exact: true })).toHaveAttribute('aria-pressed', 'true');
      await expect(dialog.getByRole('button', { name: '흰색 캐릭터 색상', exact: true })).toHaveAttribute('aria-pressed', 'true');
      await expect(dialog.getByLabel('봇 지침', { exact: true })).toHaveValue('');
      await expectFitsViewport(page);
      await name.fill('   ');
      await name.press('Enter');
      await expect(submitButton(page)).toBeDisabled();
      expect(await snapshot(page)).toEqual(before);
      expect(await diskBytes(app)).toBe(bytesBefore);
      expect(mutations.map(request => new URL(request.url()).pathname)).toEqual(['/api/conversations']);

      const defaults: BotInput = { name: `Default ${viewport.width}`, instructions: '', color: '#ffffff', shape: 'blob' };
      await name.fill(defaults.name);
      await expect(submitButton(page)).toBeEnabled();
      await page.screenshot({ path: info.outputPath('bot-defaults.png') });
      let pending = botResponse(page);
      await name.press('Enter');
      let response = await pending;
      expect(response.status()).toBe(201);
      expect(response.request().postDataJSON()).toEqual(defaults);
      expect(response.request().headers()['x-grok-bot']).toBe('1');
      expect(await response.request().headerValue('origin')).toBe(app.url);
      const first = await expectCreated(page, app, await response.json(), before, diskBefore, defaults);

      await openBotDialog(page);
      await expect(name).toHaveValue('');
      await expect(submitButton(page)).toBeDisabled();
      const custom: BotInput = { name: `도우미 ${viewport.width}`, instructions: '  Keep these instructions exactly.\n한국어 지침과 <태그> & punctuation.  ', color: '#198aff', shape: 'flower' };
      await dialog.getByRole('button', { name: '꽃형 캐릭터 모양', exact: true }).click();
      await dialog.getByRole('button', { name: '파랑 캐릭터 색상', exact: true }).click();
      await dialog.getByLabel('봇 지침', { exact: true }).fill(custom.instructions);
      await expect(submitButton(page)).toBeDisabled();
      await name.fill(`  ${custom.name}  `);
      await expectDraft(page, { ...custom, name: `  ${custom.name}  ` });
      await expectFitsViewport(page);
      await page.screenshot({ path: info.outputPath('bot-customized.png') });
      pending = botResponse(page);
      await submitButton(page).click();
      response = await pending;
      expect(response.status()).toBe(201);
      expect(response.request().postDataJSON()).toEqual(custom);
      const second = await expectCreated(page, app, await response.json(), first.state, first.disk, custom);
      const customUrl = page.url();
      if (viewport.width <= 760) await expect(page.getByRole('complementary', { name: '봇 목록', exact: true })).not.toBeVisible();
      await openSidebar(page);
      await page.getByRole('complementary', { name: '봇 목록', exact: true }).getByRole('button', { name: 'Grok', exact: true }).click();
      await expect(page.getByLabel('메시지', { exact: true })).toHaveValue('기존 대화의 미전송 초안');
      expect(new URL(page.url()).searchParams.get('conversation')).toBe(before.conversations[0].id);
      await page.goto(customUrl);
      await expect(page.getByRole('heading', { name: custom.name, exact: true })).toBeVisible();
      expect(await snapshot(page)).toEqual(second.state);
      expect(await readDisk(app)).toEqual(second.disk);
      expect(await readFile(join(app.workspace, 'notes.txt'), 'utf8')).toBe('original local content\n');
      expect(mutations.map(request => `${request.method()} ${new URL(request.url()).pathname}`)).toEqual(['POST /api/conversations', 'POST /api/bots', 'POST /api/bots']);
      expect(errors).toEqual([]);
    } finally {
      await app.stop();
    }
  });

  test(`bot cancellation, dirty guards, busy state, and a real backend error preserve drafts and disk at ${viewport.width}px`, async ({ page }, info) => {
    const app = await startTestApp();
    const { errors, mutations } = observe(page);
    const confirmations: string[] = [];
    let discard = false;
    const confirm = async (dialog: Dialog) => {
      confirmations.push(dialog.message());
      if (discard) await dialog.accept();
      else await dialog.dismiss();
    };
    page.on('dialog', confirm);
    try {
      await page.setViewportSize(viewport);
      await page.goto(app.url);
      await expect(page.getByRole('heading', { name: 'Grok', exact: true })).toBeVisible();
      const before = await snapshot(page);
      const diskBefore = await readDisk(app);
      const bytesBefore = await diskBytes(app);
      await openBotDialog(page);
      await botDialog(page).getByRole('button', { name: '취소', exact: true }).click();
      await expect(botDialog(page)).not.toBeVisible();
      expect(confirmations).toEqual([]);
      await expect(page.getByRole('button', { name: viewport.width <= 760 ? '봇 목록 열기' : '봇 추가', exact: true })).toBeFocused();

      for (const field of ['name', 'shape', 'color', 'instructions']) {
        await openBotDialog(page);
        const dialog = botDialog(page);
        const name = dialog.getByLabel('봇 이름', { exact: true });
        const instructions = dialog.getByLabel('봇 지침', { exact: true });
        const shape = dialog.getByRole('button', { name: '꽃형 캐릭터 모양', exact: true });
        const color = dialog.getByRole('button', { name: '파랑 캐릭터 색상', exact: true });
        if (field === 'name') await name.fill('저장하지 않을 이름');
        if (field === 'shape') await shape.click();
        if (field === 'color') await color.click();
        if (field === 'instructions') await instructions.fill('저장하지 않을 지침\n그대로 보존');
        const count = confirmations.length;
        await dialog.getByRole('button', { name: '취소', exact: true }).click();
        await expect(dialog).toBeVisible();
        expect(confirmations.slice(count)).toEqual(['저장하지 않은 봇 설정을 버릴까요?']);
        await expect(name).toHaveValue(field === 'name' ? '저장하지 않을 이름' : '');
        await expect(instructions).toHaveValue(field === 'instructions' ? '저장하지 않을 지침\n그대로 보존' : '');
        await expect(shape).toHaveAttribute('aria-pressed', String(field === 'shape'));
        await expect(color).toHaveAttribute('aria-pressed', String(field === 'color'));
        discard = true;
        await page.keyboard.press('Escape');
        await expect(dialog).not.toBeVisible();
        discard = false;
        expect(await snapshot(page)).toEqual(before);
        expect(await diskBytes(app)).toBe(bytesBefore);
      }
      expect(mutations).toHaveLength(0);

      await openBotDialog(page);
      await botDialog(page).getByRole('button', { name: '꽃형 캐릭터 모양', exact: true }).click();
      await botDialog(page).getByRole('button', { name: '방울형 캐릭터 모양', exact: true }).click();
      const cleanCount = confirmations.length;
      await botDialog(page).getByRole('button', { name: '봇 설정 닫기', exact: true }).click();
      await expect(botDialog(page)).not.toBeVisible();
      expect(confirmations).toHaveLength(cleanCount);

      await openBotDialog(page);
      const input: BotInput = { name: `Retry ${viewport.width}`, instructions: '  Preserve the failed draft.\n실패 뒤에도 이 지침을 보존.  ', color: '#198aff', shape: 'flower' };
      await botDialog(page).getByLabel('봇 이름', { exact: true }).fill(input.name);
      await botDialog(page).getByLabel('봇 지침', { exact: true }).fill(input.instructions);
      await botDialog(page).getByRole('button', { name: '꽃형 캐릭터 모양', exact: true }).click();
      await botDialog(page).getByRole('button', { name: '파랑 캐릭터 색상', exact: true }).click();
      let releaseRequest = () => {};
      const requestGate = new Promise<void>(resolve => { releaseRequest = resolve; });
      const reject = async (route: Route) => {
        await requestGate;
        const headers = { ...route.request().headers() };
        delete headers['x-grok-bot'];
        await route.continue({ headers });
      };
      await page.route('**/api/bots', reject);
      let pending = botResponse(page);
      await submitButton(page).click();
      await expect(botDialog(page).locator('form')).toHaveAttribute('aria-busy', 'true');
      await expect(submitButton(page)).toBeDisabled();
      await expect(botDialog(page).getByRole('button', { name: '취소', exact: true })).toBeDisabled();
      await expect(botDialog(page).getByRole('button', { name: '봇 설정 닫기', exact: true })).toBeDisabled();
      await expect(botDialog(page).getByLabel('봇 이름', { exact: true })).toBeDisabled();
      await expect(botDialog(page).getByLabel('봇 지침', { exact: true })).toBeDisabled();
      await page.keyboard.press('Enter');
      await page.keyboard.press('Escape');
      await expect(botDialog(page)).toBeVisible();
      expect(confirmations).toHaveLength(cleanCount);
      expect(mutations).toHaveLength(1);
      releaseRequest();
      let response = await pending;
      expect(response.status()).toBe(403);
      const message = 'Mutations require same-origin JSON and X-Grok-Bot: 1.';
      expect(await response.json()).toEqual({ error: message });
      await expect(botDialog(page).getByRole('alert')).toHaveText(message);
      await expect(botDialog(page).getByRole('alert')).toBeInViewport({ ratio: 1 });
      await expectDraft(page, input);
      await expect(submitButton(page)).toBeEnabled();
      await expectFitsViewport(page);
      await page.screenshot({ path: info.outputPath('bot-real-backend-error.png') });
      expect(await snapshot(page)).toEqual(before);
      expect(await diskBytes(app)).toBe(bytesBefore);

      await page.unroute('**/api/bots', reject);
      pending = botResponse(page);
      await botDialog(page).getByLabel('봇 이름', { exact: true }).press('Enter');
      response = await pending;
      expect(response.status()).toBe(201);
      expect(response.request().postDataJSON()).toEqual(input);
      const saved = await expectCreated(page, app, await response.json(), before, diskBefore, input);
      await page.reload();
      await expect(page.getByRole('heading', { name: input.name, exact: true })).toBeVisible();
      expect(await snapshot(page)).toEqual(saved.state);
      expect(await readDisk(app)).toEqual(saved.disk);
      expect(await readFile(join(app.workspace, 'notes.txt'), 'utf8')).toBe('original local content\n');
      expect(mutations.map(request => `${request.method()} ${new URL(request.url()).pathname}`)).toEqual(['POST /api/bots', 'POST /api/bots']);
      expect(errors).toEqual([]);
    } finally {
      page.off('dialog', confirm);
      await app.stop();
    }
  });
}
