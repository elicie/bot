import assert from 'node:assert/strict';
import { test } from 'node:test';
import { randomUUID } from 'node:crypto';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { Request } from 'express';
import { ApplicationComputer, isLocalComputerRequest } from '../server/computer-api.js';
import { Store } from '../server/store.js';

async function fixture() {
  const root = await mkdtemp(join(tmpdir(), 'grok-computer-api-'));
  const store = await Store.open(root);
  const computer = new ApplicationComputer(store);
  return { store, computer, close: async () => { await computer.stop(); await rm(root, { recursive: true, force: true }); } };
}

test('computer HTTP locality follows the socket and ignores forwarded-address headers', () => {
  for (const address of ['127.0.0.1', '::1', '::ffff:127.0.0.1']) {
    assert.equal(isLocalComputerRequest({ socket: { remoteAddress: address }, headers: { 'x-forwarded-for': '10.0.0.2' } } as unknown as Request), true);
  }
  for (const address of ['10.0.0.2', '100.64.0.2', '192.168.1.2', undefined]) {
    assert.equal(isLocalComputerRequest({ socket: { remoteAddress: address }, headers: { 'x-forwarded-for': '127.0.0.1' } } as unknown as Request), false);
  }
});

test('stopping the application computer invalidates starts and actions already queued before stop', async () => {
  const f = await fixture();
  try {
    let launched = 0;
    let performed = 0;
    f.computer.service.start = async () => { launched++; throw Error('A stopped request reached the service'); };
    f.computer.service.perform = async () => { performed++; throw Error('A stopped action reached the service'); };
    const owner = f.store.bots[0].id;
    const start = f.computer.start(owner, { requestId: randomUUID(), userInitiated: true, origins: ['https://example.com'] });
    const action = f.computer.perform(owner, { requestId: randomUUID(), epoch: randomUUID(), frameId: null, action: { type: 'observe' } });
    const failures = [assert.rejects(start, /stopped or replaced/), assert.rejects(action, /stopped or replaced/)];
    await f.computer.stop(owner);
    await Promise.all(failures);
    assert.equal(launched, 0);
    assert.equal(performed, 0);
  } finally { await f.close(); }
});

test('a command admitted before stop cannot start a browser after the command queue drains', async () => {
  const f = await fixture();
  try {
    const owner = f.store.bots[0].id;
    const generation = f.computer.generation;
    await f.computer.stop(owner);
    await assert.rejects(f.computer.start(owner, { requestId: randomUUID(), userInitiated: true, origins: ['https://example.com'] }, generation), /stopped or replaced/);
    assert.equal(f.computer.status(owner).state, 'stopped');
  } finally { await f.close(); }
});

test('a replacement owner stays bound when an older stop completes after a new start', async () => {
  const f = await fixture();
  try {
    const other = await f.store.addBot({ name: 'Other', color: '#75a9c0', instructions: '' });
    const owner = f.store.bots[0].id;
    let status = f.computer.service.status();
    f.computer.service.status = () => structuredClone(status);
    f.computer.service.start = async () => {
      status = { ...status, state: 'running', epoch: randomUUID(), frameId: randomUUID(), sandboxed: true };
      return status;
    };
    f.computer.service.screenshot = async () => ({ status, frame: { id: status.frameId!, epoch: status.epoch, url: 'about:blank', title: 'Fixture', viewport: status.viewport, controls: [], mimeType: 'image/png' }, screenshotBuffer: Buffer.from('fixture PNG') });
    const originalStop = f.computer.service.stop.bind(f.computer.service);
    await f.computer.start(owner, { requestId: randomUUID(), userInitiated: true, origins: ['https://example.com'] });
    let release!: () => void;
    const cleanup = new Promise<void>(resolve => { release = resolve; });
    f.computer.service.stop = async () => { status = { ...status, state: 'stopped', frameId: null }; await cleanup; return status; };
    const stopping = f.computer.stop(owner);
    await f.computer.start(other.id, { requestId: randomUUID(), userInitiated: true, origins: ['https://example.com'] });
    release();
    await stopping;
    assert.throws(() => f.computer.status(owner), /Another bot owns/);
    assert.throws(() => f.computer.preview(owner, status.epoch, status.frameId!), /Another bot owns/);
    assert.equal(f.computer.status(other.id).state, 'running');
    assert.equal(f.computer.preview(other.id, status.epoch, status.frameId!).toString(), 'fixture PNG');
    assert.throws(() => f.computer.preview(other.id, status.epoch, randomUUID()), /stale/);
    f.computer.service.stop = originalStop;
  } finally { await f.close(); }
});
