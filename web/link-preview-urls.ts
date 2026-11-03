import { fromMarkdown } from 'mdast-util-from-markdown';

export const MAX_LINK_PREVIEW_CARDS = 3;
const MAX_URL_LENGTH = 4096;

/** A request/cache key, not a trust boundary: the server still validates public destinations. */
export function canonicalLinkPreviewUrl(raw: string): string | null {
  const value = raw.trim();
  if (value.length > MAX_URL_LENGTH || !/^https?:\/\//i.test(value) || /[\s\u0000-\u001f\u007f\\]/.test(value)) return null;
  try {
    const url = new URL(value);
    if (!['http:', 'https:'].includes(url.protocol) || !url.hostname || url.username || url.password) return null;
    url.hash = '';
    return url.href;
  } catch {
    return null;
  }
}

type MarkdownNode = {
  type: string;
  value?: string;
  url?: string;
  identifier?: string;
  children?: MarkdownNode[];
};
const referenceKey = (value: string) => value.trim().replace(/\s+/g, ' ').toLowerCase();
const ignoredNodes = new Set(['code', 'inlineCode', 'image', 'imageReference', 'html', 'blockquote']);

function trimBareUrl(value: string): string {
  // Sentence punctuation is not part of a bare URL. Keep balanced URL parentheses/brackets.
  let result = value.split(/[。．、，！？；：]/, 1)[0];
  let previous: string;
  do {
    previous = result;
    result = result.replace(/[.,!?;:]+$/, '');
    for (const [open, close] of [['(', ')'], ['[', ']'], ['{', '}']]) {
      while (result.endsWith(close) && result.split(close).length > result.split(open).length) result = result.slice(0, -1);
    }
  } while (result !== previous);
  return result;
}

/**
 * First three distinct http(s) destinations in reading order. Parse the same CommonMark syntax
 * as RichMarkdown so code, image targets/alts, raw HTML, unused definitions and quoted content
 * never cause preview requests. References count where used, not where defined.
 */
export function extractLinkPreviewUrls(text: string): string[] {
  const root: MarkdownNode = fromMarkdown(text);
  const definitions = new Map<string, string>();
  const collectDefinitions = (node: MarkdownNode) => {
    if (ignoredNodes.has(node.type)) return;
    if (node.type === 'definition' && node.identifier && node.url) {
      const key = referenceKey(node.identifier);
      if (!definitions.has(key)) definitions.set(key, node.url);
    }
    node.children?.forEach(collectDefinitions);
  };
  collectDefinitions(root);
  const urls: string[] = [];
  const seen = new Set<string>();
  const add = (raw: string) => {
    const url = canonicalLinkPreviewUrl(raw);
    if (!url || seen.has(url) || urls.length >= MAX_LINK_PREVIEW_CARDS) return;
    seen.add(url);
    urls.push(url);
  };
  const visit = (node: MarkdownNode) => {
    if (urls.length >= MAX_LINK_PREVIEW_CARDS || ignoredNodes.has(node.type) || node.type === 'definition') return;
    if (node.type === 'link') {
      if (node.url) add(node.url);
      return; // A URL-looking label is not a second destination.
    }
    if (node.type === 'linkReference') {
      const url = definitions.get(referenceKey(node.identifier ?? ''));
      if (url) add(url);
      return;
    }
    if (node.type === 'text' && node.value) {
      const bare = /(?<![\p{L}\p{N}_])https?:\/\/[^\s<>"'`“”‘’「」『』〈〉《》«»]+/giu;
      for (const match of node.value.matchAll(bare)) add(trimBareUrl(match[0]));
    }
    node.children?.forEach(visit);
  };
  visit(root);
  return urls;
}
