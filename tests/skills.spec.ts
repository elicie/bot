import { test, expect, type Page } from '@playwright/test';
import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { startTestApp } from './app-fixture';
import { startMockProvider, type MockCompletion, type MockProviderRequest } from './mock-provider';
import type { AppState } from '../shared/types';

const snapshot = (page: Page): Promise<AppState> => page.evaluate(async () => (await fetch('/api/state')).json());
const initialSkill = { name: 'plan-review', description: 'Use to review a local plan.', instructions: 'BROWSER_SKILL_CANARY\nList the unresolved plan questions.' };
const skillDialog = (page: Page) => page.getByRole('dialog', { name: '앱 연결', exact: true });
const nameInput = (page: Page) => skillDialog(page).getByRole('textbox', { name: '스킬 이름', exact: true });
const descriptionInput = (page: Page) => skillDialog(page).getByRole('textbox', { name: '스킬 설명', exact: true });
const instructionsInput = (page: Page) => skillDialog(page).getByRole('textbox', { name: '스킬 지침 (Markdown)', exact: true });
const editSkill = (page: Page, name = initialSkill.name) => skillDialog(page).getByRole('button', { name: `${name} 스킬 편집`, exact: true }).click();
async function openSidebar(page: Page) {
  if (page.viewportSize()!.width <= 760) await page.getByRole('button', { name: '봇 목록 열기', exact: true }).click();
}
async function openSkills(page: Page) {
  await openSidebar(page);
  await page.getByRole('button', { name: '앱 연결', exact: true }).click();
  await expect(skillDialog(page)).toBeVisible();
}
async function createSkill(page: Page, input = initialSkill) {
  await skillDialog(page).getByRole('button', { name: '스킬 추가', exact: true }).click();
  await nameInput(page).fill(input.name);
  await descriptionInput(page).fill(input.description);
  await instructionsInput(page).fill(input.instructions);
  await skillDialog(page).getByRole('button', { name: '저장', exact: true }).click();
  await expect(skillDialog(page).getByRole('button', { name: `${input.name} 스킬 편집`, exact: true })).toBeVisible();
}

// The backend and browser use disposable user data; local skills never reach the live installation.
test('browser manages private skills, preserves stale drafts, and isolates bots across desktop and mobile navigation', async ({ page }, info) => {
  const app = await startTestApp();
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  try {
    await page.setViewportSize({ width: 1040, height: 760 });
    await page.goto(app.url);
    await openSkills(page);
    const dialog = skillDialog(page);
    await expect(dialog).toContainText('비공개 스킬');
    await createSkill(page);
    await expect.poll(async () => (await snapshot(page)).bots[0].skills?.length).toBe(1);
    expect((await snapshot(page)).bots[0].skills![0].instructions).toBe(initialSkill.instructions);
    await page.screenshot({ path: info.outputPath('skill-list-desktop.png') });
    await editSkill(page);
    await instructionsInput(page).fill('Unsaved instructions');
    page.once('dialog', prompt => prompt.dismiss());
    await dialog.getByRole('button', { name: '뒤로', exact: true }).click();
    await expect(instructionsInput(page)).toHaveValue('Unsaved instructions');
    page.once('dialog', prompt => prompt.accept());
    await dialog.getByRole('button', { name: '뒤로', exact: true }).click();
    const toggle = dialog.getByRole('switch', { name: /plan-review/ });
    await toggle.click();
    await expect(toggle).toHaveAttribute('aria-checked', 'false');
    await expect.poll(async () => (await snapshot(page)).bots[0].skills![0].enabled).toBe(false);
    await toggle.click();
    await expect(toggle).toHaveAttribute('aria-checked', 'true');
    await editSkill(page);
    await instructionsInput(page).fill('Stale local draft');
    await page.evaluate(async ({ bot, skill }) => {
      const response = await fetch(`/api/bots/${bot.id}/skills/${skill.id}`, { method: 'PATCH', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ revision: skill.revision, instructions: 'Saved from another tab.' }) });
      if (!response.ok) throw Error(await response.text());
    }, { bot: (await snapshot(page)).bots[0], skill: (await snapshot(page)).bots[0].skills![0] });
    await expect(dialog).toContainText('초안과 편집 기준 버전을 유지했습니다');
    const conflict = page.waitForResponse(response => response.request().method() === 'PATCH' && response.url().includes('/skills/'));
    await dialog.getByRole('button', { name: '저장', exact: true }).click();
    expect((await conflict).status()).toBe(409);
    await expect(dialog).toContainText('초안은 그대로 유지됩니다');
    await expect(instructionsInput(page)).toHaveValue('Stale local draft');
    page.once('dialog', prompt => prompt.accept());
    await dialog.getByRole('button', { name: '앱 연결 닫기', exact: true }).click();
    await page.reload();
    await openSkills(page);
    await editSkill(page);
    await expect(instructionsInput(page)).toHaveValue('Saved from another tab.');
    await page.setViewportSize({ width: 390, height: 844 });
    await instructionsInput(page).fill('모바일에서 저장한 전체 지침.');
    await dialog.getByRole('button', { name: '저장', exact: true }).click();
    await expect(dialog.getByRole('button', { name: 'plan-review 스킬 편집', exact: true })).toBeVisible();
    await editSkill(page);
    await expect(instructionsInput(page)).toHaveValue('모바일에서 저장한 전체 지침.');
    await page.screenshot({ path: info.outputPath('skill-editor-mobile.png') });
    expect(await dialog.evaluate(element => element.scrollWidth <= element.clientWidth)).toBe(true);
    await dialog.getByRole('button', { name: '앱 연결 닫기', exact: true }).click();
    await openSidebar(page);
    await page.getByRole('complementary', { name: '봇 목록', exact: true }).getByRole('button', { name: '봇 추가', exact: true }).click();
    const addBot = page.getByRole('dialog', { name: '봇 추가', exact: true });
    await addBot.getByLabel('봇 이름', { exact: true }).fill('Second bot');
    await addBot.getByRole('button', { name: '봇 추가', exact: true }).click();
    await expect(addBot).not.toBeVisible();
    await openSkills(page);
    await expect(dialog).toContainText('아직 비공개 스킬이 없습니다');
    await createSkill(page, { ...initialSkill, instructions: 'SECOND_BOT_PRIVATE_INSTRUCTIONS' });
    const bots = (await snapshot(page)).bots;
    expect(bots[0].skills![0].instructions).toBe('모바일에서 저장한 전체 지침.');
    expect(bots[1].skills![0].instructions).toBe('SECOND_BOT_PRIVATE_INSTRUCTIONS');
    await dialog.getByRole('button', { name: '앱 연결 닫기', exact: true }).click();
    await openSidebar(page);
    await page.getByRole('complementary', { name: '봇 목록', exact: true }).getByRole('button', { name: 'Grok', exact: true }).click();
    await openSkills(page);
    await editSkill(page);
    await expect(instructionsInput(page)).toHaveValue('모바일에서 저장한 전체 지침.');
    await page.setViewportSize({ width: 320, height: 640 });
    await instructionsInput(page).fill('가'.repeat(7000));
    await dialog.getByRole('button', { name: '저장', exact: true }).click();
    await expect(dialog.getByRole('alert')).toContainText(/18 KiB/);
    await expect(dialog.getByRole('alert')).toBeInViewport({ ratio: 0.95 });
    await expect(instructionsInput(page)).toHaveValue('가'.repeat(7000));
    expect((await snapshot(page)).bots[0].skills![0].instructions).toBe('모바일에서 저장한 전체 지침.');
    await instructionsInput(page).fill('모바일에서 저장한 전체 지침.');
    await page.screenshot({ path: info.outputPath('skill-editor-mobile320.png') });
    expect(await dialog.evaluate(element => element.scrollWidth <= element.clientWidth)).toBe(true);
    page.once('dialog', prompt => prompt.accept());
    await dialog.getByRole('button', { name: '스킬 삭제', exact: true }).click();
    await expect.poll(async () => (await snapshot(page)).bots[0].skills?.length).toBe(0);
    const disk = JSON.parse(await readFile(join(app.dataDir, 'state.json'), 'utf8'));
    expect(disk.bots[0].skills).toEqual([]);
    expect(disk.bots[1].skills[0].instructions).toBe('SECOND_BOT_PRIVATE_INSTRUCTIONS');
    expect(errors).toEqual([]);
  } finally { await app.stop(); }
});

test('browser imports a complete SKILL.md and retains existing skills after malformed or unsupported metadata', async ({ page }) => {
  const app = await startTestApp();
  try {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(app.url);
    await openSkills(page);
    const dialog = skillDialog(page);
    const input = dialog.getByLabel('SKILL.md 파일 선택', { exact: true });
    await input.setInputFiles({ name: 'SKILL.md', mimeType: 'text/markdown', buffer: Buffer.from('---\nname: imported-browser\ndescription: Use when reviewing the browser fixture.\n---\n가져온 전체 지침.\nCOMPLETE_IMPORT_TAIL') });
    await expect(dialog.getByRole('button', { name: 'imported-browser 스킬 편집', exact: true })).toBeVisible();
    await editSkill(page, 'imported-browser');
    await expect(instructionsInput(page)).toHaveValue('가져온 전체 지침.\nCOMPLETE_IMPORT_TAIL');
    await dialog.getByRole('button', { name: '뒤로', exact: true }).click();
    const before = await snapshot(page);
    await input.setInputFiles({ name: 'SKILL.md', mimeType: 'text/markdown', buffer: Buffer.from('---\nname: unsafe-browser\ndescription: Bad metadata\nallowed-tools: Bash\n---\nunsafe') });
    await expect(dialog.getByRole('alert')).toContainText('supports only name');
    expect((await snapshot(page)).bots).toEqual(before.bots);
    await input.setInputFiles({ name: 'SKILL.md', mimeType: 'text/markdown', buffer: Buffer.from([0xff, 0xfe, 0xfd]) });
    await expect(dialog.getByRole('alert')).toContainText('UTF-8');
    expect((await snapshot(page)).bots).toEqual(before.bots);
  } finally { await app.stop(); }
});

test('skill creation request keeps the existing composer draft and returns from routines without sending', async ({ page }) => {
  const app = await startTestApp();
  let sent = 0;
  page.on('request', request => { if (request.method() === 'POST' && new URL(request.url()).pathname.endsWith('/messages')) sent++; });
  try {
    await page.goto(app.url);
    await page.getByLabel('메시지', { exact: true }).fill('기존 작성 중인 내용');
    await page.getByRole('button', { name: '루틴', exact: true }).click();
    await expect(page.getByRole('main', { name: '루틴', exact: true })).toBeVisible();
    await openSkills(page);
    await skillDialog(page).getByRole('button', { name: '봇에게 만들기 요청', exact: true }).click();
    await expect(skillDialog(page)).not.toBeVisible();
    await expect(page.getByRole('main', { name: '대화', exact: true })).toBeVisible();
    await expect(page.getByLabel('메시지', { exact: true })).toHaveValue(/^기존 작성 중인 내용\n\n이 봇을 위한/);
    expect(new URL(page.url()).searchParams.get('view')).toBeNull();
    expect((await snapshot(page)).conversations).toEqual([]);
    expect(sent).toBe(0);
  } finally { await app.stop(); }
});

test('browser approval saves a model-created skill and the real source engine loads its instructions in a later turn', async ({ page }) => {
  const app = await startTestApp();
  let skillId = '';
  let stage = 0;
  const inference: MockProviderRequest[] = [];
  const provider = await startMockProvider({ reply: request => {
    if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: 'Skill browser fixture' };
    inference.push(request);
    const plan: MockCompletion[] = [
      { tool: { name: 'search_tool', arguments: { query: 'workspace create_skill', limit: 2 } } },
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__create_skill', tool_input: initialSkill } } },
      { text: 'The reusable private skill was saved.' },
      { tool: { name: 'search_tool', arguments: { query: 'workspace read_skill', limit: 2 } } },
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__read_skill', tool_input: { id: skillId } } } },
      { text: 'I loaded the saved private skill.' },
    ];
    return plan[stage++] ?? { text: 'Complete.' };
  } });
  try {
    await page.goto(app.url);
    await page.evaluate(async baseUrl => {
      const response = await fetch('/api/settings', { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ baseUrl, model: 'local-test-model', workspace: '' }) });
      if (!response.ok) throw Error(await response.text());
    }, provider.baseUrl);
    await page.getByLabel('메시지', { exact: true }).fill('Create a reusable plan-review skill.');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    const approval = page.getByRole('region', { name: '작업 승인 요청', exact: true });
    await expect(approval).toContainText(initialSkill.instructions);
    expect((await snapshot(page)).bots[0].skills).toBeUndefined();
    await approval.getByRole('button', { name: 'Save skill once', exact: true }).click();
    await expect(page.getByRole('log', { name: '대화 메시지', exact: true })).toContainText('The reusable private skill was saved.');
    await expect.poll(async () => (await snapshot(page)).conversations[0].run.status).toBe('completed');
    skillId = (await snapshot(page)).bots[0].skills![0].id;
    await openSkills(page);
    await editSkill(page);
    await expect(instructionsInput(page)).toHaveValue(initialSkill.instructions);
    await skillDialog(page).getByRole('button', { name: '앱 연결 닫기', exact: true }).click();
    await page.getByLabel('메시지', { exact: true }).fill('Use the plan-review skill.');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    await expect(page.getByRole('log', { name: '대화 메시지', exact: true })).toContainText('I loaded the saved private skill.');
    expect(inference.some(request => request.messages.some(message => message.role === 'tool' && String(message.content).includes('BROWSER_SKILL_CANARY')))).toBe(true);
  } finally { await app.stop(); await provider.close(); }
});
