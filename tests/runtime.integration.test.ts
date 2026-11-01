import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { AddressInfo } from 'node:net';
import { ENGINE_COMMAND } from '../server/engine.js';
import { createBackend } from '../server/app.js';
import { startMockProvider, type MockCompletion, type MockProviderRequest } from './mock-provider.js';
export function scriptedFileReply(request: MockProviderRequest): MockCompletion {
  const names = request.tools?.map(tool => tool.function.name) ?? [];
  if (!names.includes('search_tool')) return { text: 'Local fixture title' };
  const latestUser = request.messages.findLastIndex(message => message.role === 'user' && JSON.stringify(message.content).includes('fixture'));
  const userText = JSON.stringify(request.messages[latestUser]?.content ?? '');
  if (latestUser < 0) return { text: '로컬 OpenAI 호환 서버 응답' };
  const calls = request.messages.slice(latestUser + 1).flatMap(message => message.tool_calls ?? []) as Array<{ function?: { name?: string } }>;
  if (!calls.some(call => call.function?.name === 'search_tool')) {
    return { tool: { name: 'search_tool', arguments: { query: 'workspace write_file', limit: 3 } } };
  }
  if (!calls.some(call => call.function?.name === 'use_tool')) {
    const content = userText.includes('denied') ? 'denied edit must never reach disk\n' : 'AI approved local file content\n';
    return { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__write_file', tool_input: { path: 'notes.txt', content } } } };
  }
  return { text: '로컬 파일 작업 요청 처리가 끝났습니다.' };
}

async function eventually<T>(read: () => T | Promise<T>, check: (value: T) => boolean, timeout = 25_000): Promise<T> {
  const deadline = Date.now() + timeout;
  let value = await read();
  while (!check(value)) {
    if (Date.now() >= deadline) throw new Error(`Condition timed out: ${JSON.stringify(value)}`);
    await new Promise(resolve => setTimeout(resolve, 100));
    value = await read();
  }
  return value;
}

test('source-linked Grok Build engine uses compatible provider, pauses file writes, denies, and continues sessions', { timeout: 100_000 }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'grok-runtime-test-'));
  const workspace = join(root, 'workspace');
  await mkdir(workspace);
  await writeFile(join(workspace, 'notes.txt'), 'before approval\n');
  const provider = await startMockProvider({ reply: scriptedFileReply });
  const backend = await createBackend({ dataDir: join(root, 'private'), serveWeb: false });
  await new Promise<void>(resolve => backend.server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${(backend.server.address() as AddressInfo).port}`;
  const api = async (path: string, body: unknown, method = 'POST') => {
    const response = await fetch(`${origin}${path}`, { method, headers: { Origin: origin, 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify(body) });
    const result = await response.json();
    assert.ok(response.ok, JSON.stringify(result));
    return result;
  };
  try {
    assert.ok(ENGINE_COMMAND.endsWith('/.engine/grok-bot-engine'));
    assert.equal(backend.store.runtime.available, true, backend.store.runtime.error);
    await api('/api/settings', { baseUrl: provider.baseUrl, model: 'local-test-model', workspace, apiKey: 'private-local-fixture-key' }, 'PUT');
    const botId = backend.store.snapshot().bots[0].id;
    const { conversation } = await api('/api/conversations', { botId });
    await api(`/api/conversations/${conversation.id}/messages`, { text: 'fixture approved edit' });
    const pending = await eventually(() => backend.store.snapshot(), state => state.conversations[0].run.status === 'awaiting_permission' || state.conversations[0].run.status === 'error');
    const run = pending.conversations[0].run;
    assert.equal(run.status, 'awaiting_permission', JSON.stringify(run));
    assert.equal(await readFile(join(workspace, 'notes.txt'), 'utf8'), 'before approval\n');
    assert.ok(run.permission);
    const allow = run.permission.options.find(option => option.kind === 'allow_once');
    assert.ok(allow);
    await api(`/api/conversations/${conversation.id}/permissions/${run.permission.id}`, { optionId: allow.id });
    const completed = await eventually(() => backend.store.snapshot(), state => ['completed', 'error'].includes(state.conversations[0].run.status));
    assert.equal(completed.conversations[0].run.status, 'completed', JSON.stringify(completed.conversations[0].run));
    assert.equal(await readFile(join(workspace, 'notes.txt'), 'utf8'), 'AI approved local file content\n');
    const sessionId = completed.conversations[0].sessionId;
    assert.ok(sessionId);
    assert.ok(!JSON.stringify(completed).includes('private-local-fixture-key'));
    await api(`/api/conversations/${conversation.id}/messages`, { text: 'fixture denied edit' });
    const second = await eventually(() => backend.store.snapshot(), state => ['awaiting_permission', 'error'].includes(state.conversations[0].run.status));
    assert.equal(second.conversations[0].run.status, 'awaiting_permission', JSON.stringify(second.conversations[0].run));
    const denial = second.conversations[0].run.permission!;
    const reject = denial.options.find(option => option.kind === 'reject_once');
    assert.ok(reject);
    await api(`/api/conversations/${conversation.id}/permissions/${denial.id}`, { optionId: reject.id });
    const afterDenied = await eventually(() => backend.store.snapshot(), state => ['completed', 'error'].includes(state.conversations[0].run.status));
    assert.equal(afterDenied.conversations[0].sessionId, sessionId);
    assert.equal(await readFile(join(workspace, 'notes.txt'), 'utf8'), 'AI approved local file content\n');
    const mainRequests = provider.requests.filter(request => request.tools?.some(tool => tool.function.name === 'search_tool'));
    assert.ok(mainRequests.length >= 4);
    assert.ok(mainRequests.every(request => request.model === 'local-test-model'));
    for (const request of mainRequests) {
      const tools = request.tools!.map(tool => tool.function.name);
      assert.ok(!tools.includes('run_terminal_command'), `Unrestricted shell was advertised: ${tools}`);
      assert.ok(!tools.includes('read_file'), `Unrestricted native reader was advertised: ${tools}`);
      assert.ok(!tools.includes('search_replace'), `Unrestricted editor was advertised: ${tools}`);
    }
  } finally {
    await backend.close();
    await provider.close();
    await rm(root, { recursive: true, force: true });
  }
});
