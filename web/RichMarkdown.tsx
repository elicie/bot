// Assistant Markdown with KaTeX math (`$…$`, `$$…$$`) and Mermaid diagrams (```mermaid fences).
//
// Safety: raw HTML in messages stays skipped. KaTeX runs with `trust: false` and its output enters
// the tree as parsed elements, never as an HTML string. Mermaid renders with `securityLevel:
// 'strict'` and the resulting SVG is shown through an <img> (a blob URL), where SVG cannot run
// scripts or reach the page. Both libraries load only when a message needs them, and anything
// that fails to render falls back to its source as code.

import { useEffect, useState, type ReactNode } from 'react';
import Markdown, { type Components, type Options } from 'react-markdown';
import './chat-features.css';

interface MathPlugins {
  remark: NonNullable<Options['remarkPlugins']>;
  rehype: NonNullable<Options['rehypePlugins']>;
}
let mathPlugins: MathPlugins | null = null;
let mathLoading: Promise<MathPlugins> | null = null;
const katexOptions = { trust: false, strict: 'ignore', maxSize: 40, maxExpand: 500, errorColor: 'inherit', output: 'htmlAndMathml' } as const;

function loadMath(): Promise<MathPlugins> {
  mathLoading ??= Promise.all([import('remark-math'), import('rehype-katex'), import('katex/dist/katex.min.css')]).then(([math, katex]) => {
    mathPlugins = { remark: [math.default], rehype: [[katex.default, katexOptions]] };
    return mathPlugins;
  });
  return mathLoading;
}

/** Cheap check before loading KaTeX: `$$…$$` or a `$…$` pair on one line. */
const mathPattern = /\$\$[\s\S]+?\$\$|\$[^\s$][^$\n]*\$/;
const maxDiagramSource = 20_000;

function useMathPlugins(text: string): MathPlugins | null {
  const needed = mathPattern.test(text);
  const [plugins, setPlugins] = useState(mathPlugins);
  useEffect(() => {
    if (!needed || plugins) return;
    let active = true;
    loadMath().then((loaded) => { if (active) setPlugins(loaded); }, () => undefined);
    return () => { active = false; };
  }, [needed, plugins]);
  return needed ? plugins : null;
}

type MermaidApi = typeof import('mermaid').default;
let mermaidLoading: Promise<MermaidApi> | null = null;
let diagramCount = 0;

function darkTheme(): boolean {
  const theme = document.documentElement.dataset.theme;
  return theme === 'light' ? false : theme === 'system' ? window.matchMedia('(prefers-color-scheme: dark)').matches : true;
}

function loadMermaid(): Promise<MermaidApi> {
  mermaidLoading ??= import('mermaid').then(({ default: mermaid }) => {
    mermaid.initialize({
      startOnLoad: false,
      securityLevel: 'strict',
      suppressErrorRendering: true,
      htmlLabels: false,
      flowchart: { htmlLabels: false },
      theme: darkTheme() ? 'dark' : 'default',
      fontFamily: 'system-ui, sans-serif',
      maxTextSize: maxDiagramSource,
    });
    return mermaid;
  });
  return mermaidLoading;
}

function CodeFallback({ source, note }: { source: string; note?: string }) {
  return <div className="diagram-fallback">
    {note && <small role="note">{note}</small>}
    <pre><code className="language-mermaid">{source}</code></pre>
  </div>;
}

function MermaidDiagram({ source }: { source: string }) {
  const [state, setState] = useState<{ status: 'loading' } | { status: 'ready'; url: string; label: string; width?: number; height?: number } | { status: 'error' }>({ status: 'loading' });
  useEffect(() => {
    if (source.length > maxDiagramSource) { setState({ status: 'error' }); return; }
    let active = true;
    let url: string | null = null;
    setState({ status: 'loading' });
    loadMermaid()
      .then(async (mermaid) => {
        await mermaid.parse(source);
        const { svg } = await mermaid.render(`grok-bot-diagram-${++diagramCount}`, source);
        if (!active) return;
        url = URL.createObjectURL(new Blob([svg], { type: 'image/svg+xml' }));
        const label = source.trim().split(/\s/)[0] ?? 'diagram';
        // The SVG is sized to its container (width="100%"); give the image its drawing size instead.
        const box = /viewBox="[-\d.]+[\s,]+[-\d.]+[\s,]+([\d.]+)[\s,]+([\d.]+)"/.exec(svg);
        const [width, height] = box ? [Math.ceil(Number(box[1])), Math.ceil(Number(box[2]))] : [undefined, undefined];
        setState({ status: 'ready', url, label, width, height });
      })
      .catch(() => { if (active) setState({ status: 'error' }); });
    return () => { active = false; if (url) URL.revokeObjectURL(url); };
  }, [source]);
  if (state.status === 'error') return <CodeFallback source={source} note="다이어그램을 표시할 수 없어 코드로 보여드려요." />;
  if (state.status === 'loading') return <div className="diagram-loading" role="status">다이어그램 그리는 중…</div>;
  return <figure className="mermaid-diagram" data-diagram={state.label}>
    <img src={state.url} alt={`Mermaid 다이어그램 (${state.label})`} width={state.width} height={state.height} />
  </figure>;
}

type HastNode = { type: string; value?: string; tagName?: string; properties?: { className?: unknown }; children?: HastNode[] };
const textOf = (node: HastNode): string => node.type === 'text' ? node.value ?? '' : (node.children ?? []).map(textOf).join('');

const components: Components = {
  a: ({ children, href }) => <a href={href} target="_blank" rel="noopener noreferrer">{children}</a>,
  img: ({ alt }) => <span className="markdown-image">[이미지{alt ? ` · ${alt}` : ''}]</span>,
  pre: ({ node, children }) => {
    const code = (node as HastNode | undefined)?.children?.find((child) => child.type === 'element' && child.tagName === 'code');
    const classes = code?.properties?.className;
    if (code && Array.isArray(classes) && classes.includes('language-mermaid')) return <MermaidDiagram source={textOf(code).replace(/\n$/, '')} />;
    return <pre>{children}</pre>;
  },
};

export function RichMarkdown({ text }: { text: string }): ReactNode {
  const math = useMathPlugins(text);
  return <Markdown skipHtml remarkPlugins={math?.remark} rehypePlugins={math?.rehype} components={components}>{text}</Markdown>;
}
