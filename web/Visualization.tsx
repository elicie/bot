// Visualization previews ("시각화"): ```html, ```svg, and ```visualization fences in assistant
// messages become a card that runs the source in a sandboxed frame, with "시각화 확대"
// (full-screen viewer, closed with "시각화 미리보기 닫기") and the code one click away.
// Security rules live in `visualization-document.ts`.

import { useCallback, useEffect, useMemo, useRef, useState, type RefCallback } from 'react';
import { Code2, Eye, Maximize2, RotateCcw } from 'lucide-react';
import {
  VISUALIZATION_ALLOW,
  VISUALIZATION_START_TIMEOUT_MS,
  buildVisualizationDocument,
  inlineHeight,
  inlineSafe,
  newNonce,
  readFrameMessage,
  sourceProblem,
  sourceTitle,
  visualizationLabels,
  wantsD3,
  type VisualizationKind,
} from './visualization-document';
import { IconButton } from './ui';
import { PreviewModal } from './Previews';
import './previews.css';

/** Wait this long after the last change before running a source (streaming answers). */
const SETTLE_MS = 400;

let d3Loading: Promise<string | null> | null = null;
/** The bundled d3 build (no network), inlined into documents that use d3. */
function loadD3(): Promise<string | null> {
  d3Loading ??= import('../node_modules/d3/dist/d3.min.js?raw').then(
    (module) => (inlineSafe(module.default) ? module.default : null),
    () => null,
  );
  return d3Loading;
}

type FrameStatus =
  | { kind: 'starting' }
  | { kind: 'ready' }
  | { kind: 'failed'; reason: 'timeout' | 'script' | 'navigation' | 'build'; detail?: string };

const failureTitles = {
  timeout: '이 시각화를 시작하는 데 10초 넘게 걸렸습니다',
  script: '이 시각화에서 스크립트 오류가 발생했어요',
  navigation: '이 시각화를 불러올 수 없어요',
  build: '이 시각화를 불러올 수 없어요',
} as const;

/** Debounced copy of `value`: the latest value once it stopped changing for `delay` ms. */
function useSettled<T>(value: T, delay: number): T {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    if (Object.is(value, settled)) return;
    const timer = setTimeout(() => setSettled(value), delay);
    return () => clearTimeout(timer);
  }, [value, settled, delay]);
  return settled;
}

/** True once the element has been near the viewport (frames start only when they could be seen). */
function useSeen<T extends Element>(): [RefCallback<T>, boolean] {
  const [seen, setSeen] = useState(typeof IntersectionObserver === 'undefined');
  const observer = useRef<IntersectionObserver | null>(null);
  const ref = useCallback((element: T | null) => {
    observer.current?.disconnect();
    observer.current = null;
    if (!element || seen) return;
    observer.current = new IntersectionObserver((entries) => {
      if (entries.some((entry) => entry.isIntersecting)) {
        setSeen(true);
        observer.current?.disconnect();
      }
    }, { rootMargin: '200px' });
    observer.current.observe(element);
  }, [seen]);
  useEffect(() => () => observer.current?.disconnect(), []);
  return [ref, seen];
}

/**
 * One sandboxed run of a source. A new nonce (and so a fresh frame) per mount or retry; the frame's
 * messages count only if they come from this frame's window and carry this nonce.
 */
function VisualizationFrame({ source, kind, title, fill, onEscape }: {
  source: string;
  kind: VisualizationKind;
  title: string;
  /** Fill the container (viewer) instead of sizing to the content (inline card). */
  fill?: boolean;
  onEscape?: () => void;
}) {
  const frame = useRef<HTMLIFrameElement>(null);
  const [attempt, setAttempt] = useState(0);
  const [status, setStatus] = useState<FrameStatus>({ kind: 'starting' });
  const [height, setHeight] = useState<number | null>(null);
  const [blocked, setBlocked] = useState(0);
  const [navigations, setNavigations] = useState(0);
  const [document_, setDocument] = useState<{ nonce: string; html: string } | null>(null);
  const loads = useRef(0);
  const escape = useRef(onEscape);
  escape.current = onEscape;
  const fail = useCallback((reason: Extract<FrameStatus, { kind: 'failed' }>['reason'], detail?: string) => {
    setStatus((current) => (current.kind === 'failed' ? current : { kind: 'failed', reason, detail }));
  }, []);

  // Build the document (loading d3 first when the source uses it).
  useEffect(() => {
    let active = true;
    setStatus({ kind: 'starting' });
    setDocument(null);
    setHeight(null);
    setBlocked(0);
    setNavigations(0);
    loads.current = 0;
    const nonce = newNonce();
    const runtime = wantsD3(kind, source) ? loadD3() : Promise.resolve(null);
    runtime
      .then((d3) => { if (active) setDocument({ nonce, html: buildVisualizationDocument(source, kind, nonce, d3 ?? undefined) }); })
      .catch(() => { if (active) fail('build'); });
    return () => { active = false; };
  }, [source, kind, attempt, fail]);

  // Startup timeout and frame messages for the current document.
  useEffect(() => {
    if (!document_) return;
    const timer = setTimeout(() => fail('timeout'), VISUALIZATION_START_TIMEOUT_MS);
    const listen = (event: MessageEvent) => {
      if (!frame.current || event.source !== frame.current.contentWindow) return;
      const message = readFrameMessage(event.data, document_.nonce);
      if (!message) return;
      switch (message.type) {
        case 'ready':
          clearTimeout(timer);
          setStatus((current) => (current.kind === 'starting' ? { kind: 'ready' } : current));
          break;
        case 'error':
          clearTimeout(timer);
          fail('script', message.message);
          break;
        case 'size':
          setHeight(message.height);
          break;
        case 'blocked':
          setBlocked(message.count);
          break;
        case 'escape':
          escape.current?.();
          break;
        case 'navigation':
          setNavigations((count) => count + 1);
          break;
      }
    };
    window.addEventListener('message', listen);
    return () => { clearTimeout(timer); window.removeEventListener('message', listen); };
  }, [document_, fail]);

  if (status.kind === 'failed') {
    return <div className="visualization-failure" role="alert">
      <strong>{failureTitles[status.reason]}</strong>
      {status.reason === 'navigation' && <span>다른 페이지로 이동하려는 시각화는 표시하지 않아요.</span>}
      {status.reason === 'timeout' && <span>무한 반복이나 오래 걸리는 계산이 없는지 코드를 확인해 주세요. 다른 시각화도 멈췄다면 페이지를 새로고침해 주세요.</span>}
      {status.detail && <code>{status.detail}</code>}
      <button type="button" className="small-button" onClick={() => setAttempt((value) => value + 1)}><RotateCcw size={13} />다시 시도</button>
    </div>;
  }
  return <div className={`visualization-stage ${fill ? 'is-fill' : ''}`}>
    {status.kind === 'starting' && <div className="visualization-loading" role="status">시각화를 시작하는 중…</div>}
    {document_ && <iframe
      key={document_.nonce}
      ref={frame}
      className="visualization-frame"
      title={title}
      sandbox="allow-scripts"
      allow={VISUALIZATION_ALLOW}
      referrerPolicy="no-referrer"
      srcDoc={document_.html}
      style={fill ? undefined : { height: inlineHeight(height ?? 0) }}
      onLoad={() => {
        // The srcdoc loads once. Another load means a navigation got past the bootstrap's guard (the
        // app's `frame-src 'none'` turns it into an error page), so stop showing it.
        loads.current += 1;
        if (loads.current > 1) fail('navigation');
      }}
    />}
    {status.kind === 'ready' && (blocked > 0 || navigations > 0) && <small className="visualization-blocked" role="note">
      {[blocked > 0 && `외부 리소스 요청 ${blocked}개를 차단했어요.`, navigations > 0 && '시각화 안의 링크는 열 수 없어요.'].filter(Boolean).join(' ')}
    </small>}
  </div>;
}

export function VisualizationCard({ source, kind }: { source: string; kind: VisualizationKind }) {
  const settled = useSettled(source, SETTLE_MS);
  const [view, setView] = useState<'preview' | 'code'>('preview');
  const [expanded, setExpanded] = useState(false);
  const [seenRef, seen] = useSeen<HTMLDivElement>();
  const problem = sourceProblem(source);
  const title = useMemo(() => (kind === 'svg' ? null : sourceTitle(settled)) ?? visualizationLabels[kind], [kind, settled]);
  const settling = settled !== source;
  const code = <pre className="visualization-code"><code className={`language-${kind === 'svg' ? 'svg' : 'html'}`}>{source}</code></pre>;
  return <figure className="visualization-card" data-visualization={kind} aria-label={title}>
    <figcaption className="visualization-header">
      <span className="visualization-title" title={title}>{title}</span>
      <div className="visualization-actions">
        {!problem && <button type="button" className="visualization-toggle" aria-pressed={view === 'code'} onClick={() => setView(view === 'code' ? 'preview' : 'code')}>
          {view === 'code' ? <><Eye size={13} />미리보기</> : <><Code2 size={13} />코드 보기</>}
        </button>}
        {!problem && <IconButton label="시각화 확대" onClick={() => setExpanded(true)} disabled={settling}><Maximize2 size={14} /></IconButton>}
      </div>
    </figcaption>
    {problem ? <>
      <div className="visualization-failure" role="note">
        <strong>이 시각화는 사용할 수 없습니다</strong>
        <span>{problem === 'too-large' ? '미리보기로 실행하기에는 코드가 너무 길어요. 코드로 보여드려요.' : '표시할 내용이 없어요.'}</span>
      </div>
      {problem === 'too-large' && code}
    </> : view === 'code' ? code : <div ref={seenRef} className="visualization-body">
      {settling || !seen
        ? <div className="visualization-loading" role="status">{settling ? '시각화를 준비하는 중…' : '시각화를 시작하는 중…'}</div>
        : <VisualizationFrame source={settled} kind={kind} title={title} />}
    </div>}
    {expanded && !problem && <PreviewModal title="시각화 미리보기" closeLabel="시각화 미리보기 닫기" className="visualization-viewer" onClose={() => setExpanded(false)}
      heading={title}>
      {(close) => <VisualizationFrame source={settled} kind={kind} title={title} fill onEscape={close} />}
    </PreviewModal>}
  </figure>;
}

