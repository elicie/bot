import test from 'node:test';
import assert from 'node:assert/strict';
import { canonicalLinkPreviewUrl, extractLinkPreviewUrls } from '../web/link-preview-urls.js';

test('preview URLs allow only absolute credential-free HTTP(S) and ignore fragments', () => {
  assert.equal(canonicalLinkPreviewUrl('HTTPS://Example.COM:443/guide?q=hello#part'), 'https://example.com/guide?q=hello');
  assert.equal(canonicalLinkPreviewUrl('http://example.com'), 'http://example.com/');
  for (const input of ['javascript:alert(1)', 'file:///etc/passwd', 'data:text/plain,hello', 'ftp://example.com/', '/api/generated-images/id', '//example.com/a', 'https://user:pass@example.com/', 'https://user@example.com/', 'https://example.com/a b', 'https://example.com/\\evil', 'https://example.com/\nnext', 'https://example.com/' + 'x'.repeat(4097)]) {
    assert.equal(canonicalLinkPreviewUrl(input), null, input);
  }
});

test('answers collect Markdown and bare links in document order, deduplicate and stop at three', () => {
  const answer = [
    '[첫 자료](https://EXAMPLE.com/one#top)',
    '같은 자료 https://example.com/one#bottom',
    '참고: https://example.com/two.',
    '<https://example.com/three>',
    '[제한 밖](https://example.com/four)',
  ].join('\n\n');
  assert.deepEqual(extractLinkPreviewUrls(answer), ['https://example.com/one', 'https://example.com/two', 'https://example.com/three']);
});

test('code blocks, inline code, image sources, raw HTML, quoted content and unused definitions are not fetched', () => {
  const answer = [
    '`https://example.com/inline`',
    '```html\nhttps://example.com/fenced\n```',
    '~~~\nhttps://example.com/tilde\n~~~',
    '    https://example.com/indented',
    '![https://example.com/image-label](https://example.com/image.png)',
    '![reference image][image-ref]',
    '<img src="https://example.com/html.png">',
    '<script>https://example.com/script</script>',
    '> https://example.com/quote',
    '> [quoted link](https://example.com/quote-link)',
    '[unused]: https://example.com/unused',
    '[image-ref]: https://example.com/image-reference.png',
    '[actual link](https://example.com/kept)',
  ].join('\n\n');
  assert.deepEqual(extractLinkPreviewUrls(answer), ['https://example.com/kept']);
});

test('used reference links resolve definitions once without fetching URL-like link labels', () => {
  const answer = [
    '[https://example.com/label][SOURCE]',
    '[same][source]',
    '[source]: https://example.com/destination#part "Title"',
    '[shortcut]',
    '[shortcut]: https://example.com/shortcut',
  ].join('\n\n');
  assert.deepEqual(extractLinkPreviewUrls(answer), ['https://example.com/destination', 'https://example.com/shortcut']);
});

test('bare links trim sentence punctuation while preserving balanced URL parentheses', () => {
  assert.deepEqual(extractLinkPreviewUrls('자료: https://example.com/wiki/Thing_(one). 다음 (https://example.com/two).'), ['https://example.com/wiki/Thing_(one)', 'https://example.com/two']);
});

test('invalid, unsafe and image URLs do not consume the answer cap', () => {
  assert.deepEqual(extractLinkPreviewUrls('[a](javascript:alert)\n\n[b](ftp://example.com)\n\n![image](https://example.com/image)\n\nhttps://example.com/one\n\nhttps://example.com/two\n\nhttps://example.com/three'), ['https://example.com/one', 'https://example.com/two', 'https://example.com/three']);
});

test('escaped destinations and nested link parentheses follow Markdown parsing', () => {
  assert.deepEqual(extractLinkPreviewUrls('[일반 문서](https://example.com/path_(nested))\n\n[a](https://example.com/a?x=1&y=2#section)'), ['https://example.com/path_(nested)', 'https://example.com/a?x=1&y=2']);
});

test('empty and code-only answers have no preview candidates', () => {
  assert.deepEqual(extractLinkPreviewUrls(''), []);
  assert.deepEqual(extractLinkPreviewUrls('```\nhttps://example.com/\n```'), []);
});
