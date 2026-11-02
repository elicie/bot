import assert from 'node:assert/strict';
import test from 'node:test';
import { createHash, randomUUID } from 'node:crypto';
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import type { AddressInfo } from 'node:net';
import type { AppState, Canvas, CanvasInput, Conversation } from '../shared/types.js';
import { createBackend } from '../server/app.js';
import { canvasInputSchema, canvasRangeSchema, canvasReadSchema, canvasRestoreSchema, canvasSchema, MAX_CANVAS_CONTENT_BYTES, MAX_CANVAS_READ_RESPONSE_BYTES, prepareCanvasRange, publicCanvas, readCanvas, replaceCanvasRange, restoreCanvas, saveCanvas, type CanvasRead, type CanvasReadInput } from '../server/canvas.js';
import { HttpError } from '../server/files.js';
import { skillResponseText } from '../server/skills.js';
import { Store } from '../server/store.js';

const timestamp = '2026-01-01T00:00:00.000Z';
const input = (revision = 0, content = 'Initial text\r\n😀 end', title = 'Notes', language = 'text'): CanvasInput => ({ revision, title, language, content });
const status = (expected: number) => (error: unknown) => error instanceof HttpError && error.status === expected;
const sizedContent = (unit: string, bytes = MAX_CANVAS_CONTENT_BYTES) => {
  const count = Math.floor(bytes / Buffer.byteLength(unit, 'utf8'));
  return unit.repeat(count) + 'x'.repeat(bytes - count * Buffer.byteLength(unit, 'utf8'));
};
const mixedContent = () => sizedContent('😀α尾\r\n\t\b\f\u0001"\\DaTa:text/plain,literal;data:image/png;base64,not-an-image\n');
const continuation = (page: CanvasRead, offset = page.nextOffset!): CanvasReadInput => ({ offset, revision: page.revision, viewHash: page.viewHash });
const assertReadText = (text: string, expected: CanvasRead) => {
  assert.ok(Buffer.byteLength(text, 'utf8') <= 18 * 1024, `Read response has ${Buffer.byteLength(text, 'utf8')} bytes.`);
  assert.doesNotMatch(text, /data:/i);
  assert.deepEqual(JSON.parse(text), { canvas: expected });
};
async function temporary() {
  const root = await mkdtemp(path.join(tmpdir(), 'grok-canvas-'));
  return { root, close: async () => {
    assert.equal(path.dirname(root), path.resolve(tmpdir()));
    assert.ok(path.basename(root).startsWith('grok-canvas-'));
    await rm(root, { recursive: true, force: true });
  } };
}

test('canvas schemas enforce JS-unit titles, UTF-8 text limits, strict safe revisions, and private history invariants', () => {
  assert.equal(canvasInputSchema.parse(input(0, '', '  Notes  ')).title, 'Notes');
  assert.equal(canvasInputSchema.safeParse(input(0, '', '😀'.repeat(50))).success, true);
  assert.equal(canvasInputSchema.safeParse(input(0, '', '😀'.repeat(51))).success, false);
  assert.equal(canvasInputSchema.safeParse(input(0, '😀'.repeat(MAX_CANVAS_CONTENT_BYTES / 4))).success, true);
  assert.equal(canvasInputSchema.safeParse(input(0, 'x'.repeat(MAX_CANVAS_CONTENT_BYTES))).success, true);
  for (const bad of [
    input(-1), input(0.1), input(Number.MAX_SAFE_INTEGER + 1), input(Infinity),
    input(0, 'x\0y'), input(0, '😀'.repeat(MAX_CANVAS_CONTENT_BYTES / 4) + 'x'),
    input(0, '', ' '), input(0, '', 'x'.repeat(101)), input(0, '', 'title\0'),
    input(0, '\ud83d'), input(0, '\ude00'), input(0, '', '\ud83d'), input(0, '', '\ude00'),
    ...['', 'Text', 'c++', '1text', 'a'.repeat(33), 'x_y', 'a\n'].map(language => input(0, '', 'Title', language)),
    { ...input(), conversationId: randomUUID() },
  ]) assert.equal(canvasInputSchema.safeParse(bad).success, false, JSON.stringify(bad).slice(0, 200));
  assert.equal(canvasInputSchema.safeParse(input(Number.MAX_SAFE_INTEGER, '', 'Title', 'a0-b')).success, true);
  for (const revision of [-1, 0, 0.5, Number.MAX_SAFE_INTEGER + 1]) {
    assert.equal(canvasRestoreSchema.safeParse({ revision, targetRevision: 1 }).success, false);
    assert.equal(canvasRangeSchema.safeParse({ revision, oldText: 'x', replacement: '' }).success, false);
  }
  assert.equal(canvasRestoreSchema.safeParse({ revision: 1, targetRevision: 0 }).success, false);
  const first = saveCanvas(undefined, input(), timestamp);
  const second = saveCanvas(first, input(1, 'Changed'), timestamp);
  assert.deepEqual(canvasSchema.parse({ ...first, history: undefined }).history, []);
  assert.equal(canvasSchema.safeParse({ ...second, redacted: false, history: [{ ...second.history[0], redacted: false }] }).success, true);
  for (const bad of [
    { ...first, revision: 0 }, { ...first, redacted: true }, { ...first, title: ' not canonical ' },
    { ...first, content: '\ud83d' }, { ...first, title: '\ude00' },
    { ...second, history: [{ ...second.history[0], redacted: true }] },
    { ...second, history: [{ ...second.history[0], revision: 2 }] },
    { ...second, history: [second.history[0], second.history[0]] },
    { ...second, revision: 3, history: [second.history[0]] },
    { ...second, history: [{ ...second.history[0], content: '\ud83d' }] },
    { ...second, history: [{ ...second.history[0], title: ' not canonical ' }] },
    { ...second, history: Array.from({ length: 21 }, () => second.history[0]) },
    { ...first, updatedAt: 'invalid' }, { ...first, source: 'external' }, { ...first, unexpected: true },
  ]) assert.equal(canvasSchema.safeParse(bad).success, false);
});

test('canvas read schema accepts only optional safe offsets, positive revisions, and lowercase view hashes', () => {
  assert.deepEqual(canvasReadSchema.parse({}), {});
  assert.equal(canvasReadSchema.safeParse({ offset: Number.MAX_SAFE_INTEGER, revision: Number.MAX_SAFE_INTEGER, viewHash: 'a'.repeat(64) }).success, true);
  for (const bad of [
    ...[-1, 0.5, Number.MAX_SAFE_INTEGER + 1, Infinity, NaN, '0', null].map(offset => ({ offset })),
    ...[0, -1, 0.5, Number.MAX_SAFE_INTEGER + 1, Infinity, '1'].map(revision => ({ revision })),
    ...['a'.repeat(63), 'a'.repeat(65), 'A'.repeat(64), 'g'.repeat(64), ''].map(viewHash => ({ viewHash })),
    { conversationId: randomUUID() }, { offset: 0, unexpected: true },
  ]) assert.equal(canvasReadSchema.safeParse(bad).success, false);
});

test('canvas reads reconstruct complete 128 KiB Unicode/control/data text from bounded literal-escaped JSON pages without exposing historical fields', () => {
  const content = mixedContent();
  assert.equal(Buffer.byteLength(content, 'utf8'), MAX_CANVAS_CONTENT_BYTES);
  let canvas = saveCanvas(undefined, input(0, 'HISTORICAL_TEXT_CANARY', 'OLD_TITLE_CANARY', 'old-language'), timestamp);
  for (let revision = 1; revision < 22; revision++) canvas = saveCanvas(canvas, input(revision, `HISTORICAL_TEXT_CANARY_${revision}`, 'OLD_TITLE_CANARY', 'old-language'), timestamp);
  canvas = saveCanvas(canvas, input(canvas.revision, content, 'DaTa:"\t😀', 'markdown'), timestamp);
  const before = structuredClone(canvas);
  const chunks: string[] = [];
  let request: CanvasReadInput = {};
  let first: CanvasRead | undefined;
  let last: CanvasRead;
  do {
    const page = readCanvas(canvas, '', request)!;
    first ??= page;
    assertReadText(skillResponseText({ canvas: page }), page);
    assert.equal(page.offset, request.offset ?? 0);
    assert.equal(page.totalBytes, MAX_CANVAS_CONTENT_BYTES);
    assert.equal(page.revision, first.revision);
    assert.equal(page.viewHash, first.viewHash);
    assert.equal(page.viewHash, createHash('sha256').update(JSON.stringify([canvas.title, canvas.language, content, false])).digest('hex'));
    assert.equal(page.history.length, 20);
    assert.ok(page.history.every(version => Object.keys(version).sort().join(',') === 'revision,source,updatedAt'));
    assert.doesNotMatch(skillResponseText({ canvas: page }), /HISTORICAL_TEXT_CANARY|OLD_TITLE_CANARY|old-language/);
    const end = page.nextOffset ?? page.totalBytes;
    assert.equal(Buffer.byteLength(page.content, 'utf8'), end - page.offset);
    assert.equal(page.content, Buffer.from(content, 'utf8').toString('utf8', page.offset, end));
    if (page.nextOffset !== undefined) {
      assert.ok(page.content.length > 0);
      assert.ok(page.nextOffset > page.offset && page.nextOffset < page.totalBytes);
    }
    chunks.push(page.content);
    last = page;
    request = continuation(page);
    assert.ok(chunks.length < 1000, 'Each page must make progress.');
  } while (last.nextOffset !== undefined);
  assert.ok(chunks.length > 1);
  assert.equal(chunks.join(''), content);
  const eof = readCanvas(canvas, '', continuation(first!, first!.totalBytes))!;
  assert.equal(eof.content, '');
  assert.equal(eof.offset, MAX_CANVAS_CONTENT_BYTES);
  assert.equal(Object.hasOwn(eof, 'nextOffset'), false);
  assertReadText(skillResponseText({ canvas: eof }), eof);
  assert.deepEqual(canvas, before);
  assert.equal(MAX_CANVAS_READ_RESPONSE_BYTES, 18 * 1024);
});

test('canvas pages budget metadata, escaping and redaction expansion and return full content only when the complete response fits', () => {
  for (const [content, secret] of [
    ['Short 😀\r\nDaTa:text/plain,\\literal"\u0001', ''],
    ['', ''],
    [sizedContent('\u0001\b\f\r\n\t"\\'), ''],
    [sizedContent('😀尾α'), ''],
    [sizedContent('dAtA:image/png;base64,literal;data:text/plain,x\n'), ''],
    ['q'.repeat(MAX_CANVAS_CONTENT_BYTES), 'q'],
  ]) {
    const canvas = saveCanvas(undefined, input(0, content, 'Title 😀\t"DaTa:', 'text'), timestamp);
    const projected = publicCanvas(canvas, secret);
    let request: CanvasReadInput = {};
    const chunks: string[] = [];
    let page: CanvasRead;
    do {
      page = readCanvas(canvas, secret, request)!;
      assertReadText(skillResponseText({ canvas: page }), page);
      assert.equal(page.totalBytes, Buffer.byteLength(projected.content, 'utf8'));
      assert.equal(page.title, projected.title);
      assert.equal(page.redacted, projected.redacted);
      assert.equal(page.offset, request.offset ?? 0);
      assert.equal(Buffer.byteLength(page.content, 'utf8'), (page.nextOffset ?? page.totalBytes) - page.offset);
      if (page.nextOffset !== undefined) assert.ok(page.content.length > 0 && page.nextOffset > page.offset);
      chunks.push(page.content);
      request = continuation(page);
      assert.ok(chunks.length < 1000);
    } while (page.nextOffset !== undefined);
    assert.equal(chunks.join(''), projected.content);
    assert.equal(canvas.content, content);
    if (Buffer.byteLength(content, 'utf8') < 100) {
      assert.equal(chunks.length, 1);
      assert.equal(Object.hasOwn(page, 'nextOffset'), false);
    } else assert.ok(chunks.length > 1);
  }
});

test('canvas offsets reject invalid UTF-8 boundaries and require both current revision and public view for continuation', () => {
  const canvas = saveCanvas(undefined, input(0, 'A😀β尾\r\nZ'), timestamp);
  const first = readCanvas(canvas)!;
  for (const offset of [2, 3, 4, 6, 8, 9]) assert.throws(() => readCanvas(canvas, '', continuation(first, offset)), status(400));
  for (const offset of [0, 1, 5, 7, 10, 11, 12, 13]) {
    const page = readCanvas(canvas, '', continuation(first, offset))!;
    assert.equal(page.content, Buffer.from(canvas.content, 'utf8').toString('utf8', offset));
    assertReadText(skillResponseText({ canvas: page }), page);
  }
  assert.throws(() => readCanvas(canvas, '', continuation(first, first.totalBytes + 1)), status(400));
  for (const request of [{ offset: 1 }, { offset: 1, revision: 1 }, { offset: 1, viewHash: first.viewHash }]) {
    assert.throws(() => readCanvas(canvas, '', request), status(400));
  }
  assert.throws(() => readCanvas(canvas, '', { revision: 2 }), status(409));
  assert.throws(() => readCanvas(canvas, '', { viewHash: 'f'.repeat(64) }), status(409));
  const changed = saveCanvas(canvas, input(1, 'Changed'), timestamp);
  assert.throws(() => readCanvas(changed, '', continuation(first, 1)), status(409));
  assert.throws(() => readCanvas(undefined, '', continuation(first, 1)), status(409));
  assert.throws(() => readCanvas(undefined, '', { viewHash: first.viewHash }), status(409));
  assert.equal(readCanvas(undefined), null);
  const masked = readCanvas(canvas, 'A')!;
  assert.equal(masked.revision, first.revision);
  assert.notEqual(masked.viewHash, first.viewHash);
  assert.throws(() => readCanvas(canvas, 'A', continuation(first, 1)), status(409));
  assert.throws(() => readCanvas(canvas, '', continuation(masked, 1)), status(409));
  const clean = saveCanvas(canvas, input(1, 'Unchanged content'), timestamp);
  const cleanRead = readCanvas(clean)!;
  for (const field of ['title', 'language'] as const) {
    const altered = { ...clean, [field]: `${clean[field]}a` };
    assert.throws(() => readCanvas(altered, '', continuation(cleanRead, 1)), status(409));
  }
});

test('canvas saves use CAS, no-ops retain identity, restore creates a new version, and revisions cannot overflow', () => {
  assert.throws(() => saveCanvas(undefined, input(1), timestamp), status(409));
  const first = saveCanvas(undefined, input(), timestamp);
  assert.equal(first.revision, 1);
  assert.equal(first.source, 'user');
  assert.deepEqual(first.history, []);
  assert.equal(saveCanvas(first, input(1), '2026-02-02T00:00:00.000Z'), first);
  assert.throws(() => saveCanvas(first, input(0), timestamp), status(409));
  const second = saveCanvas(first, input(1, 'Second', 'Revised', 'markdown'), timestamp);
  const restored = restoreCanvas(second, { revision: 2, targetRevision: 1 }, timestamp);
  assert.equal(restored.revision, 3);
  assert.equal(restored.source, 'restore');
  assert.equal(restored.content, first.content);
  assert.deepEqual(restored.history.map(version => version.revision), [1, 2]);
  assert.equal(Object.hasOwn(restored.history[1], 'history'), false);
  assert.throws(() => restoreCanvas(second, { revision: 1, targetRevision: 1 }, timestamp), status(409));
  assert.throws(() => restoreCanvas(second, { revision: 2, targetRevision: 2 }, timestamp), status(404));
  const exhausted = { ...first, revision: Number.MAX_SAFE_INTEGER };
  assert.throws(() => saveCanvas(exhausted, input(Number.MAX_SAFE_INTEGER, 'Different'), timestamp), status(409));
  assert.equal(saveCanvas(exhausted, input(Number.MAX_SAFE_INTEGER), timestamp), exhausted);
});

test('range proposals preserve raw Unicode, CRLF and literal replacements, reject ambiguous overlaps, and recheck immutable results', () => {
  const first = saveCanvas(undefined, input(0, 'α😀\r\nunique\r\n尾😀'), timestamp);
  const proposal = prepareCanvasRange(first, { revision: 1, oldText: '😀\r\nunique', replacement: '$&\r\n🐈' });
  assert.equal(Object.isFrozen(proposal), true);
  assert.equal(proposal.content, 'α$&\r\n🐈\r\n尾😀');
  const next = replaceCanvasRange(first, proposal, timestamp);
  assert.equal(next.content, proposal.content);
  assert.equal(next.source, 'bot');
  assert.equal(first.content, 'α😀\r\nunique\r\n尾😀');
  assert.equal(replaceCanvasRange(first, prepareCanvasRange(first, { revision: 1, oldText: 'unique', replacement: '' }), timestamp).content, 'α😀\r\n\r\n尾😀');
  assert.throws(() => prepareCanvasRange(first, { revision: 2, oldText: 'unique', replacement: '' }), status(409));
  assert.throws(() => prepareCanvasRange(first, { revision: 1, oldText: 'absent', replacement: '' }), /absent/);
  assert.throws(() => prepareCanvasRange({ ...first, content: 'aaa' }, { revision: 1, oldText: 'aa', replacement: '' }), /ambiguous/);
  for (const oldText of ['', '\0', '\ud83d', '\ude00']) assert.throws(() => prepareCanvasRange(first, { revision: 1, oldText, replacement: '' }));
  for (const replacement of ['\0', '\ud83d', '\ude00']) assert.throws(() => prepareCanvasRange(first, { revision: 1, oldText: 'unique', replacement }));
  assert.throws(() => prepareCanvasRange(first, { revision: 1, oldText: 'unique', replacement: 'x'.repeat(MAX_CANVAS_CONTENT_BYTES) }));
  assert.throws(() => replaceCanvasRange({ ...first, content: `${first.content}!` }, proposal, timestamp), /changed/);
  assert.throws(() => replaceCanvasRange({ ...first, content: `${first.content}😀\r\nunique` }, proposal, timestamp), /ambiguous/);
});

test('redaction projects current and historical metadata once and only matching current or target text is read-only', () => {
  const first = saveCanvas(undefined, input(0, 'old a secret', 'a', 'java'), timestamp);
  const second = saveCanvas(first, input(1, 'next', 'xyz', 'cpp'), timestamp);
  const projected = publicCanvas(second, 'a');
  assert.equal(projected.redacted, undefined);
  assert.equal(projected.history[0].redacted, true);
  assert.equal(projected.history[0].content, 'old [redacted] secret');
  assert.equal(projected.history[0].title, '[redacted]');
  assert.equal(projected.history[0].language, 'j[redacted]v[redacted]');
  assert.equal(projected.history[0].updatedAt, timestamp);
  assert.equal(projected.history[0].source, 'user');
  assert.equal(Object.hasOwn(readCanvas(second, 'a')!.history[0], 'content'), false);
  assert.equal(readCanvas(undefined), null);
  assert.throws(() => restoreCanvas(second, { revision: 2, targetRevision: 1 }, timestamp, 'a'), /read-only/);
  assert.equal(saveCanvas(second, input(2, 'new', 'xyz', 'cpp'), timestamp, 'a').revision, 3);
  for (const bad of [input(2, 'a', 'xyz', 'cpp'), input(2, 'new', 'a', 'cpp'), input(2, 'new', 'xyz', 'java')]) {
    assert.throws(() => saveCanvas(second, bad, timestamp, 'a'), /read-only/);
  }
  const current = { ...second, content: 'a', source: 'restore' as const };
  assert.throws(() => saveCanvas(current, input(2, 'new', 'xyz', 'cpp'), timestamp, 'a'), /read-only/);
  assert.throws(() => prepareCanvasRange(current, { revision: 2, oldText: 'a', replacement: '' }, 'a'), /read-only/);
  assert.equal(publicCanvas(current, 'a').source, 'restore');
  assert.equal(publicCanvas(current, 'a').content, '[redacted]');
  assert.equal(publicCanvas(current, 'a').redacted, true);
  assert.equal(saveCanvas(current, input(2, 'new', 'xyz', 'cpp'), timestamp, 'different-secret').revision, 3);
});

test('store canvas CAS is atomic across concurrent callers, bounded history survives restart, and undo never rewinds revision', async () => {
  const f = await temporary();
  let clock = 0;
  try {
    const store = await Store.open(f.root, { now: () => new Date(Date.parse(timestamp) + clock++ * 1000) });
    const conversation = await store.addConversation(store.bots[0].id);
    assert.equal(store.readCanvas(conversation.id), null);
    const results = await Promise.allSettled([
      store.updateCanvas(conversation.id, input(0, 'one')),
      store.updateCanvas(conversation.id, input(0, 'two')),
    ]);
    assert.deepEqual(results.map(result => result.status), ['fulfilled', 'rejected']);
    assert.equal(conversation.canvas!.revision, 1);
    assert.equal(conversation.canvas!.content, 'one');
    const revision = store.snapshot().revision;
    const priorTimestamp = conversation.updatedAt;
    const original = conversation.canvas;
    await store.updateCanvas(conversation.id, input(1, 'one'));
    assert.equal(store.snapshot().revision, revision);
    assert.equal(conversation.updatedAt, priorTimestamp);
    assert.equal(conversation.canvas, original);
    conversation.run = { id: randomUUID(), status: 'running' };
    for (let expected = 1; expected < 26; expected++) await store.updateCanvas(conversation.id, input(expected, `text-${expected + 1}`));
    assert.equal(conversation.canvas!.revision, 26);
    assert.deepEqual(conversation.canvas!.history.map(version => version.revision), Array.from({ length: 20 }, (_, index) => index + 6));
    const second = await store.addConversation(store.bots[0].id);
    await store.updateCanvas(second.id, input(0, 'Independent while another conversation runs'));
    await store.restoreCanvas(conversation.id, { revision: 26, targetRevision: 7 });
    assert.equal(conversation.canvas!.revision, 27);
    assert.equal(conversation.canvas!.content, 'text-7');
    assert.equal(conversation.canvas!.source, 'restore');
    assert.deepEqual(conversation.canvas!.history.map(version => version.revision), Array.from({ length: 20 }, (_, index) => index + 7));
    await store.restoreCanvas(conversation.id, { revision: 27, targetRevision: 26 });
    assert.equal(conversation.canvas!.revision, 28);
    assert.equal(conversation.canvas!.content, 'text-26');
    await assert.rejects(store.restoreCanvas(conversation.id, { revision: 28, targetRevision: 1 }), status(404));
    assert.equal(store.readCanvas(conversation.id)!.history.every(version => !Object.hasOwn(version, 'content')), true);
    await store.flush();
    const reopened = await Store.open(f.root);
    assert.deepEqual(reopened.conversation(conversation.id).canvas, conversation.canvas);
    assert.equal(reopened.conversation(conversation.id).run.status, 'interrupted');
    assert.equal(reopened.conversation(second.id).canvas!.content, second.canvas!.content);
  } finally { await f.close(); }
});

test('store read continuations are pure, use public byte offsets, and reject secret masking changes at the same raw revision', async () => {
  const f = await temporary();
  try {
    const store = await Store.open(f.root);
    const conversation = await store.addConversation(store.bots[0].id);
    await store.updateCanvas(conversation.id, input(0, 'old q secret', 'old q title', 'sql'));
    await store.updateCanvas(conversation.id, input(1, `q😀${mixedContent().slice(4)}`, 'q title', 'text'));
    const settings = { baseUrl: 'http://127.0.0.1:12345/v1', model: 'fixture', workspace: '' };
    await store.updateSettings({ ...settings, apiKey: 'q' });
    const revision = store.snapshot().revision;
    const current = conversation.canvas;
    const updatedAt = conversation.updatedAt;
    let emissions = 0;
    const unsubscribe = store.subscribe(() => { emissions++; });
    const first = store.readCanvas(conversation.id)!;
    assert.ok(first.nextOffset);
    assert.equal(first.redacted, true);
    assert.deepEqual(first.history, [{ revision: 1, source: 'user', updatedAt: current!.history[0].updatedAt, redacted: true }]);
    const next = store.readCanvas(conversation.id, continuation(first))!;
    assert.equal(next.offset, first.nextOffset);
    assert.equal(next.revision, first.revision);
    assert.equal(next.viewHash, first.viewHash);
    assertReadText(skillResponseText({ canvas: first }), first);
    assertReadText(skillResponseText({ canvas: next }), next);
    assert.equal(store.snapshot().revision, revision);
    assert.equal(conversation.canvas, current);
    assert.equal(conversation.updatedAt, updatedAt);
    assert.equal(emissions, 0);
    unsubscribe();
    await store.updateSettings({ ...settings, clearApiKey: true });
    assert.equal(conversation.canvas!.revision, first.revision);
    assert.throws(() => store.readCanvas(conversation.id, continuation(first)), status(409));
    const unmasked = store.readCanvas(conversation.id)!;
    assert.notEqual(unmasked.viewHash, first.viewHash);
    assert.equal(unmasked.totalBytes, Buffer.byteLength(conversation.canvas!.content, 'utf8'));
    await store.updateSettings({ ...settings, apiKey: 'q' });
    assert.throws(() => store.readCanvas(conversation.id, continuation(unmasked)), status(409));
    assert.equal(store.readCanvas(conversation.id)!.viewHash, first.viewHash);
  } finally { await f.close(); }
});

test('store capacity preflight leaves canvas, timestamps, revisions and subscribers unchanged and does not poison persistence', { timeout: 30000 }, async () => {
  const f = await temporary();
  try {
    const store = await Store.open(f.root);
    const conversation = await store.addConversation(store.bots[0].id);
    await store.updateCanvas(conversation.id, input(0, 'x'.repeat(MAX_CANVAS_CONTENT_BYTES)));
    await store.updateCanvas(conversation.id, input(1, 'y'.repeat(MAX_CANVAS_CONTENT_BYTES)));
    const message = { id: randomUUID(), role: 'user' as const, text: '', createdAt: timestamp, tools: [] };
    conversation.messages.push(message);
    await store.changed();
    const file = path.join(f.root, 'state.json');
    const bytes = Buffer.byteLength(await readFile(file, 'utf8'));
    const before = conversation.canvas;
    const updatedAt = conversation.updatedAt;
    const revision = store.snapshot().revision;
    message.text = 'z'.repeat(64 * 1024 * 1024 - bytes - 2048);
    let emitted = 0;
    const unsubscribe = store.subscribe(() => { emitted++; });
    await assert.rejects(store.updateCanvas(conversation.id, input(2, 'z'.repeat(MAX_CANVAS_CONTENT_BYTES))), /64 MiB/);
    await assert.rejects(store.restoreCanvas(conversation.id, { revision: 2, targetRevision: 1 }), /64 MiB/);
    assert.equal(conversation.canvas, before);
    assert.equal(conversation.updatedAt, updatedAt);
    message.text = '';
    assert.equal(store.snapshot().revision, revision);
    assert.equal(emitted, 0);
    store.assertWritable();
    unsubscribe();
    await store.changed();
    await store.restoreCanvas(conversation.id, { revision: 2, targetRevision: 1 });
    assert.equal(conversation.canvas!.revision, 3);
    assert.equal(conversation.canvas!.content, 'x'.repeat(MAX_CANVAS_CONTENT_BYTES));
    assert.equal((await Store.open(f.root)).conversation(conversation.id).canvas!.revision, 3);
  } finally { await f.close(); }
});

test('persisted canvas defaults migrate and malformed private versions are rejected without replacing the state file', async () => {
  const f = await temporary();
  try {
    const store = await Store.open(f.root);
    const conversation = await store.addConversation(store.bots[0].id);
    const file = path.join(f.root, 'state.json');
    const legacy = JSON.parse(await readFile(file, 'utf8'));
    assert.equal(Object.hasOwn(legacy.conversations[0], 'canvas'), false);
    assert.equal((await Store.open(f.root)).conversation(conversation.id).canvas, undefined);
    await store.updateCanvas(conversation.id, input());
    await store.updateCanvas(conversation.id, input(1, 'next'));
    const valid = JSON.parse(await readFile(file, 'utf8'));
    valid.conversations[0].canvas.redacted = false;
    valid.conversations[0].canvas.history[0].redacted = false;
    valid.conversations[0].thread = { parentConversationId: randomUUID(), parentMessageId: randomUUID() };
    valid.conversations[0].groupId = randomUUID();
    valid.conversations[0].groupRound = { id: randomUUID(), userMessageId: randomUUID(), pendingBotIds: [store.bots[0].id], createdAt: timestamp };
    valid.conversations[0].memberSessions = [{ botId: store.bots[0].id, sessionId: 'session', sessionFingerprint: 'fingerprint' }];
    await writeFile(file, JSON.stringify(valid));
    assert.equal((await Store.open(f.root)).conversation(conversation.id).canvas!.redacted, false);
    const corruptions: ((canvas: Canvas) => void)[] = [
      canvas => { canvas.revision = 0; }, canvas => { canvas.revision = Number.MAX_SAFE_INTEGER + 1; },
      canvas => { canvas.content = '\0'; }, canvas => { canvas.language = 'TypeScript'; },
      canvas => { canvas.content = '\ud83d'; }, canvas => { canvas.title = '\ude00'; },
      canvas => { canvas.title = ' not canonical '; }, canvas => { canvas.history[0].content = '\ude00'; },
      canvas => { canvas.history = []; },
      canvas => { canvas.redacted = true; }, canvas => { canvas.history[0].redacted = true; },
      canvas => { canvas.history[0].revision = canvas.revision; }, canvas => { canvas.history.push(canvas.history[0]); },
      canvas => { canvas.history = Array.from({ length: 21 }, () => canvas.history[0]); },
    ];
    for (const corrupt of corruptions) {
      const state = structuredClone(valid);
      corrupt(state.conversations[0].canvas);
      const bytes = JSON.stringify(state);
      await writeFile(file, bytes);
      await assert.rejects(Store.open(f.root), /existing state.json was not replaced/);
      assert.equal(await readFile(file, 'utf8'), bytes);
    }
    const initial = structuredClone(legacy);
    initial.conversations[0].canvas = { ...saveCanvas(undefined, input(), timestamp), history: undefined };
    await writeFile(file, JSON.stringify(initial));
    assert.deepEqual((await Store.open(f.root)).conversation(conversation.id).canvas!.history, []);
  } finally { await f.close(); }
});

const fakeAgent = `#!/usr/bin/env node
import { createInterface } from 'node:readline';
import { randomUUID } from 'node:crypto';
import { writeFile } from 'node:fs/promises';
if (process.argv.includes('--version')) { console.log('grok-bot-engine 0.1.0 (2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8)'); process.exit(0); }
const root = process.argv[process.argv.indexOf('--fixture-root') + 1];
const send = value => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', ...value }) + '\\n');
let sessionId;
createInterface({ input: process.stdin }).on('line', async line => {
 const { id, method, params } = JSON.parse(line);
 if (method === 'initialize') { send({ id, result: { protocolVersion: 1, agentCapabilities: { loadSession: true, mcpCapabilities: { http: true } }, agentInfo: { name: 'fake', version: '1' } } }); return; }
 if (method === 'session/new' || method === 'session/load') {
  sessionId = params.sessionId || randomUUID();
  await writeFile(root + '/broker.json', JSON.stringify(params.mcpServers[0]));
  await writeFile(root + '/rules.txt', params._meta.rules);
  send({ id, result: { sessionId } }); return;
 }
 if (method === 'session/prompt') { setInterval(() => {}, 1000); }
});
`;
type ToolResult = { isError?: boolean; content: { type: string; text: string }[] };
async function apiFixture() {
  const f = await temporary();
  const workspace = path.join(f.root, 'workspace');
  await mkdir(workspace);
  await writeFile(path.join(workspace, 'export.txt'), 'unchanged\n');
  const engine = path.join(f.root, 'fake-engine.mjs');
  await writeFile(engine, fakeAgent, { mode: 0o700 });
  const backend = await createBackend({ dataDir: path.join(f.root, 'data'), serveWeb: false, routines: { autoStart: false }, runtime: { command: engine, args: ['--fixture-root', f.root], cancelDeadlineMs: 50 } });
  await new Promise<void>(resolve => backend.server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${(backend.server.address() as AddressInfo).port}`;
  const api = (route: string, body?: unknown, method = body === undefined ? 'GET' : 'POST') => fetch(origin + route, {
    method, headers: { Origin: origin, 'X-Grok-Bot': '1', 'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body),
  });
  const conversation = await backend.store.addConversation(backend.store.bots[0].id);
  const wait = async (predicate: () => boolean) => {
    const deadline = Date.now() + 5000;
    while (!predicate()) {
      assert.ok(Date.now() < deadline, `Canvas fixture timed out. ${JSON.stringify(backend.store.snapshot())}`);
      await new Promise(resolve => setTimeout(resolve, 10));
    }
  };
  const configure = async (apiKey = '', files = false) => {
    assert.equal((await api('/api/settings', { baseUrl: 'http://127.0.0.1:12345/v1', model: 'fixture', workspace: files ? workspace : '', apiKey }, 'PUT')).status, 200);
  };
  const save = (value: unknown, id = conversation.id) => api(`/api/conversations/${id}/canvas`, value, 'PUT');
  const restore = (revision: number, targetRevision: number, id = conversation.id) => api(`/api/conversations/${id}/canvas/restore`, { revision, targetRevision });
  const start = async (id = conversation.id) => {
    assert.equal(backend.store.runtime.available, true, backend.store.runtime.error);
    assert.equal((await api(`/api/conversations/${id}/messages`, { text: 'Hold this canvas test turn.' })).status, 202);
    await wait(() => backend.store.conversation(id).run.status === 'running');
  };
  const mcp = async (method: string, params?: unknown): Promise<unknown> => {
    const broker = JSON.parse(await readFile(path.join(f.root, 'broker.json'), 'utf8')) as { url: string; headers: { value: string }[] };
    const response = await fetch(broker.url, { method: 'POST', headers: { 'Content-Type': 'application/json', Authorization: broker.headers[0].value }, body: JSON.stringify({ jsonrpc: '2.0', id: randomUUID(), method, params }) });
    assert.equal(response.status, 200);
    return (await response.json() as { result: unknown }).result;
  };
  const tool = async (name: string, args: unknown): Promise<ToolResult> => await mcp('tools/call', { name, arguments: args }) as ToolResult;
  const tools = async () => await mcp('tools/list') as { tools: { name: string; description: string; inputSchema: Record<string, unknown> }[] };
  const pending = async () => {
    await wait(() => conversation.run.status === 'awaiting_permission');
    return structuredClone(conversation.run.permission!);
  };
  const approve = async (allow = true) => {
    const permission = await pending();
    assert.deepEqual(permission.options.map(option => option.kind), ['allow_once', 'reject_once']);
    assert.deepEqual(permission.paths, []);
    assert.equal((await api(`/api/conversations/${conversation.id}/permissions/${permission.id}`, { optionId: permission.options.find(option => option.kind === (allow ? 'allow_once' : 'reject_once'))!.id })).status, 200);
    return permission;
  };
  return { ...f, backend, api, workspace, origin, conversation, configure, save, restore, start, wait, tool, tools, pending, approve, close: async () => { await backend.close(); await f.close(); } };
}
const value = <T>(result: ToolResult): T => JSON.parse(result.content[0].text) as T;

test('direct Node canvas routes save and restore during runs while workspace mutations stay idle-only', { timeout: 15000 }, async () => {
  const f = await apiFixture();
  try {
    await f.configure('', true);
    assert.equal((await f.save(input())).status, 200);
    const initial = f.conversation.canvas!;
    await f.start();
    const responses = await Promise.all([f.save(input(1, 'user winner')), f.save(input(1, 'user loser'))]);
    assert.deepEqual(responses.map(response => response.status).sort(), [200, 409]);
    assert.equal(f.conversation.canvas!.revision, 2);
    assert.equal((await f.restore(2, 1)).status, 200);
    assert.equal(f.conversation.canvas!.revision, 3);
    assert.equal(f.conversation.canvas!.source, 'restore');
    assert.equal(f.conversation.canvas!.content, initial.content);
    assert.equal((await f.api('/api/file', { path: 'export.txt', content: 'not allowed', hash: '' }, 'PUT')).status, 409);
    assert.equal((await f.save(input(1, 'stale'))).status, 409);
    assert.equal((await f.save(input(3, '\0'))).status, 400);
    assert.equal((await f.save(input(3, '\ud83d'))).status, 400);
    assert.equal((await f.save(input(3, 'valid', '\ude00'))).status, 400);
    assert.equal((await f.tool('replace_canvas_range', { revision: 3, oldText: '\ud83d', replacement: '' })).isError, true);
    assert.equal((await f.tool('replace_canvas_range', { revision: 3, oldText: 'Initial', replacement: '\ude00' })).isError, true);
    assert.equal(f.conversation.run.permission, undefined);
    assert.equal((await f.save(input(3, 'x'.repeat(MAX_CANVAS_CONTENT_BYTES + 1)))).status, 400);
    assert.equal((await f.save(input(), randomUUID())).status, 404);
    assert.equal((await f.api(`/api/conversations/${f.conversation.id}/canvas/restore`, { revision: 3, targetRevision: 0 })).status, 400);
    const state = await (await f.api('/api/state')).json() as AppState;
    assert.equal(state.conversations[0].canvas!.revision, 3);
  } finally { await f.close(); }
});

test('authenticated canvas tools read only the exact run conversation and metadata history, with no workspace prerequisite', { timeout: 15000 }, async () => {
  const f = await apiFixture();
  try {
    await f.configure();
    const other = await f.backend.store.addConversation(f.backend.store.bots[0].id);
    await f.backend.store.updateCanvas(other.id, input(0, 'OTHER_CONVERSATION_CANARY'));
    await f.start();
    assert.deepEqual(value(await f.tool('read_canvas', {})), { canvas: null });
    // Reads must see edits made after the model run began, not its initial snapshot.
    await f.save(input(0, 'HISTORICAL_FULL_TEXT_CANARY'));
    await f.save(input(1, 'current data:text/plain,😀\r\nline'));
    const read = await f.tool('read_canvas', {});
    assert.equal(read.isError, undefined);
    const canvas = value<{ canvas: CanvasRead }>(read).canvas;
    assert.equal(canvas.content, 'current data:text/plain,😀\r\nline');
    assert.equal(canvas.revision, 2);
    assert.deepEqual(canvas.history, [{ revision: 1, source: 'user', updatedAt: f.conversation.canvas!.history[0].updatedAt }]);
    assertReadText(read.content[0].text, canvas);
    assert.doesNotMatch(read.content[0].text, /HISTORICAL_FULL_TEXT_CANARY|OTHER_CONVERSATION_CANARY|data:/);
    assert.equal((await f.tool('read_canvas', { conversationId: other.id })).isError, true);
    const unauthenticated = await fetch(`${f.origin}/internal/mcp/${f.conversation.run.id}`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'tools/list' }) });
    assert.equal(unauthenticated.status, 403);
    const rules = await readFile(path.join(f.root, 'rules.txt'), 'utf8');
    assert.match(rules, /read_canvas/);
    assert.match(rules, /one-use browser approval/);
    assert.match(rules, /returned nextOffset as offset with the first page revision and viewHash/);
    assert.match(rules, /same revision and viewHash before proposing write_file/);
    assert.match(rules, /reread from offset 0/);
    assert.match(rules, /Do not use spill files or native file tools/);
  } finally { await f.close(); }
});

test('direct canvas discovery declares strict page inputs and authenticated reads deliver complete bounded pages with revision/view conflicts', { timeout: 15000 }, async () => {
  const f = await apiFixture();
  try {
    await f.configure();
    await f.save(input(0, 'OLD_CONTENT_CANARY', 'OLD_TITLE_CANARY', 'old-language'));
    const content = mixedContent();
    await f.save(input(1, content, 'DaTa:text/plain,😀', 'markdown'));
    await f.start();
    const declaration = (await f.tools()).tools.find(tool => tool.name === 'read_canvas')!;
    assert.deepEqual(declaration.inputSchema, {
      type: 'object', properties: {
        offset: { type: 'integer', minimum: 0, maximum: Number.MAX_SAFE_INTEGER },
        revision: { type: 'integer', minimum: 1, maximum: Number.MAX_SAFE_INTEGER },
        viewHash: { type: 'string', pattern: '^[a-f0-9]{64}$' },
      }, additionalProperties: false,
    });
    assert.match(declaration.description, /UTF-8 bytes in public redacted content/);
    assert.match(declaration.description, /Pass nextOffset as offset together with the returned revision and viewHash/);
    assert.match(declaration.description, /Assemble all pages of the same revision and viewHash/);
    assert.match(declaration.description, /never use spill files or native file tools/);
    const firstRead = await f.tool('read_canvas', {});
    assert.equal(firstRead.isError, undefined);
    const first = value<{ canvas: CanvasRead }>(firstRead).canvas;
    assert.equal(first.offset, 0);
    assert.equal(first.totalBytes, MAX_CANVAS_CONTENT_BYTES);
    assert.ok(first.nextOffset);
    const current = f.conversation.canvas;
    const chunks: string[] = [];
    let read = firstRead;
    let page = first;
    do {
      assertReadText(read.content[0].text, page);
      assert.equal(read.content[0].text, skillResponseText({ canvas: page }));
      assert.equal(page.revision, 2);
      assert.equal(page.viewHash, first.viewHash);
      assert.doesNotMatch(read.content[0].text, /OLD_CONTENT_CANARY|OLD_TITLE_CANARY|old-language/);
      assert.deepEqual(page.history, [{ revision: 1, source: 'user', updatedAt: current!.history[0].updatedAt }]);
      chunks.push(page.content);
      if (page.nextOffset === undefined) break;
      assert.ok(page.content.length > 0);
      const offset = page.nextOffset;
      read = await f.tool('read_canvas', continuation(page));
      assert.equal(read.isError, undefined);
      page = value<{ canvas: CanvasRead }>(read).canvas;
      assert.equal(page.offset, offset);
      assert.ok(chunks.length < 1000);
    } while (true);
    assert.ok(chunks.length > 1);
    assert.equal(chunks.join(''), content);
    assert.equal(f.conversation.canvas, current);
    assert.equal(f.conversation.run.permission, undefined);
    for (const args of [
      { offset: -1 }, { offset: 0.5 }, { offset: Number.MAX_SAFE_INTEGER + 1 }, { offset: '0' },
      { offset: 1 }, { offset: 1, revision: first.revision }, { offset: 1, viewHash: first.viewHash },
      continuation(first, 1), continuation(first, first.totalBytes + 1),
      { revision: 0 }, { viewHash: 'A'.repeat(64) }, { offset: 0, unexpected: true },
    ]) assert.equal((await f.tool('read_canvas', args)).isError, true, JSON.stringify(args));
    const eof = await f.tool('read_canvas', continuation(first, first.totalBytes));
    assert.equal(eof.isError, undefined);
    const eofPage = value<{ canvas: CanvasRead }>(eof).canvas;
    assert.equal(eofPage.content, '');
    assert.equal(Object.hasOwn(eofPage, 'nextOffset'), false);
    assertReadText(eof.content[0].text, eofPage);
    const wrongView = await f.tool('read_canvas', { ...continuation(first), viewHash: 'f'.repeat(64) });
    assert.equal(wrongView.isError, true);
    assert.match(wrongView.content[0].text, /public canvas view changed/);
    assert.equal((await f.save(input(2, 'new revision'))).status, 200);
    const stale = await f.tool('read_canvas', continuation(first));
    assert.equal(stale.isError, true);
    assert.match(stale.content[0].text, /Canvas changed/);
    assert.doesNotMatch(stale.content[0].text, /new revision/);
    const latest = value<{ canvas: CanvasRead }>(await f.tool('read_canvas', {})).canvas;
    assert.equal(latest.revision, 3);
    assert.equal(latest.content, 'new revision');
    assert.equal(Object.hasOwn(latest, 'nextOffset'), false);
  } finally { await f.close(); }
});

test('canvas approval displays the complete raw range, is one-use, and successful Unicode replacement records bot history', { timeout: 15000 }, async () => {
  const f = await apiFixture();
  try {
    await f.configure();
    const oldText = `${'x'.repeat(101000)}OLD_END😀\r\n`;
    const replacement = `${'y'.repeat(101000)}NEW_END🐈\r\n`;
    await f.save(input(0, `before\r\n${oldText}after😀`));
    await f.start();
    const replacing = f.tool('replace_canvas_range', { revision: 1, oldText, replacement });
    const permission = await f.pending();
    assert.equal(permission.options[0].label, 'Replace canvas once');
    assert.ok(permission.description.includes(oldText));
    assert.ok(permission.description.endsWith(replacement));
    assert.ok(permission.description.length > 200000);
    assert.equal(f.conversation.canvas!.revision, 1);
    const allowed = await f.approve();
    const result = await replacing;
    assert.equal(result.isError, undefined);
    assert.deepEqual(value(result), { revision: 2, saved: true });
    assert.equal(f.conversation.canvas!.content, `before\r\n${replacement}after😀`);
    assert.equal(f.conversation.canvas!.source, 'bot');
    assert.equal(f.conversation.canvas!.history[0].content, `before\r\n${oldText}after😀`);
    assert.equal(f.conversation.messages.at(-1)!.tools.at(-1)!.status, 'completed');
    assert.equal(f.conversation.messages.at(-1)!.tools.at(-1)!.output, 'Saved canvas revision 2.');
    assert.equal((await f.api(`/api/conversations/${f.conversation.id}/permissions/${allowed.id}`, { optionId: allowed.options[0].id })).status, 409);
  } finally { await f.close(); }
});

test('denied, ambiguous and conflicting canvas proposals write nothing and manual edits are admitted while approval waits', { timeout: 15000 }, async () => {
  const f = await apiFixture();
  try {
    await f.configure();
    await f.save(input(0, 'prefix unique suffix'));
    await f.start();
    const denied = f.tool('replace_canvas_range', { revision: 1, oldText: 'unique', replacement: 'denied' });
    await f.approve(false);
    assert.equal((await denied).isError, true);
    assert.equal(f.conversation.canvas!.revision, 1);
    assert.equal(f.conversation.messages.at(-1)!.tools.at(-1)!.status, 'failed');
    assert.equal(f.conversation.messages.at(-1)!.tools.at(-1)!.output, 'Canvas replacement was denied or cancelled.');
    const conflicting = f.tool('replace_canvas_range', { revision: 1, oldText: 'unique', replacement: 'bot result' });
    await f.pending();
    assert.equal((await f.save(input(1, 'user current unique suffix'))).status, 200);
    await f.approve();
    const rejected = await conflicting;
    assert.equal(rejected.isError, true);
    assert.match(rejected.content[0].text, /Canvas changed/);
    assert.equal(f.conversation.canvas!.revision, 2);
    assert.equal(f.conversation.canvas!.content, 'user current unique suffix');
    assert.equal(f.conversation.messages.at(-1)!.tools.at(-1)!.status, 'failed');
    assert.equal((await f.save(input(2, 'duplicate duplicate'))).status, 200);
    assert.equal((await f.tool('replace_canvas_range', { revision: 3, oldText: 'duplicate', replacement: '' })).isError, true);
    assert.equal(f.conversation.run.permission, undefined);
    assert.equal(f.conversation.canvas!.revision, 3);
    const deleting = f.tool('replace_canvas_range', { revision: 3, oldText: 'duplicate duplicate', replacement: '' });
    await f.approve();
    assert.equal((await deleting).isError, undefined);
    assert.equal(f.conversation.canvas!.content, '');
    assert.equal(f.conversation.canvas!.revision, 4);
  } finally { await f.close(); }
});

test('cancellation while canvas approval is pending consumes no write and no stale approval can be replayed', { timeout: 15000 }, async () => {
  const f = await apiFixture();
  try {
    await f.configure();
    await f.save(input(0, 'unique'));
    await f.start();
    const replacing = f.tool('replace_canvas_range', { revision: 1, oldText: 'unique', replacement: 'not written' });
    const permission = await f.pending();
    assert.equal((await f.api(`/api/conversations/${f.conversation.id}/cancel`, {})).status, 200);
    assert.equal((await replacing).isError, true);
    assert.equal(f.conversation.run.status, 'cancelled');
    assert.equal(f.conversation.canvas!.revision, 1);
    assert.equal(f.conversation.canvas!.content, 'unique');
    assert.equal(f.conversation.messages.at(-1)!.tools.at(-1)!.status, 'failed');
    assert.equal((await f.api(`/api/conversations/${f.conversation.id}/permissions/${permission.id}`, { optionId: permission.options[0].id })).status, 409);
  } finally { await f.close(); }
});

test('cancel queued after canvas approval wins before commit and releases the app command queue without deadlocking', { timeout: 15000 }, async t => {
  const f = await apiFixture();
  let release!: () => void;
  try {
    await f.configure();
    await f.save(input(0, 'unique'));
    await f.start();
    const replacing = f.tool('replace_canvas_range', { revision: 1, oldText: 'unique', replacement: 'not written' });
    const permission = await f.pending();
    const resolve = f.backend.runtime.resolve.bind(f.backend.runtime);
    let enter!: () => void;
    const entered = new Promise<void>(done => { enter = done; });
    const gate = new Promise<void>(done => { release = done; });
    t.mock.method(f.backend.runtime, 'resolve', async (...args: Parameters<typeof resolve>) => { enter(); await gate; return resolve(...args); });
    const approving = f.api(`/api/conversations/${f.conversation.id}/permissions/${permission.id}`, { optionId: permission.options[0].id });
    await entered;
    const requested = new Promise<void>(done => {
      f.backend.server.once('request', request => { assert.equal(request.url, `/api/conversations/${f.conversation.id}/cancel`); done(); });
    });
    const cancelling = f.api(`/api/conversations/${f.conversation.id}/cancel`, {});
    await requested;
    release();
    assert.equal((await approving).status, 200);
    assert.equal((await cancelling).status, 200);
    const result = await replacing;
    assert.equal(result.isError, true);
    assert.equal(f.conversation.run.status, 'cancelled');
    assert.equal(f.conversation.canvas!.revision, 1);
    assert.equal(f.conversation.canvas!.content, 'unique');
    assert.equal(f.conversation.messages.at(-1)!.tools.at(-1)!.status, 'failed');
  } finally { release?.(); t.mock.restoreAll(); await f.close(); }
});

test('snapshots and SSE mark redacted canvas versions without double-masking and raw content stays durable until settings change', { timeout: 15000 }, async () => {
  const f = await apiFixture();
  try {
    await f.configure();
    await f.save(input(0, 'a', 'xyz', 'cpp'));
    await f.configure('a');
    const publicState = await (await f.api('/api/state')).json() as AppState;
    assert.equal(publicState.conversations[0].canvas!.content, '[redacted]');
    assert.equal(publicState.conversations[0].canvas!.redacted, true);
    assert.equal(publicState.conversations[0].canvas!.source, 'user');
    assert.equal(publicState.conversations[0].canvas!.updatedAt, f.conversation.canvas!.updatedAt);
    assert.equal((await f.save(input(1, 'new', 'xyz', 'cpp'))).status, 403);
    const events = await f.api('/api/events');
    const reader = events.body!.getReader();
    const first = new TextDecoder().decode((await reader.read()).value);
    assert.match(first, /"content":"\[redacted\]"/);
    assert.match(first, /"redacted":true/);
    await reader.cancel();
    const disk = JSON.parse(await readFile(path.join(f.root, 'data', 'state.json'), 'utf8'));
    assert.equal(disk.conversations[0].canvas.content, 'a');
    assert.equal(disk.conversations[0].canvas.redacted, undefined);
    await f.configure('different-key');
    assert.equal((await f.save(input(1, 'next', 'xyz', 'cpp'))).status, 200);
    assert.equal(f.conversation.canvas!.revision, 2);
    assert.equal(f.conversation.canvas!.history[0].content, 'a');
    await f.configure('a');
    const cleanCurrent = await (await f.api('/api/state')).json() as AppState;
    assert.equal(cleanCurrent.conversations[0].canvas!.redacted, undefined);
    assert.equal(cleanCurrent.conversations[0].canvas!.history[0].redacted, true);
    assert.equal((await f.restore(2, 1)).status, 403);
    assert.equal((await f.save(input(2, 'new', 'xyz', 'cpp'))).status, 200);
  } finally { await f.close(); }
});

test('bot-delivery recipients cannot read, replace, save or restore any canvas, including completed deliveries', { timeout: 15000 }, async () => {
  const f = await apiFixture();
  try {
    await f.configure();
    const receiver = await f.backend.store.addBot({ name: 'Receiver', color: '#3b82f6', instructions: '' });
    const source = f.conversation;
    const assistant = { id: randomUUID(), role: 'assistant' as const, text: '', createdAt: timestamp, tools: [] };
    source.messages.push(assistant);
    source.run = { id: randomUUID(), status: 'running' };
    const delivery = await f.backend.store.queueBotMessage(source.id, source.run.id, assistant.id, { toBotId: receiver.id, text: 'Recipient canvas denial' });
    source.run.status = 'completed';
    await f.backend.store.changed();
    const recipient = f.backend.store.conversation(delivery.recipientConversationId);
    assert.throws(() => f.backend.store.readCanvas(recipient.id), status(403));
    await assert.rejects(f.backend.store.updateCanvas(recipient.id, input()), status(403));
    await assert.rejects(f.backend.store.restoreCanvas(recipient.id, { revision: 1, targetRevision: 1 }), status(403));
    assert.equal((await f.save(input(), recipient.id)).status, 403);
    assert.equal((await f.restore(1, 1, recipient.id)).status, 403);
    await f.backend.routines.tick();
    await f.wait(() => recipient.run.status === 'running');
    assert.equal((await f.tool('read_canvas', {})).isError, true);
    assert.equal((await f.tool('replace_canvas_range', { revision: 0, oldText: 'x', replacement: '' })).isError, true);
    assert.equal(recipient.run.permission, undefined);
    await f.api(`/api/conversations/${recipient.id}/cancel`, {});
    assert.throws(() => f.backend.store.readCanvas(recipient.id), status(403));
    recipient.canvas = saveCanvas(undefined, input(0, 'MUST_NOT_LEAK_RECIPIENT_CANVAS'), timestamp);
    assert.equal(f.backend.store.snapshot().conversations.find(item => item.id === recipient.id)!.canvas, undefined);
  } finally { await f.close(); }
});

test('workspace canvas export remains an ordinary approved write_file operation and never writes on denial', { timeout: 15000 }, async () => {
  const f = await apiFixture();
  try {
    await f.configure('', true);
    await f.save(input(0, 'Export 😀\r\ncomplete canvas\n'));
    await f.start();
    const latest = value<{ canvas: CanvasRead }>(await f.tool('read_canvas', {})).canvas;
    const file = value<{ hash: string }>(await f.tool('read_file', { path: 'export.txt' }));
    const denied = f.tool('write_file', { path: 'export.txt', content: latest.content, hash: file.hash });
    const permission = await f.pending();
    assert.deepEqual(permission.paths, ['export.txt']);
    // Existing file permissions keep their own review and one-use behavior.
    const reject = permission.options.find(option => option.kind === 'reject_once')!;
    await f.api(`/api/conversations/${f.conversation.id}/permissions/${permission.id}`, { optionId: reject.id });
    assert.equal((await denied).isError, true);
    assert.equal(await readFile(path.join(f.workspace, 'export.txt'), 'utf8'), 'unchanged\n');
    const allowed = f.tool('write_file', { path: 'export.txt', content: latest.content, hash: file.hash });
    const next = await f.pending();
    await f.api(`/api/conversations/${f.conversation.id}/permissions/${next.id}`, { optionId: next.options.find(option => option.kind === 'allow_once')!.id });
    assert.equal((await allowed).isError, undefined);
    assert.equal(await readFile(path.join(f.workspace, 'export.txt'), 'utf8'), latest.content);
    assert.equal(f.conversation.canvas!.revision, 1);
  } finally { await f.close(); }
});
