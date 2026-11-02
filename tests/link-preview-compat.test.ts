import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Store } from '../server/store.js';

test('legacy store preserves the optional Rust preview preference without advertising unsupported endpoints', async () => {
  const root = await mkdtemp(join(tmpdir(), 'grok-preview-compat-'));
  try {
    const initial = await Store.open(root);
    await initial.flush();
    const path = join(root, 'state.json');
    const disk = JSON.parse(await readFile(path, 'utf8'));
    assert.equal(Object.hasOwn(disk, 'linkPreviewsEnabled'), false);
    await writeFile(path, JSON.stringify({ ...disk, linkPreviewsEnabled: true }), { mode: 0o600 });
    const store = await Store.open(root);
    assert.equal(Object.hasOwn(store.snapshot(), 'linkPreviewsEnabled'), false);
    await store.addBot({ name: 'Preserved setting', color: '#75a9c0', instructions: '' });
    await store.flush();
    assert.equal(JSON.parse(await readFile(path, 'utf8')).linkPreviewsEnabled, true);
    await writeFile(path, JSON.stringify({ ...disk, linkPreviewsEnabled: 'true' }), { mode: 0o600 });
    await assert.rejects(Store.open(root), /Cannot load application state/);
  } finally {
    // mkdtemp root belongs only to this fixture, not an application data directory.
    assert.equal(root.startsWith(join(tmpdir(), 'grok-preview-compat-')), true);
    await rm(root, { recursive: true, force: true });
  }
});
