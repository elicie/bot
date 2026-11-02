import test from 'node:test';
import assert from 'node:assert/strict';
import { previewText } from '../web/preview-text.js';
import { conversationPreview, runLabels } from '../web/state.js';
import type { Conversation, Message, RunStatus } from '../shared/types.js';

const image = '/api/generated-images/12345678-1234-1234-1234-123456789abc';
const cases: [string, string, string][] = [
  ['issue example', '\\(23 = 20 + 3\\)으로 나누어 계산...', '23 = 20 + 3으로 나누어 계산...'],
  ['TeX delimiters', '\\[a_i * b_j\\]와 \\(\\frac{1}{2}\\)', 'a_i * b_j와 \\frac{1}{2}'],
  ['dollar math', '$x_i * y_j$와 $$E=mc^2$$', 'x_i * y_j와 E=mc^2'],
  ['display math', '$$\n\\int_0^1 x\\,dx = \\frac{1}{2}\n$$', '\\int_0^1 x\\,dx = \\frac{1}{2}'],
  ['headings and quotes', '# 제목\n\n> **굵게**와 *강조*\n> 다음 줄', '제목 굵게와 강조 다음 줄'],
  ['CJK emphasis', '정답은 **6월(25)**입니다. **“백서”**와 __굵게__', '정답은 6월(25)입니다. “백서”와 굵게'],
  ['lists and rules', '- 첫째\n- 둘째\n\n---\n\n1. 셋째', '첫째 둘째 셋째'],
  ['inline code', '`file_name`과 ``a ` b``', 'file_name과 a ` b'],
  ['literal inline math source', '`\\(x_i\\)`과 `price $5`', '\\(x_i\\)과 price $5'],
  ['math with backticks', '\\(a ` b\\)', 'a ` b'],
  ['adjacent math', '\\(a\\)\\(b\\)\\[c\\]', 'abc'],
  ['display math with paragraphs', '\\[\na_i = b_j\n\nc_i = d_j\n\\]', 'a_i = b_j c_i = d_j'],
  ['Markdown-looking TeX content', '\\(\\text{`sample`} + x_i\\)와 \\(\\text{$5 and $10} + x_i\\)', '\\text{`sample`} + x_i와 \\text{$5 and $10} + x_i'],
  ['placeholder collision', 'PREVIEWMATH0END \\(x\\)', 'PREVIEWMATH0END x'],
  ['encoded placeholder collision', '&#80;REVIEWMATH0END \\(x\\)', 'PREVIEWMATH0END x'],
  ['formatted placeholder collision', 'PREVIEW**MATH**0END \\(x\\)', 'PREVIEWMATH0END x'],
  ['quoted display math', '> \\[\n> x_i = y_j\n> \\]', 'x_i = y_j'],
  ['quoted comparison operator', '> \\[\n> x > y\n> \\]', 'x > y'],
  ['leading quoted inline comparison', '> \\(> 10\\)', '> 10'],
  ['leading quoted display comparison', '> \\[> 10\\]', '> 10'],
  ['nested quoted math', '> > \\[\n> > x > y\n> > \\]', 'x > y'],
  ['literal opener before real math', '예시: `\\(` 여는 기호, 실제 \\(23 = 20 + 3\\)', '예시: \\( 여는 기호, 실제 23 = 20 + 3'],
  ['escaped URL parentheses', '[문서](https://example.com/\\(x\\))', '문서'],
  ['links with nested URL parentheses', '[안내](https://example.com/a_(b) "제목")와 [문서][ref]\n\n[ref]: https://example.com', '안내와 문서'],
  ['generated Markdown image', `![생성 결과](${image})`, '이미지'],
  ['generated Markdown link', `[다운로드](${image})`, '이미지'],
  ['generated reference link', `[다운로드][img]\n\n[img]: ${image}`, '이미지'],
  ['generated nested reference definition', `[다운로드][img]\n\n> [img]: ${image}`, '이미지'],
  ['generated bare URL', `생성: ${image}`, '생성: 이미지'],
  ['generated autolink', `<https://example.com${image}>`, '이미지'],
  ['ordinary image', '![alt](https://example.com/image.png)', '이미지'],
  ['fenced code', '앞\n\n```ts\nconst secret = "**raw**";\n```\n\n뒤', '앞 코드 뒤'],
  ['indented code', '    console.log("raw")', '코드'],
  ['tilde fence', '~~~python\nprint("raw")\n~~~', '코드'],
  ['unfinished code fence', '```ts\nconst raw = true;', '코드'],
  ['literal delimiters inside block', '```ts\nconst math = "\\(x\\)";\n```', '코드'],
  ['HTML is not rendered', '<script>alert("raw")</script>\n\n안전한 <b>텍스트</b>', '안전한 텍스트'],
  ['entities, escaped symbols and currency', 'A &amp; B \\*literal\\*와 \\$5, snake_case', 'A & B *literal*와 $5, snake_case'],
  ['whitespace', '  첫 줄\n\n다음\t줄  ', '첫 줄 다음 줄'],
  ['empty formatting', '#\n\n---\n\n[ref]: https://example.com', ''],
];
for (const [name, input, expected] of cases) {
  test(`preview text: ${name}`, () => assert.equal(previewText(input), expected));
}
for (const language of ['mermaid', 'html', 'svg', 'visualization', 'HTML']) {
  test(`preview text: ${language} visualization is a short chart label`, () => {
    assert.equal(previewText(`\`\`\`${language}\nraw source\n\`\`\``), '차트');
  });
}

function message(text: string, extra: Partial<Message> = {}): Message {
  return { id: 'message', role: 'assistant', text, tools: [], createdAt: '2026-11-02T00:00:00Z', ...extra };
}
function conversation(messages: Message[], status: RunStatus = 'completed'): Conversation {
  return { id: 'conversation', botId: 'bot', title: '', updatedAt: '2026-11-02T00:00:00Z', messages, run: { id: 'run', status } };
}

test('conversation preview formats the latest visible message without mutating stored text', () => {
  const input = conversation([message('이전'), message('**굵게**'), message(''), message('숨긴 보고', { reportDeliveryIds: ['delivery'] })]);
  const original = structuredClone(input);
  assert.equal(conversationPreview(input), '굵게');
  assert.deepEqual(input, original);
  assert.equal(conversationPreview(conversation([message('**사용자**', { role: 'user' })])), '사용자');
});

test('conversation preview retains active, error, and empty state copy', () => {
  for (const status of ['starting', 'running', 'awaiting_permission'] as const) {
    assert.equal(conversationPreview(conversation([message('**이전**')], status)), runLabels[status]);
  }
  const failed = conversation([message('**이전**')], 'error');
  assert.equal(conversationPreview(failed), '실행 오류');
  failed.run.error = '실제 오류';
  assert.equal(conversationPreview(failed), '실제 오류');
  assert.equal(conversationPreview(undefined), '새 대화를 시작해 보세요');
  for (const messages of [[], [message('  ')], [message('---')], [message('숨김', { reportDeliveryIds: [] })]]) {
    assert.equal(conversationPreview(conversation(messages)), '아직 메시지가 없습니다');
  }
});
