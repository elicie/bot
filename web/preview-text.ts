import type { Nodes } from 'mdast';
import { unified } from 'unified';
import remarkParse from 'remark-parse';
import remarkCjkFriendly from 'remark-cjk-friendly';
import remarkMath from 'remark-math';
import { visualizationLanguages } from './visualization-document';

const parser = unified().use(remarkParse).use(remarkCjkFriendly).use(remarkMath);
const generatedImage = /(?:https?:\/\/[^\s/<>]+)?\/api\/generated-images\/[\w-]+(?:[?#][^\s<>]*)?/g;
const generatedImageLink = /^(?:https?:\/\/[^/]+)?\/api\/generated-images\//;
// Sidebar rerenders while typing and SSE replaces message objects. Cache by source, with bounded memory.
const cache = new Map<string, string>();
const maxCachedCharacters = 200_000;
let cachedCharacters = 0;

function visit(node: Nodes, callback: (node: Nodes) => void) {
  callback(node);
  if ('children' in node) node.children.forEach(child => visit(child, callback));
}

/** Protect TeX-style math from Markdown, without touching code or existing dollar math. */
function parsePreview(text: string) {
  const tree = parser.parse(text);
  const math: string[] = [];
  let prefix = 'PREVIEWMATH';
  if (!/\\[([]/.test(text)) return { tree, math, prefix };
  const literals: { start: number; end: number }[] = [];
  const quotes: { start: number; end: number }[] = [];
  const visibleText = plainText(tree, new Map());
  visit(tree, node => {
    if (!node.position) return;
    const range = { start: node.position.start.offset!, end: node.position.end.offset! };
    if (['code', 'inlineCode', 'math', 'inlineMath'].includes(node.type)) literals.push(range);
    if (node.type === 'blockquote') quotes.push(range);
  });
  while (text.includes(prefix) || visibleText.includes(prefix)) prefix += 'X';
  const pattern = /\\\(([\s\S]*?)\\\)|\\\[([\s\S]*?)\\\]/g;
  const parts: string[] = [];
  let end = 0;
  let match: RegExpExecArray | null;
  while ((match = pattern.exec(text))) {
    const offset = match.index;
    if (literals.some(range => offset >= range.start && offset < range.end)) {
      // Do not let a literal opener consume a later, genuine formula's closing delimiter.
      pattern.lastIndex = offset + 2;
      continue;
    }
    let value = match[1] ?? match[2];
    const depth = quotes.filter(range => offset >= range.start && offset < range.end).length;
    if (depth) value = value.replace(new RegExp(`\n(?:[ \\t]*>[ \\t]?){${depth}}`, 'g'), '\n');
    // Collision-free plain tokens also handle adjacent formulas and blank-line display math.
    math.push(value);
    parts.push(text.slice(end, offset), `${prefix}${math.length - 1}END`);
    end = pattern.lastIndex;
  }
  parts.push(text.slice(end));
  return { tree: math.length ? parser.parse(parts.join('')) : tree, math, prefix };
}

function plainText(node: Nodes, definitions: ReadonlyMap<string, string>): string {
  switch (node.type) {
    case 'text': return node.value.replace(generatedImage, '이미지');
    case 'inlineCode':
    case 'inlineMath': return node.value;
    case 'math': return ` ${node.value} `;
    case 'code': {
      const language = node.lang?.toLowerCase() ?? '';
      return language === 'mermaid' || Object.hasOwn(visualizationLanguages, language) ? ' 차트 ' : ' 코드 ';
    }
    case 'image':
    case 'imageReference': return '이미지';
    case 'link':
      if (generatedImageLink.test(node.url)) return '이미지';
      break;
    case 'linkReference':
      if (generatedImageLink.test(definitions.get(node.identifier) ?? '')) return '이미지';
      break;
    case 'break': return ' ';
    case 'html':
    case 'definition':
    case 'thematicBreak': return '';
  }
  if (!('children' in node)) return '';
  const separator = ['root', 'blockquote', 'list', 'listItem'].includes(node.type) ? ' ' : '';
  return node.children.map(child => plainText(child, definitions)).join(separator);
}

/** Plain, single-line conversation copy only; never renders HTML or changes stored messages. */
export function previewText(text: string): string {
  const cached = cache.get(text);
  if (cached !== undefined) return cached;
  const { tree, math, prefix } = parsePreview(text);
  const definitions = new Map<string, string>();
  visit(tree, node => { if (node.type === 'definition' && !definitions.has(node.identifier)) definitions.set(node.identifier, node.url); });
  const result = plainText(tree, definitions)
    .replace(new RegExp(`${prefix}(\\d+)END`, 'g'), (token, index: string) => math[Number(index)] ?? token)
    .replace(/\s+/g, ' ').trim();
  if (text.length <= maxCachedCharacters) {
    while (cache.size >= 100 || cachedCharacters + text.length > maxCachedCharacters) {
      const oldest = cache.keys().next().value!;
      cache.delete(oldest);
      cachedCharacters -= oldest.length;
    }
    cache.set(text, result);
    cachedCharacters += text.length;
  }
  return result;
}
