import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import test, { describe, type TestContext } from 'node:test';
import { unified } from 'unified';
import { MAX_PREVIEW_SOURCE, previewText } from '../web/preview-text.js';

// Observe the real parser without exporting the helper's processor or cache. Unified
// processors inherit parse, including processors created before this spy is installed.
const processorPrototype = Object.getPrototypeOf(unified) as typeof unified;
function observeParser(t: TestContext) {
  const parse = t.mock.method(processorPrototype, 'parse');
  t.after(() => parse.mock.restore());
  return parse;
}
function assertBoundedParserInput(parse: ReturnType<typeof observeParser>) {
  assert.ok(parse.mock.callCount() > 0, 'the spy must observe actual Markdown parsing');
  for (const call of parse.mock.calls) {
    const source = call.arguments[0];
    assert.ok(typeof source === 'string', 'preview parsing receives a source string');
    assert.ok(source.length <= MAX_PREVIEW_SOURCE,
      `Markdown received ${source.length} characters; the cap is ${MAX_PREVIEW_SOURCE}`);
  }
}
function sizedMessage(prefix: string, length: number) {
  const body = 'ordinary message content '.repeat(Math.ceil(length / 25));
  return (prefix + body).slice(0, length);
}
function assertPreviewBound(result: string) {
  assert.ok(result.length > 0, 'these nonempty fixtures should have visible previews');
  assert.ok(result.length <= 280, `preview has ${result.length} characters`);
}

// Prototype mocks and the module-level cache are deliberately exercised serially.
describe('preview text performance contracts', { concurrency: false }, () => {
  test('caps every actual Markdown parser input for 200 KiB and 1 MiB messages', t => {
    assert.equal(MAX_PREVIEW_SOURCE, 2048);
    const parse = observeParser(t);
    for (const size of [200 * 1024, 1024 * 1024]) {
      for (const prefix of [
        '# Bounded Markdown\n\n**Visible** text and [a link](https://example.com).\n\n',
        '# Bounded formula\n\n\\(x_i * y_j + z_i\\) and visible text.\n\n',
      ]) {
        const source = sizedMessage(prefix, size);
        assert.equal(source.length, size);
        parse.mock.resetCalls();
        const result = previewText(source);
        assertPreviewBound(result);
        assert.match(result, /^Bounded (?:Markdown|formula)/);
        // Math normalization may legitimately invoke the parser a second time.
        // Check every observed input, not an implementation-specific pass count.
        assertBoundedParserInput(parse);
        assert.equal(parse.mock.calls[0].arguments[0], source.slice(0, MAX_PREVIEW_SOURCE));
      }
    }
  });

  test('dense short formulas cannot expand a later parser pass beyond the source cap', t => {
    const parse = observeParser(t);
    for (const size of [200 * 1024, 1024 * 1024]) {
      const source = '\\(x\\) '.repeat(Math.ceil(size / 6)).slice(0, size);
      parse.mock.resetCalls();
      const result = previewText(source);
      assertPreviewBound(result);
      assert.match(result, /^x(?: x)+/);
      assertBoundedParserInput(parse);
    }
  });

  test('120 distinct 4.8 KB messages are cache hits after a fresh snapshot reload', t => {
    const parse = observeParser(t);
    const messages = Array.from({ length: 120 }, (_, index) => ({
      id: `preview-performance:snapshot:${index}`,
      text: sizedMessage(`# Conversation ${index}\n\n**Visible** \\(x_i * y_j + z_i\\)\n\n`, 4800),
    }));
    const previews = messages.map(message => {
      assert.equal(message.text.length, 4800);
      const result = previewText(message.text, message.id);
      assertPreviewBound(result);
      assert.match(result, /^Conversation \d+ Visible/);
      return result;
    });
    assert.ok(parse.mock.callCount() >= messages.length, 'distinct cold messages must be parsed');
    assertBoundedParserInput(parse);

    // JSON models a new SSE/history payload, not reuse of the original objects.
    const reloaded = JSON.parse(JSON.stringify(messages)) as typeof messages;
    assert.notStrictEqual(reloaded, messages);
    parse.mock.resetCalls();
    for (const [index, message] of reloaded.entries()) {
      assert.notStrictEqual(message, messages[index]);
      assert.equal(previewText(message.text, message.id), previews[index]);
    }
    assert.equal(parse.mock.callCount(), 0, 'all 120 stable message IDs must hit the cache');
  });

  test('a changed bounded prefix invalidates an ID while changes beyond the cap do not', t => {
    const parse = observeParser(t);
    const id = 'preview-performance:edited-message';
    const original = sizedMessage('# Original revision\n\n', 200 * 1024);
    const first = previewText(original, id);
    assert.match(first, /^Original revision/);
    assertBoundedParserInput(parse);

    parse.mock.resetCalls();
    const revised = sizedMessage('# Revised revision\n\n', 200 * 1024);
    const second = previewText(revised, id);
    assert.match(second, /^Revised revision/);
    assert.notEqual(second, first);
    assertBoundedParserInput(parse);

    parse.mock.resetCalls();
    const changedTail = revised.slice(0, MAX_PREVIEW_SOURCE) + 'changed outside the preview\n'.repeat(40_000);
    assert.notEqual(changedTail, revised);
    assert.equal(previewText(changedTail, id), second);
    assert.equal(parse.mock.callCount(), 0, 'an irrelevant tail edit must not trigger Markdown parsing');
  });

  test('200 KiB and 1 MiB messages remain cacheable after serialization', t => {
    const parse = observeParser(t);
    for (const size of [200 * 1024, 1024 * 1024]) {
      const message = {
        id: `preview-performance:large:${size}`,
        text: sizedMessage(`# Large message ${size}\n\n`, size),
      };
      parse.mock.resetCalls();
      const result = previewText(message.text, message.id);
      assertPreviewBound(result);
      assertBoundedParserInput(parse);

      const reloaded = JSON.parse(JSON.stringify(message)) as typeof message;
      assert.notStrictEqual(reloaded, message);
      parse.mock.resetCalls();
      assert.equal(previewText(reloaded.text, reloaded.id), result);
      assert.equal(parse.mock.callCount(), 0, `${size}-character messages must not bypass the cache`);
    }
  });

  test('deep blockquote, list, and mixed containers bypass Markdown safely', t => {
    const parse = observeParser(t);
    for (const [index, markers] of ['> '.repeat(64), '- '.repeat(64), '1. '.repeat(64), '> - 1. '.repeat(24)].entries()) {
      const source = markers + 'nested content';
      assert.ok(source.length < MAX_PREVIEW_SOURCE, 'exercise nesting protection, not source truncation');
      assert.equal(previewText(source, `preview-performance:nested:${index}`), '메시지');
    }
    assert.equal(parse.mock.callCount(), 0, 'pathological containers must be rejected before parsing');
    assert.equal(previewText('**Healthy message**', 'preview-performance:after-nesting'), 'Healthy message');
    assertBoundedParserInput(parse);
  });

  test('a parser exception returns the safe fallback without poisoning other messages', t => {
    const parse = observeParser(t);
    parse.mock.mockImplementationOnce(() => { throw new RangeError('simulated Markdown parser failure'); });
    assert.equal(previewText('**Failing message**', 'preview-performance:forced-failure'), '메시지');
    assert.ok(parse.mock.callCount() > 0, 'the failure must occur inside the real parser entry point');

    parse.mock.resetCalls();
    assert.equal(previewText('**Unaffected message**', 'preview-performance:after-failure'), 'Unaffected message');
    assertBoundedParserInput(parse);
  });

  test('the message cache eventually evicts entries instead of growing without bound', t => {
    const parse = observeParser(t);
    // Capacity is a resource contract; which ID is evicted is not. Probe the
    // entire set until any miss rather than requiring a particular FIFO/LRU order.
    const messages = Array.from({ length: 2049 }, (_, index) => ({
      id: `preview-performance:capacity:${index}`,
      text: `Capacity entry ${index}`,
    }));
    for (const message of messages) assert.equal(previewText(message.text, message.id), message.text);
    assertBoundedParserInput(parse);

    parse.mock.resetCalls();
    let evicted = false;
    for (const message of messages) {
      assert.equal(previewText(message.text, message.id), message.text);
      if (parse.mock.callCount() > 0) {
        evicted = true;
        parse.mock.resetCalls();
        assert.equal(previewText(message.text, message.id), message.text);
        assert.equal(parse.mock.callCount(), 0, 'a miss must still populate the bounded cache');
        break;
      }
    }
    assert.ok(evicted, 'a cache of at most 2048 entries cannot retain all 2049 IDs');
  });

  test('many unclosed TeX openers complete in an isolated process with bounded parser input', { timeout: 45_000 }, () => {
    // This is a generous runaway-process safety gate, not a machine-speed
    // benchmark. A synchronous parser regression cannot hang the parent runner.
    const child = spawnSync(process.execPath, ['--import', 'tsx', '--input-type=module', '--eval', `
      import assert from 'node:assert/strict';
      import { mock } from 'node:test';
      import { unified } from 'unified';
      import { MAX_PREVIEW_SOURCE, previewText } from ${JSON.stringify(new URL('../web/preview-text.ts', import.meta.url).href)};
      const parse = mock.method(Object.getPrototypeOf(unified), 'parse');
      try {
        for (const opener of ['\\\\(', '\\\\[']) {
          for (const size of [200 * 1024, 1024 * 1024]) {
            const unit = opener + ' unclosed ';
            const source = unit.repeat(Math.ceil(size / unit.length)).slice(0, size);
            assert.equal(source.length, size);
            for (let iteration = 0; iteration < 8; iteration++) {
              parse.mock.resetCalls();
              const result = previewText(source, 'unclosed:' + opener + ':' + size + ':' + iteration);
              assert.ok(result.length > 0 && result.length <= 280);
              assert.ok(parse.mock.callCount() > 0, 'observe actual parsing of every cold message');
              for (const call of parse.mock.calls) {
                assert.equal(typeof call.arguments[0], 'string');
                assert.ok(call.arguments[0].length <= MAX_PREVIEW_SOURCE);
              }
            }
          }
        }
        process.stdout.write('unclosed delimiters bounded\\n');
      } finally {
        parse.mock.restore();
      }
    `], {
      cwd: new URL('..', import.meta.url),
      encoding: 'utf8',
      timeout: 30_000,
      killSignal: 'SIGKILL',
      maxBuffer: 1024 * 1024,
    });
    assert.ifError(child.error);
    assert.equal(child.signal, null, `child terminated with ${child.signal}: ${child.stderr}`);
    assert.equal(child.status, 0, child.stderr || child.stdout);
    assert.match(child.stdout, /unclosed delimiters bounded/);
  });
});
