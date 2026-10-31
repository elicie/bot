import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { Store } from '../server/store.js';

test('state revisions survive restart and subscribers see committed disk snapshots', async () => {
  const root = await mkdtemp(join(tmpdir(), 'grok-state-order-'));
  try {
    const store = await Store.open(root);
    const observations: Array<Promise<void>> = [];
    const unsubscribe = store.subscribe(snapshot => {
      observations.push(readFile(join(root, 'state.json'), 'utf8').then(bytes => {
        const persisted = JSON.parse(bytes);
        assert.ok(persisted.revision >= snapshot.revision);
        assert.ok(persisted.bots.some((bot: { name: string }) => bot.name === snapshot.bots.at(-1)!.name));
      }));
    });
    const before = store.snapshot().revision;
    await store.addBot({ name: 'Persisted bot', color: '#75a9c0', instructions: '' });
    assert.ok(store.snapshot().revision > before);
    await Promise.all(observations);
    unsubscribe();
    const previous = store.snapshot().revision;
    await store.flush();
    const restarted = await Store.open(root);
    assert.ok(restarted.snapshot().revision > previous);
    assert.equal(restarted.snapshot().bots.at(-1)?.name, 'Persisted bot');
    await restarted.flush();
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
