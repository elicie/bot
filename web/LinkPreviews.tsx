// Opt-in, settled assistant-answer previews. Only our own API is requested by the browser;
// untrusted metadata is rendered as React text and thumbnails must be local generated images.
import { createContext, useContext, useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { ArrowUpRight, Link2 } from 'lucide-react';
import { request } from './api';
import { useMediaUrl } from './use-media-url';
import { canonicalLinkPreviewUrl, extractLinkPreviewUrls } from './link-preview-urls';
import './link-previews.css';

interface LinkPreview {
  url: string;
  title: string;
  description: string;
  imageUrl?: string;
}
type PreviewState = { status: 'loading' } | { status: 'ready'; preview: LinkPreview } | { status: 'error' };
type Listener = (state: PreviewState) => void;
interface CacheEntry {
  state: PreviewState;
  expiresAt: number;
  listeners: Set<Listener>;
  controller?: AbortController;
  timeout?: ReturnType<typeof setTimeout>;
}
const CACHE_LIMIT = 100;
const SUCCESS_TTL = 5 * 60_000;
const FAILURE_TTL = 30_000;
const REQUEST_TIMEOUT = 15_000;
const generatedThumbnail = /^\/api\/generated-images\/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

function readPreview(value: unknown): LinkPreview {
  if (!value || typeof value !== 'object' || !('url' in value) || typeof value.url !== 'string' ||
    !('title' in value) || typeof value.title !== 'string' || !('description' in value) || typeof value.description !== 'string') throw new Error('Invalid preview');
  const url = canonicalLinkPreviewUrl(value.url);
  if (!url) throw new Error('Invalid preview destination');
  const imageUrl = 'imageUrl' in value && typeof value.imageUrl === 'string' && generatedThumbnail.test(value.imageUrl) ? value.imageUrl : undefined;
  return { url, title: value.title.trim().slice(0, 300), description: value.description.trim().slice(0, 1000), ...(imageUrl ? { imageUrl } : {}) };
}

/** Provider-scoped, bounded LRU. Subscribers share both pending requests and short-lived results. */
class LinkPreviewCache {
  private enabled = false;
  private entries = new Map<string, CacheEntry>();

  setEnabled(enabled: boolean) {
    this.enabled = enabled;
    if (!enabled) this.clear();
  }

  clear() {
    this.enabled = false;
    for (const entry of this.entries.values()) {
      entry.controller?.abort();
      clearTimeout(entry.timeout);
      entry.listeners.clear();
    }
    this.entries.clear();
  }

  subscribe(url: string, listener: Listener): () => void {
    if (!this.enabled) return () => undefined;
    const now = Date.now();
    for (const [key, entry] of this.entries) {
      if (entry.state.status !== 'loading' && entry.expiresAt <= now) this.entries.delete(key);
    }
    let entry = this.entries.get(url);
    if (!entry) {
      // Never evict an observed request. If the visible surface itself reaches the limit,
      // fall back to the original link rather than creating an unbounded request/cache set.
      while (this.entries.size >= CACHE_LIMIT) {
        const oldest = [...this.entries].find(([, cached]) => cached.state.status !== 'loading' || !cached.listeners.size);
        if (!oldest) { listener({ status: 'error' }); return () => undefined; }
        oldest[1].controller?.abort();
        clearTimeout(oldest[1].timeout);
        this.entries.delete(oldest[0]);
      }
      entry = { state: { status: 'loading' }, expiresAt: Infinity, listeners: new Set() };
      this.entries.set(url, entry);
      this.load(url, entry);
    } else {
      this.entries.delete(url);
      this.entries.set(url, entry);
    }
    const observed = entry;
    observed.listeners.add(listener);
    listener(observed.state);
    return () => {
      observed.listeners.delete(listener);
      // StrictMode's effect replay and another occurrence in the same commit can reuse a
      // request. Once its final real subscriber leaves, abort it and discard the entry.
      queueMicrotask(() => {
        if (this.entries.get(url) !== observed || observed.listeners.size || !observed.controller) return;
        observed.controller.abort();
        clearTimeout(observed.timeout);
        this.entries.delete(url);
      });
    };
  }

  private load(url: string, entry: CacheEntry) {
    const controller = new AbortController();
    entry.controller = controller;
    entry.timeout = setTimeout(() => controller.abort(), REQUEST_TIMEOUT);
    const settle = (state: PreviewState) => {
      clearTimeout(entry.timeout);
      if (!this.enabled || this.entries.get(url) !== entry) return;
      entry.controller = undefined;
      entry.state = state;
      entry.expiresAt = Date.now() + (state.status === 'ready' ? SUCCESS_TTL : FAILURE_TTL);
      for (const listener of entry.listeners) listener(state);
    };
    void request<unknown>(`/api/link-preview?url=${encodeURIComponent(url)}`, { signal: controller.signal, headers: { 'X-Grok-Bot': '1' } })
      .then(value => settle({ status: 'ready', preview: readPreview(value) }))
      .catch(() => settle({ status: 'error' }));
  }
}

const LinkPreviewContext = createContext<{ enabled: boolean; cache: LinkPreviewCache } | null>(null);

export function LinkPreviewProvider({ enabled, children }: { enabled: boolean; children: ReactNode }) {
  const [cache] = useState(() => new LinkPreviewCache());
  // Run before card effects. Disabling unmounts every group in the same render, then aborts
  // and clears the cache before paint, including completed previews and thumbnail elements.
  useLayoutEffect(() => {
    cache.setEnabled(enabled);
    return () => cache.clear();
  }, [cache, enabled]);
  const context = useMemo(() => ({ enabled, cache }), [enabled, cache]);
  return <LinkPreviewContext.Provider value={context}>{children}</LinkPreviewContext.Provider>;
}

function LinkPreviewCard({ url, cache, visible }: { url: string; cache: LinkPreviewCache; visible: boolean }) {
  const [state, setState] = useState<PreviewState>({ status: 'loading' });
  const [imageFailed, setImageFailed] = useState(false);
  const descriptionId = useId();
  useEffect(() => {
    if (!visible) return;
    return cache.subscribe(url, setState);
  }, [url, cache, visible]);
  const preview = state.status === 'ready' ? state.preview : null;
  const image = useMediaUrl(visible && !imageFailed ? preview?.imageUrl ?? null : null);
  const destination = preview?.url ?? url;
  const host = new URL(destination).host;
  const title = preview?.title || host;
  const description = state.status === 'error'
    ? '미리보기를 불러오지 못했습니다. 링크로 이동할 수 있습니다.'
    : preview ? preview.description || '페이지 설명이 없습니다.' : visible ? '미리보기 불러오는 중…' : '링크 미리보기';
  return <li>
    <a className={`link-preview-card is-${state.status}`} href={destination} target="_blank" rel="noopener noreferrer" referrerPolicy="no-referrer" aria-label={`${title} (새 탭에서 열기)`} aria-describedby={descriptionId}>
      <span className="link-preview-thumbnail" aria-hidden="true">
        {image.url && !imageFailed
          ? <img src={image.url} alt="" loading="lazy" decoding="async" referrerPolicy="no-referrer" onError={() => setImageFailed(true)} />
          : <Link2 size={21} />}
      </span>
      <span className="link-preview-copy">
        <span className="link-preview-host">{host}<ArrowUpRight size={12} aria-hidden="true" /></span>
        <strong className="link-preview-title">{title}</strong>
        <span className="link-preview-description" id={descriptionId}>{description}</span>
      </span>
    </a>
  </li>;
}

function VisibleLinkPreviewGroup({ urls, cache }: { urls: string[]; cache: LinkPreviewCache }) {
  const ref = useRef<HTMLDivElement>(null);
  const [visible, setVisible] = useState(false);
  useEffect(() => {
    const element = ref.current;
    if (!element) return;
    if (typeof IntersectionObserver === 'undefined') { setVisible(true); return; }
    const observer = new IntersectionObserver(entries => {
      if (!entries.some(entry => entry.isIntersecting)) return;
      setVisible(true);
      observer.disconnect();
    }, { root: element.closest('.transcript'), rootMargin: '100px 0px' });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);
  return <div className="link-preview-group" role="group" aria-label="링크 미리보기" ref={ref}>
    <ul className="link-preview-list">{urls.map(url => <LinkPreviewCard key={url} url={url} cache={cache} visible={visible} />)}</ul>
  </div>;
}

/** Explicit permission from MessageView is required, even inside the opt-in provider. */
export function LinkPreviewGroup({ text, settled }: { text: string; settled: boolean }) {
  const context = useContext(LinkPreviewContext);
  const eligible = context?.enabled === true && settled;
  const urls = useMemo(() => eligible ? extractLinkPreviewUrls(text) : [], [eligible, text]);
  if (!context || !eligible || !urls.length) return null;
  return <VisibleLinkPreviewGroup urls={urls} cache={context.cache} />;
}
