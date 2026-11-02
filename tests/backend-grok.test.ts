import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtemp, mkdir, writeFile, readFile, rm, readdir, symlink } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import type { AddressInfo } from 'node:net';
import { ENGINE_COMMAND } from '../server/engine.js';
import { createBackend } from '../server/app.js';
import { startMockProvider, type MockCompletion, type MockProviderRequest } from './mock-provider.js';
import { randomUUID } from 'node:crypto';
import type { AppState, Conversation } from '../shared/types.js';

test('source-linked app engine exposes only broker tools, blocks bypasses, approves exact disk writes, and continues its ACP session', { timeout: 45000 }, async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'grok-bot-backend-real-'));
  const workspace = path.join(root, 'workspace');
  await mkdir(workspace);
  await writeFile(path.join(workspace, 'notes.txt'), 'original\n');
  await writeFile(path.join(root, 'outside.txt'), 'outside stays unchanged\n');
  await symlink(root, path.join(workspace, 'linked-parent'));
  await symlink(path.join(root, 'outside.txt'), path.join(workspace, 'linked-file.txt'));
  let plan: MockCompletion[] = [];
  const inference: MockProviderRequest[] = [];
  const provider = await startMockProvider({ reply: request => {
    if (request.tools?.some(tool => tool.function.name === 'session_title')) return { tool: { name: 'session_title', arguments: { title: 'Deterministic local test' } } };
    inference.push(request);
    return plan.shift() ?? { text: 'Local turn complete.' };
  } });
  const backend = await createBackend({ dataDir: path.join(root, 'data'), serveWeb: false });
  await new Promise<void>(resolve => backend.server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${(backend.server.address() as AddressInfo).port}`;
  async function api(route: string, body?: unknown, method = body === undefined ? 'GET' : 'POST') {
    return fetch(origin + route, { method, headers: { Origin: origin, 'X-Grok-Bot': '1', 'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });
  }
  const state = async () => await (await api('/api/state')).json() as AppState;
  async function wait(id: string, predicate: (conversation: Conversation) => boolean) {
    const deadline = Date.now() + 15000;
    let current: Conversation;
    do {
      current = (await state()).conversations.find(c => c.id === id)!;
      if (predicate(current)) return current;
      if (current.run.status === 'error') throw new Error(current.run.error);
      await new Promise(resolve => setTimeout(resolve, 20));
    } while (Date.now() < deadline);
    throw new Error(`Timed out waiting for actual Grok. ${JSON.stringify(current)}`);
  }
  try {
    assert.equal(path.isAbsolute(ENGINE_COMMAND), true);
    assert.equal(backend.store.runtime.available, true, backend.store.runtime.error);
    assert.match(backend.store.runtime.version ?? '', /2bdd1d6a/);
    assert.equal((await api('/api/settings', { baseUrl: provider.baseUrl, model: 'local-test-model', workspace }, 'PUT')).status, 200);
    const { conversation } = await (await api('/api/conversations', { botId: backend.store.bots[0].id })).json() as { conversation: Conversation };
    plan = [
      { tool: { name: 'search_tool', arguments: { query: 'workspace write_file', limit: 3 } } },
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__write_file', tool_input: { path: 'notes.txt', content: 'approved by browser\n' } } } },
      { text: 'The approved write succeeded.' },
    ];
    assert.equal((await api(`/api/conversations/${conversation.id}/messages`, { text: 'Write notes.txt using the workspace broker.' })).status, 202);
    const pending = await wait(conversation.id, c => c.run.status === 'awaiting_permission');
    assert.equal(await readFile(path.join(workspace, 'notes.txt'), 'utf8'), 'original\n');
    const permission = pending.run.permission!;
    assert.deepEqual(permission.paths, ['notes.txt']);
    assert.equal((await api(`/api/conversations/${conversation.id}/permissions/${permission.id}`, { optionId: permission.options.find(o => o.kind === 'allow_once')!.id })).status, 200);
    const completed = await wait(conversation.id, c => c.run.status === 'completed');
    assert.equal(completed.messages.at(-1)!.text, 'The approved write succeeded.');
    assert.equal(await readFile(path.join(workspace, 'notes.txt'), 'utf8'), 'approved by browser\n');
    assert.ok(inference.length >= 3);
    assert.equal(provider.requests.every(request => request.model === 'local-test-model'), true);
    for (const request of inference) assert.deepEqual(request.tools!.map(tool => tool.function.name).sort(), ['search_tool', 'todo_write', 'use_tool']);
    assert.ok(inference.some(request => request.messages.some(message => message.role === 'tool' && String(message.content).includes('workspace__write_file'))));
    const originalSession = completed.sessionId;
    const beforeContinuation = inference.length;
    plan = [
      { tool: { name: 'run_terminal_command', arguments: { command: `printf bypass > ${path.join(root, 'outside.txt')}` } } },
      { tool: { name: 'read_file', arguments: { target_file: path.join(root, 'outside.txt') } } },
      { tool: { name: 'use_tool', arguments: { file: path.join(root, 'outside.txt') } } },
      { tool: { name: 'use_tool', arguments: { file: '~/.grok/config.toml' } } },
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__write_file', tool_input: { path: '../outside.txt', content: 'bypass' } } } },
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__read_file', tool_input: { path: 'linked-parent/outside.txt' } } } },
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__write_file', tool_input: { path: 'linked-file.txt', content: 'bypass' } } } },
      { text: 'Forbidden tools and traversal were blocked.' },
    ];
    await api(`/api/conversations/${conversation.id}/messages`, { text: 'Continue and verify the configured confinement.' });
    const continued = await wait(conversation.id, c => c.run.status === 'completed' && c.messages.length === 4);
    assert.equal(continued.sessionId, originalSession);
    assert.equal(continued.messages[1].text, 'The approved write succeeded.');
    assert.equal(continued.messages[3].text, 'Forbidden tools and traversal were blocked.');
    assert.equal(inference[beforeContinuation].messages.some(message => message.role === 'assistant' && String(message.content).includes('The approved write succeeded.')), true);
    assert.equal(await readFile(path.join(root, 'outside.txt'), 'utf8'), 'outside stays unchanged\n');
    for (const request of inference) assert.deepEqual(request.tools!.map(tool => tool.function.name).sort(), ['search_tool', 'todo_write', 'use_tool']);
    const outcomes = inference.slice(beforeContinuation + 1).map(request => request.messages.filter(message => message.role === 'tool').at(-1)?.content).join('\n');
    assert.match(outcomes, /not found|Unknown tool|unknown tool|not available|not supported/i);
    assert.match(outcomes, /denied|permission/i);
    assert.match(outcomes, /traversal/);
    assert.match(outcomes, /Symbolic links/);
    assert.equal(outcomes.includes('outside stays unchanged'), false);
    const configFiles: string[] = [];
    const walk = async (directory: string): Promise<void> => {
      for (const entry of await readdir(directory, { withFileTypes: true })) {
        const filename = path.join(directory, entry.name);
        if (entry.isDirectory()) await walk(filename);
        else if (entry.name === 'config.toml') configFiles.push(filename);
      }
    };
    await walk(path.join(root, 'data', 'grok'));
    assert.equal(configFiles.length, 1);
    const config = await readFile(configFiles[0], 'utf8');
    assert.match(config, /env_key = "GROK_BOT_PROVIDER_KEY"/);
    assert.equal(config.includes('api_key ='), false);
    plan = [
      { tool: { name: 'use_tool', arguments: { tool_name: 'workspace__write_file', tool_input: { path: 'notes.txt', content: 'denied write\n' } } } },
      { text: 'The write was denied.' },
    ];
    await api(`/api/conversations/${conversation.id}/messages`, { text: 'Propose another write so I can deny it.' });
    const denied = await wait(conversation.id, c => c.run.status === 'awaiting_permission');
    const denyPermission = denied.run.permission!;
    await api(`/api/conversations/${conversation.id}/permissions/${denyPermission.id}`, { optionId: denyPermission.options.find(o => o.kind === 'reject_once')!.id });
    await wait(conversation.id, c => c.run.status === 'completed' && c.messages.length === 6);
    assert.equal(await readFile(path.join(workspace, 'notes.txt'), 'utf8'), 'approved by browser\n');
    plan = [{ tool: { name: 'use_tool', arguments: { tool_name: 'workspace__write_file', tool_input: { path: 'notes.txt', content: 'cancelled write\n' } } } }];
    await api(`/api/conversations/${conversation.id}/messages`, { text: 'Propose a write so I can cancel the task.' });
    const cancelledPending = await wait(conversation.id, c => c.run.status === 'awaiting_permission');
    assert.equal((await api(`/api/conversations/${conversation.id}/cancel`, {})).status, 200);
    const cancelled = await wait(conversation.id, c => c.run.status === 'cancelled');
    assert.equal(cancelled.run.permission, undefined);
    assert.equal((await api(`/api/conversations/${conversation.id}/permissions/${cancelledPending.run.permission!.id}`, { optionId: cancelledPending.run.permission!.options[0].id })).status, 409);
    assert.equal(await readFile(path.join(workspace, 'notes.txt'), 'utf8'), 'approved by browser\n');
    const apiKey = 'backend-only-local-fixture-key-78584';
    await api('/api/settings', { baseUrl: provider.baseUrl, model: 'local-test-model', workspace, apiKey }, 'PUT');
    plan = [{ text: 'Authenticated local provider reply.' }];
    await api(`/api/conversations/${conversation.id}/messages`, { text: 'Test provider credential redaction.' });
    const redacted = await wait(conversation.id, c => c.run.status === 'completed' && c.messages.length === 10);
    assert.equal(redacted.messages.at(-1)!.text, 'Authenticated local provider reply.');
    assert.notEqual(redacted.sessionId, originalSession);
    assert.deepEqual(redacted.messages.slice(0, 2), completed.messages);
    const replacement = inference.at(-1)!;
    const replacementPrompt = replacement.messages.filter(message => message.role === 'user').map(message => typeof message.content === 'string' ? message.content : JSON.stringify(message.content)).join('\n');
    assert.match(replacementPrompt, /The approved write succeeded\./);
    assert.ok(replacementPrompt.includes('Current user message:'));
    assert.ok(replacementPrompt.includes('Test provider credential redaction.'));
    assert.equal(replacement.messages.some(message => message.role === 'tool' || message.tool_calls?.length), false);
    assert.equal(JSON.stringify(await state()).includes(apiKey), false);
    const checkRuntimeFiles = async (directory: string): Promise<void> => {
      for (const entry of await readdir(directory, { withFileTypes: true })) {
        const filename = path.join(directory, entry.name);
        if (entry.isDirectory()) await checkRuntimeFiles(filename);
        else if (entry.isFile()) assert.equal((await readFile(filename)).includes(Buffer.from(apiKey)), false, `Runtime file must not persist the provider key: ${entry.name}`);
      }
    };
    await checkRuntimeFiles(path.join(root, 'data', 'grok'));
    const canary = `PRIVATE_CONTENT_${randomUUID()}`;
    const outside = path.join(root, 'private-canary.txt');
    await writeFile(outside, canary);
    const literalPrompts = [
      `@${outside}`,
      `@ ${outside}`,
      `Quoted \`@${outside}\` and (@${outside})`,
      `/mcp install unexpected @${outside}`,
      '[Image #1: /tmp/private-image.png]',
      '\u0085/mcp install unexpected',
      'Keep DATA:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB as literal text',
    ];
    let firstLiteralId = '';
    for (const text of literalPrompts) {
      const previous = inference.length;
      const { conversation: literal } = await (await api('/api/conversations', { botId: backend.store.bots[0].id })).json() as { conversation: Conversation };
      firstLiteralId ||= literal.id;
      plan = [{ text: 'Literal prompt accepted.' }];
      await api(`/api/conversations/${literal.id}/messages`, { text });
      const finished = await wait(literal.id, c => c.run.status === 'completed');
      assert.equal(finished.messages[0].text, text);
      assert.equal(finished.messages[1].text, 'Literal prompt accepted.');
      const requests = inference.slice(previous);
      assert.ok(requests.length >= 1);
      assert.equal(JSON.stringify(requests).includes(canary), false, 'Prompt expansion must never read the external file.');
      const wireText = requests.flatMap(request => request.messages.filter(message => message.role === 'user').map(message => JSON.stringify(message.content))).join('\n');
      assert.equal(wireText.includes('<attached_files>'), false);
      assert.equal(wireText.includes('@'), false);
    }
    await api('/api/settings', { baseUrl: provider.baseUrl, model: 'local-test-model', workspace }, 'PUT');
    plan = [{ text: 'History stayed literal.' }];
    const beforeReplay = inference.length;
    const literalConversation = backend.store.conversation(firstLiteralId);
    await api(`/api/conversations/${literalConversation.id}/messages`, { text: 'Continue after engine context replacement' });
    await wait(literalConversation.id, c => c.run.status === 'completed' && c.messages.length === 4);
    assert.equal(JSON.stringify(inference.slice(beforeReplay)).includes(canary), false);
    assert.equal(await readFile(outside, 'utf8'), canary);
  } finally {
    await backend.close();
    await provider.close();
    await rm(root, { recursive: true, force: true });
  }
});
