import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir, networkInterfaces } from 'node:os';
import { join } from 'node:path';
import type { AddressInfo } from 'node:net';
import { randomUUID } from 'node:crypto';
import { createBackend } from '../server/app.js';

async function readEvent(reader: ReadableStreamDefaultReader<Uint8Array>, matches: (value: string) => boolean): Promise<string> {
  const read = async () => {
    let value = '';
    while (true) {
      const item = await reader.read();
      if (item.done) throw Error('SSE stream closed');
      value += Buffer.from(item.value).toString();
      if (matches(value)) return value;
    }
  };
  let timer: NodeJS.Timeout;
  return Promise.race([read(), new Promise<string>((_, reject) => { timer = setTimeout(() => reject(Error('SSE event timed out')), 5000); })]).finally(() => clearTimeout(timer));
}

test('LAN state, mutation responses, and already-open SSE hide local computer approval and tool details', async () => {
  const address = Object.values(networkInterfaces()).flat().find(item => item?.family === 'IPv4' && !item.internal)?.address;
  assert.ok(address, 'This locality test requires a network interface');
  const root = await mkdtemp(join(tmpdir(), 'grok-computer-privacy-'));
  const backend = await createBackend({ dataDir: root, serveWeb: false, allowedHosts: [address], routines: { autoStart: false } });
  await new Promise<void>(resolve => backend.server.listen(0, '0.0.0.0', resolve));
  const port = (backend.server.address() as AddressInfo).port;
  const lan = `http://${address}:${port}`;
  const local = `http://127.0.0.1:${port}`;
  const controller = new AbortController();
  let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
  try {
    const conversation = await backend.store.addConversation(backend.store.bots[0].id);
    const response = await fetch(lan + '/api/events', { signal: controller.signal });
    reader = response.body!.getReader();
    await readEvent(reader, value => value.includes('event: state'));
    const canary = 'LOCAL_COMPUTER_QUERY_AND_ACTION_SECRET';
    conversation.run = { id: randomUUID(), status: 'awaiting_permission', permission: { id: randomUUID(), title: 'Observe managed browser', kind: 'other', description: `https://public.example/?token=${canary}#${canary}`, paths: [], localOnly: true, options: [{ id: 'once', label: 'Allow once', kind: 'allow_once' }] } };
    conversation.messages.push({ id: randomUUID(), role: 'assistant', text: '', createdAt: new Date().toISOString(), tools: [{ id: randomUUID(), kind: 'other', status: 'pending', title: 'workspace__computer_act', input: canary, output: canary, localOnly: true }] });
    const next = readEvent(reader, value => value.includes('localOnly'));
    await backend.store.changed();
    assert.equal((await next).includes(canary), false);
    const lanState = await (await fetch(lan + '/api/state')).text();
    assert.equal(lanState.includes(canary), false);
    assert.match(lanState, /localhost/);
    const localState = await (await fetch(local + '/api/state')).text();
    assert.equal(localState.includes(canary), true);
    const options = { method: 'PATCH', headers: { Origin: lan, 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: JSON.stringify({ pinned: true }) };
    conversation.run.status = 'completed';
    const mutation = await fetch(lan + '/api/bots/' + backend.store.bots[0].id, options);
    assert.equal(mutation.status, 200);
    assert.equal((await mutation.text()).includes(canary), false);
    conversation.run.permission!.localOnly = false;
    await backend.store.changed();
    const fileApproval = await (await fetch(lan + '/api/state')).text();
    assert.equal(fileApproval.includes(canary), true);
  } finally {
    controller.abort();
    await reader?.cancel().catch(() => {});
    await backend.close();
    await rm(root, { recursive: true, force: true });
  }
});
