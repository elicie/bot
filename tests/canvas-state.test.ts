import { test } from 'node:test';
import assert from 'node:assert/strict';
import type { Canvas } from '../shared/types';
import { canvasDirty, canvasDraft, canvasExportRequest, canvasFilename, canvasStale, reconcileCanvas } from '../web/canvas-state';
const canvas = (revision: number, content = '한글 😀\r\n문서'): Canvas => ({ revision, content, title: '안내', language: 'markdown', source: 'user', updatedAt: '2026-11-02T00:00:00.000Z', history: [] });
test('canvas live snapshots refresh clean editors but never replace an unsaved draft', () => {
  const original = canvasDraft(canvas(1));
  assert.equal(canvasDirty(original), false);
  assert.equal(reconcileCanvas(original, canvas(2, 'Bot 수정')).content, 'Bot 수정');
  const dirty = { ...original, content: '내 초안' };
  assert.equal(canvasDirty(dirty), true);
  assert.equal(reconcileCanvas(dirty, canvas(2)), dirty);
  assert.equal(reconcileCanvas(original, canvas(1)), original);
});
test('same-revision secret mask/unmask refreshes clean editors and blocks dirty projected drafts', () => {
  const original = canvas(1, 'protected-value');
  const masked = { ...original, content: '[redacted]', redacted: true };
  const clean = canvasDraft(original);
  const projected = reconcileCanvas(clean, masked);
  assert.equal(projected.content, '[redacted]');
  assert.equal(projected.redacted, true);
  assert.equal(reconcileCanvas(projected, original).content, 'protected-value');
  const dirty = { ...projected, content: '[redacted] + my draft' };
  assert.equal(reconcileCanvas(dirty, original), dirty);
  assert.equal(canvasStale(dirty, original), true);
  assert.equal(canvasStale(clean, masked), true);
});
test('new canvas drafts keep creation revision zero and notice edits to metadata or code', () => {
  const draft = canvasDraft();
  assert.equal(draft.base.revision, 0);
  assert.equal(canvasDirty(draft), false);
  assert.equal(canvasDirty({ ...draft, language: 'rust' }), true);
  assert.equal(canvasDirty({ ...draft, title: '문서.rs' }), true);
  assert.equal(canvasDirty({ ...draft, content: 'fn main() {}' }), true);
});
test('download filename is a basename, never an absolute or traversal path', () => {
  assert.equal(canvasFilename({ title: '../../문서', language: 'markdown' }), '_.._문서.md');
  assert.equal(canvasFilename({ title: 'app', language: 'typescript' }), 'app.ts');
  assert.equal(canvasFilename({ title: 'style.css', language: 'text' }), 'style.css');
  assert.equal(canvasFilename({ title: '...', language: 'text' }), 'canvas.txt');
});
test('workspace export request binds saved revision and keeps existing approved write flow', () => {
  const prompt = canvasExportRequest(canvas(7));
  assert.match(prompt, /버전이 7/);
  assert.match(prompt, /"안내.md"/);
  assert.match(prompt, /버전이 달라졌다면 저장하지/);
  assert.match(prompt, /기존 파일 쓰기 승인/);
});
