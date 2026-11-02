import { test, expect } from '@playwright/test';
import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import type { AppState } from '../shared/types';
import { startTestApp } from './app-fixture';
import { startMockProvider, type MockCompletion, type MockProviderRequest } from './mock-provider';

/** Provider decisions use only engine-delivered pages, never the app's state/API. */
test('actual engine delivers complete bounded canvas pages for tail editing and full approved export', async ({ page }) => {
  const content = '# Code document\r\n' + '한글 😀\t"\\ DaTa:text/plain;base64,QUJD\r\n'.repeat(1500) + '\r\nUNIQUE_CANVAS_TAIL';
  expect(Buffer.byteLength(content)).toBeGreaterThan(20_000);
  expect(Buffer.byteLength(content)).toBeLessThanOrEqual(128 * 1024);
  const changed = content.replace('UNIQUE_CANVAS_TAIL', 'UPDATED_CANVAS_TAIL');
  const observations = { pages: 0, readOriginal: false, readChanged: false, malformed: false, assemblies: [] as { revision: number; bytes: number; total: number; offsets: number[] }[] };
  const tool = (name: string, input: Record<string, unknown>): MockCompletion => ({ tool: { name: 'use_tool', arguments: { tool_name: `workspace__${name}`, tool_input: input } } });
  const provider = await startMockProvider({ reply: (request: MockProviderRequest): MockCompletion => {
    if (!request.tools?.some(item => item.function.name === 'search_tool')) return { text: 'Canvas paging acceptance' };
    const calls = request.messages.flatMap(message => message.tool_calls ?? []) as Array<{ function?: { name?: string; arguments?: string } }>;
    if (!calls.some(call => call.function?.name === 'search_tool')) return { tool: { name: 'search_tool', arguments: { query: 'workspace read_canvas replace_canvas_range write_file', limit: 5 } } };
    const outcomes = request.messages.filter(message => message.role === 'tool').map(message => typeof message.content === 'string' ? message.content : Array.isArray(message.content) ? message.content.map((part: { text?: string }) => part.text ?? '').join('') : '');
    try {
      const result = JSON.parse(outcomes.at(-1) ?? '{}');
      if (result.saved === true && typeof result.revision === 'number') return tool('read_canvas', {});
    } catch { /* Discovery and non-canvas results need not be JSON. */ }
    const pages = outcomes.flatMap(text => {
      if (!text.includes('"canvas"')) return [];
      try {
        const value = JSON.parse(text);
        if (!value.canvas) return [];
        if (Buffer.byteLength(text) > 18 * 1024) observations.malformed = true;
        return [value.canvas];
      } catch { observations.malformed = true; return []; }
    });
    if (!pages.length) return tool('read_canvas', {});
    const latest = pages.at(-1)!;
    observations.pages = Math.max(observations.pages, pages.length);
    if (latest.nextOffset !== undefined) return tool('read_canvas', { offset: latest.nextOffset, revision: latest.revision, viewHash: latest.viewHash });
    const sameView = pages.filter(item => item.revision === latest.revision && item.viewHash === latest.viewHash);
    const assembled = sameView.map(item => item.content).join('');
    observations.assemblies.push({ revision: latest.revision, bytes: Buffer.byteLength(assembled), total: latest.totalBytes, offsets: sameView.map(item => item.offset) });
    if (Buffer.byteLength(assembled) !== latest.totalBytes) observations.malformed = true;
    if (latest.revision === 1) {
      observations.readOriginal = assembled === content;
      if (!observations.readOriginal) return { text: 'Original pages did not reconstruct.' };
      return tool('replace_canvas_range', { revision: 1, oldText: 'UNIQUE_CANVAS_TAIL', replacement: 'UPDATED_CANVAS_TAIL' });
    }
    observations.readChanged = assembled === changed;
    const used = calls.filter(call => call.function?.name === 'use_tool').map(call => JSON.parse(call.function!.arguments ?? '{}').tool_name);
    if (!used.includes('workspace__write_file')) return tool('write_file', { path: 'canvas-export.txt', hash: '', content: assembled });
    return { text: 'Paged edit and export finished.' };
  } });
  const app = await startTestApp();
  const api = async (path: string, body: unknown, method = 'POST') => {
    const response = await page.request.fetch(app.url + path, { method, headers: { Origin: app.url, 'X-Grok-Bot': '1', 'Content-Type': 'application/json' }, data: body });
    expect(response.ok(), await response.text()).toBe(true); return response.json();
  };
  try {
    const state: AppState = await api('/api/settings', { baseUrl: provider.baseUrl, model: 'local-test-model', workspace: app.workspace }, 'PUT');
    const { conversation } = await api('/api/conversations', { botId: state.bots[0].id });
    await api(`/api/conversations/${conversation.id}/canvas`, { revision: 0, title: 'Code', language: 'text', content }, 'PUT');
    await page.goto(`${app.url}/?bot=${state.bots[0].id}&conversation=${conversation.id}`);
    await page.getByRole('textbox', { name: '메시지', exact: true }).fill('Read all canvas pages, change the tail and export the complete saved document.');
    await page.getByRole('button', { name: '메시지 보내기', exact: true }).click();
    await page.getByRole('button', { name: 'Replace canvas once', exact: true }).waitFor({ timeout: 60000 });
    expect(observations.readOriginal).toBe(true);
    await page.getByRole('button', { name: 'Replace canvas once', exact: true }).click();
    // A completed replacement returns its revision, not document bytes: reread the new view.
    await page.getByRole('button', { name: 'Allow once', exact: true }).waitFor({ timeout: 60000 });
    expect(observations.readChanged, JSON.stringify(observations)).toBe(true);
    await page.getByRole('button', { name: 'Allow once', exact: true }).click();
    await expect(page.getByText('작업 완료', { exact: true })).toBeVisible({ timeout: 60000 });
    expect(observations.pages).toBeGreaterThan(2);
    expect(observations.malformed).toBe(false);
    expect(await readFile(join(app.workspace, 'canvas-export.txt'), 'utf8')).toBe(changed);
  } finally { await app.stop(); await provider.close(); }
});
