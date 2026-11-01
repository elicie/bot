import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import {
  CHANNEL,
  MAX_INLINE_HEIGHT,
  MAX_VISUALIZATION_SOURCE,
  MIN_INLINE_HEIGHT,
  VISUALIZATION_CSP,
  buildVisualizationDocument,
  inlineHeight,
  inlineSafe,
  newNonce,
  readFrameMessage,
  sourceProblem,
  sourceTitle,
  visualizationLanguages,
  wantsD3,
} from '../web/visualization-document.js';
import { mainBotId } from '../web/state.js';
import type { AppState } from '../shared/types.js';

const nonce = 'a'.repeat(32);
const cspMeta = `<meta http-equiv="Content-Security-Policy" content="${VISUALIZATION_CSP}">`;

test('the CSP meta comes first, before any of the source, and allows no network', () => {
  for (const source of ['<p>hi</p>', '<!DOCTYPE html><html><head><title>x</title></head><body>y</body></html>', '<meta http-equiv="refresh" content="0;url=https://example.com">']) {
    const html = buildVisualizationDocument(source, 'html', nonce);
    assert.ok(html.startsWith(`<!DOCTYPE html><html><head>${cspMeta}`), html.slice(0, 200));
    assert.ok(html.indexOf(source) > html.indexOf(cspMeta));
    assert.ok(html.endsWith(source) || html.endsWith(`${source}</body></html>`));
  }
  assert.match(VISUALIZATION_CSP, /^default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob:;/);
  assert.doesNotMatch(VISUALIZATION_CSP, /https?:|'self'|\*|connect-src|unsafe-eval/);
});

test('whole documents continue the head; fragments and SVG start the body', () => {
  const whole = buildVisualizationDocument('<!doctype html><html><head><style>p{}</style></head><body></body></html>', 'html', nonce);
  assert.ok(!whole.includes('</head><body><!doctype'));
  const fragment = buildVisualizationDocument('<style>p{}</style><p>x</p>', 'visualization', nonce);
  assert.ok(fragment.includes('</head><body><style>p{}</style><p>x</p></body></html>'));
  const svg = buildVisualizationDocument('<html><svg></svg></html>', 'svg', nonce);
  assert.ok(svg.includes('</head><body><html><svg></svg></html></body></html>'));
});

test('the bootstrap is one line with the nonce and keeps source line numbers', () => {
  const html = buildVisualizationDocument('<p>one</p>\n<script>two()</script>', 'html', nonce);
  assert.equal(html.split('\n').length, 2, 'the source starts on line 1');
  assert.ok(html.includes(`var nonce = "${nonce}";`));
  assert.ok(html.includes('var offset = 0;'));
  assert.ok(html.includes('document.currentScript'), 'the bootstrap removes itself');
  assert.ok(html.includes('window.addEventListener("click", guardLinks, true)'), 'link clicks are cancelled');
  const withRuntime = buildVisualizationDocument('<p>x</p>', 'visualization', nonce, '// header\nwindow.d3 = {};');
  assert.ok(withRuntime.includes('var offset = 1;'));
  assert.throws(() => buildVisualizationDocument('<p>x</p>', 'html', 'not-hex"; alert(1); "'));
  assert.throws(() => buildVisualizationDocument('<p>x</p>', 'html', nonce, 'a = "</script><script>alert(1)</script>"'));
});

test('runtimes are inlined only when they cannot end their script element', () => {
  assert.equal(inlineSafe('var a = 1;'), true);
  assert.equal(inlineSafe('var a = "</SCRIPT>";'), false);
  assert.equal(inlineSafe('var a = "<!--";'), false);
  assert.equal(inlineSafe('var a = "<script";'), false);
  const d3 = readFileSync('node_modules/d3/dist/d3.min.js', 'utf8');
  assert.equal(inlineSafe(d3), true, 'the bundled d3 build can be inlined');
});

test('frame messages need the channel and this frame nonce', () => {
  const base = { channel: CHANNEL, nonce };
  assert.deepEqual(readFrameMessage({ ...base, type: 'ready' }, nonce), { type: 'ready' });
  assert.deepEqual(readFrameMessage({ ...base, type: 'escape' }, nonce), { type: 'escape' });
  assert.deepEqual(readFrameMessage({ ...base, type: 'navigation' }, nonce), { type: 'navigation' });
  assert.equal(readFrameMessage({ ...base, type: 'ready' }, 'b'.repeat(32)), null);
  assert.equal(readFrameMessage({ ...base, channel: 'other', type: 'ready' }, nonce), null);
  assert.equal(readFrameMessage({ ...base, type: 'navigate' }, nonce), null);
  assert.equal(readFrameMessage(null, nonce), null);
  assert.equal(readFrameMessage('ready', nonce), null);
  assert.equal(readFrameMessage({ ...base, type: 'error', message: 'x'.repeat(2000) }, nonce)?.type, 'error');
  const error = readFrameMessage({ ...base, type: 'error', message: 'x'.repeat(2000) }, nonce);
  assert.equal(error?.type === 'error' && error.message.length, 500);
  assert.deepEqual(readFrameMessage({ ...base, type: 'error', message: { toString: () => 'x' } }, nonce), { type: 'error', message: '' });
  assert.equal(readFrameMessage({ ...base, type: 'size', height: Number.NaN }, nonce), null);
  assert.equal(readFrameMessage({ ...base, type: 'size', height: -1 }, nonce), null);
  assert.deepEqual(readFrameMessage({ ...base, type: 'size', height: 1e9 }, nonce), { type: 'size', height: 100_000 });
  assert.equal(readFrameMessage({ ...base, type: 'blocked', count: 1.5 }, nonce), null);
  assert.equal(inlineHeight(0), MIN_INLINE_HEIGHT);
  assert.equal(inlineHeight(10_000), MAX_INLINE_HEIGHT);
});

test('sources: limits, titles, d3 use, languages, nonces', () => {
  assert.equal(sourceProblem('  \n'), 'empty');
  assert.equal(sourceProblem('x'.repeat(MAX_VISUALIZATION_SOURCE + 1)), 'too-large');
  assert.equal(sourceProblem('<p>x</p>'), null);
  assert.equal(sourceTitle('<title> 매출 &amp; 비용 </title>'), '매출 & 비용');
  assert.equal(sourceTitle('<p>no title</p>'), null);
  assert.equal(wantsD3('html', 'd3.select("#x")'), true);
  assert.equal(wantsD3('html', 'const d3x = 1'), false);
  assert.equal(wantsD3('visualization', ''), true);
  assert.equal(Object.hasOwn(visualizationLanguages, 'constructor'), false);
  assert.match(newNonce(), /^[0-9a-f]{32}$/);
  assert.notEqual(newNonce(), newNonce());
});

test('the main bot is the server choice, else the first bot', () => {
  const bots = [{ id: 'a' }, { id: 'b' }] as AppState['bots'];
  assert.equal(mainBotId({ bots } as AppState), 'a');
  assert.equal(mainBotId({ bots, mainBotId: 'b' } as AppState), 'b');
  assert.equal(mainBotId({ bots, mainBotId: 'gone' } as AppState), 'a');
  assert.equal(mainBotId({ bots: [] as AppState['bots'] } as AppState), null);
  assert.equal(mainBotId(null), null);
});
