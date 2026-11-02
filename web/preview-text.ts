import type { Nodes } from 'mdast';
import { unified } from 'unified';
import remarkParse from 'remark-parse';
import remarkCjkFriendly from 'remark-cjk-friendly';
import { visualizationLanguages } from './visualization-document';

const parser = unified().use(remarkParse).use(remarkCjkFriendly);
export const MAX_PREVIEW_SOURCE = 2048;
const maxPreviewLength = 280;
const cache = new Map<string, { source: string; result: string }>();
const maxCachedMessages = 2048;
const generatedImage = /(?:https?:\/\/[^\s/<>]+)?\/api\/generated-images\/[\w-]+(?:[?#][^\s<>]*)?/g;
const generatedImageLink = /^(?:https?:\/\/[^/]+)?\/api\/generated-images\//;

type Range = { start: number; end: number; block?: boolean };
function visit(node: Nodes, callback: (node: Nodes) => void) {
  callback(node);
  if ('children' in node) node.children.forEach(child => visit(child, callback));
}

// Even a short source can contain pathological nesting. Reject it before entering Markdown.
function deepContainers(source: string): boolean {
  return source.split('\n').some(line => {
    let rest = line.trimStart();
    for (let depth = 0; ; depth++) {
      const marker = /^(?:>|[-+*](?=[ \t])|\d{1,9}[.)](?=[ \t]))/.exec(rest);
      if (!marker) return false;
      if (depth >= 16) return true;
      rest = rest.slice(marker[0].length).trimStart();
    }
  });
}

/** A single forward scan: unmatched openers never search the remaining source repeatedly. */
function parsePreview(text: string) {
  const tree = parser.parse(text);
  const math = new Map<string, string>();
  if (!/[\\$]/.test(text)) return { tree, math, source: text };
  const literals: Range[] = [];
  const quotes: Range[] = [];
  const boundaries = new Set<number>();
  visit(tree, node => {
    if (!node.position) return;
    const range = { start: node.position.start.offset!, end: node.position.end.offset!, block: !['inlineCode', 'html'].includes(node.type) };
    if (['code', 'inlineCode', 'html', 'image', 'imageReference', 'definition', 'linkReference'].includes(node.type)) literals.push(range);
    if (node.type === 'link') {
      // A label is its own prose region; destinations/titles must not participate in math.
      boundaries.add(range.start);
      const labelEnd = node.children.at(-1)?.position?.end.offset;
      literals.push({ ...range, start: text[range.start] === '<' ? range.start : labelEnd ?? range.start });
    }
    if (node.type === 'blockquote') quotes.push(range);
  });
  literals.sort((a, b) => a.start - b.start || b.end - a.end);
  const occupied = new Set(text + plainText(tree, new Map(), text));
  const tokens: { index: number; token: string; block?: boolean }[] = [];
  const lastClose = new Map<string, number>();
  let literalIndex = 0;
  for (let index = 0; index < text.length;) {
    if (boundaries.has(index)) tokens.push({ index, token: '', block: true });
    while (literals[literalIndex]?.end <= index) literalIndex++;
    const literal = literals[literalIndex];
    if (literal && literal.start <= index) {
      if (literal.block) tokens.push({ index, token: '', block: true });
      index = literal.end;
      continue;
    }
    let token = text[index];
    if (token === '\\') token = text.slice(index, index + 2);
    else if (token === '$') while (text[index + token.length] === '$') token += '$';
    if (token.startsWith('\\') || token.startsWith('$')) {
      tokens.push({ index, token });
      if (token === '\\)' || token === '\\]' || token.startsWith('$')) lastClose.set(token, index);
    }
    index += token.length;
  }
  const parts: string[] = [];
  let end = 0;
  let marker = 0xe000;
  let open: { start: number; body: number; close: string } | undefined;
  for (const { index, token, block } of tokens) {
    if (block) { open = undefined; continue; } // Pair math only within one visible prose/label region.
    if (open && token === open.close) {
      let value = text.slice(open.body, index);
      const depth = quotes.filter(range => open!.start >= range.start && open!.start < range.end).length;
      if (depth) value = value.replace(new RegExp(`\n(?:[ \\t]*>[ \\t]?){${depth}}`, 'g'), '\n');
      if (token !== '$' || !/\n[ \t]*\n/.test(value)) {
        while (occupied.has(String.fromCharCode(marker))) marker++;
        const placeholder = String.fromCharCode(marker++);
        math.set(placeholder, value);
        parts.push(text.slice(end, open.start), placeholder);
        end = index + token.length;
      }
      open = undefined;
    } else if (!open && (token === '\\(' || token === '\\[' || token.startsWith('$'))) {
      const close = token === '\\(' ? '\\)' : token === '\\[' ? '\\]' : token;
      // Unmatched currency/openers must not hide a later, differently-delimited formula.
      if ((lastClose.get(close) ?? -1) > index) open = { start: index, body: index + token.length, close };
    }
  }
  parts.push(text.slice(end));
  const source = parts.join('');
  return { tree: math.size ? parser.parse(source) : tree, math, source };
}

function plainText(node: Nodes, definitions: ReadonlyMap<string, string>, source: string): string {
  switch (node.type) {
    case 'text': return node.value.replace(generatedImage, '이미지');
    case 'inlineCode': return node.value;
    case 'code': {
      const language = node.lang?.toLowerCase() ?? '';
      return language === 'mermaid' || Object.hasOwn(visualizationLanguages, language) ? ' 차트 ' : ' 코드 ';
    }
    case 'image':
    case 'imageReference': return '이미지';
    case 'link':
      if (generatedImageLink.test(node.url)) return '이미지';
      break;
    case 'linkReference': {
      if (generatedImageLink.test(definitions.get(node.identifier) ?? '')) return '이미지';
      // Keep the original collapsed/shortcut identifier; format only its visible label.
      const start = node.children[0]?.position?.start.offset;
      const end = node.children.at(-1)?.position?.end.offset;
      if (start !== undefined && end !== undefined) return previewText(source.slice(start, end));
      break;
    }
    case 'break': return ' ';
    case 'html':
    case 'definition':
    case 'thematicBreak': return '';
  }
  if (!('children' in node)) return '';
  const separator = ['root', 'blockquote', 'list', 'listItem'].includes(node.type) ? ' ' : '';
  return node.children.map(child => plainText(child, definitions, source)).join(separator);
}

/** Bounded plain-text copy. The message-ID cache survives fresh SSE objects without retaining full replies. */
export function previewText(text: string, messageId?: string): string {
  let source = text.slice(0, MAX_PREVIEW_SOURCE);
  const cached = messageId ? cache.get(messageId) : undefined;
  if (cached?.source === source) return cached.result;
  // Copy only on a miss so engines cannot retain a full reply behind a cached substring.
  source = source.split('').join('');
  let result = '메시지';
  try {
    if (!deepContainers(source)) {
      const { tree, math, source: parsedSource } = parsePreview(source);
      const definitions = new Map<string, string>();
      visit(tree, node => { if (node.type === 'definition' && !definitions.has(node.identifier)) definitions.set(node.identifier, node.url); });
      result = plainText(tree, definitions, parsedSource)
        .replace(/[\ue000-\uf8ff]/g, token => math.get(token) ?? token)
        .replace(/\s+/g, ' ').trim().slice(0, maxPreviewLength);
    }
  } catch { /* A malformed message must not prevent the sidebar/history from opening. */ }
  if (messageId) {
    cache.delete(messageId);
    if (cache.size >= maxCachedMessages) cache.delete(cache.keys().next().value!);
    cache.set(messageId, { source, result });
  }
  return result;
}
