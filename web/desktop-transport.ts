// Optional desktop transport. The native host injects this context before the app loads; ordinary
// web pages keep their native fetch/EventSource/media behavior. Never put this token in a URL.
export interface DesktopContext {
  readonly origin: string;
  readonly token: string;
}

declare global {
  interface Window {
    readonly __GROK_DESKTOP__?: Readonly<DesktopContext>;
  }
}

export const DESKTOP_TOKEN_HEADER = 'X-Grok-Desktop-Token';

/** An injected context is usable only in the exact loopback top-level document it belongs to. */
export function desktopContext(): Readonly<DesktopContext> | null {
  if (typeof window === 'undefined') return null;
  try {
    if (window.top !== window) return null;
    const context = window.__GROK_DESKTOP__;
    if (!context || !Object.isFrozen(context) || typeof context.origin !== 'string' || typeof context.token !== 'string') return null;
    const origin = new URL(context.origin);
    if (origin.protocol !== 'http:' || origin.hostname !== '127.0.0.1' || origin.origin !== context.origin
      || origin.username || origin.password || origin.search || origin.hash || origin.pathname !== '/'
      || window.location.origin !== context.origin || !/^[a-f0-9]{64}$/i.test(context.token)) return null;
    return context;
  } catch {
    return null;
  }
}

function apiUrl(input: RequestInfo | URL, context: Readonly<DesktopContext>): URL | null {
  try {
    const address = typeof input === 'string' || input instanceof URL ? input : input.url;
    const url = new URL(address, window.location.href);
    return url.origin === context.origin && !url.username && !url.password
      && (url.pathname === '/api' || url.pathname.startsWith('/api/')) ? url : null;
  } catch {
    return null;
  }
}

export function needsDesktopAuthentication(input: RequestInfo | URL): boolean {
  const context = desktopContext();
  return context !== null && apiUrl(input, context) !== null;
}

/** Do not globally patch fetch: credentials belong only to the desktop's own API, never redirects. */
export function apiFetch(input: RequestInfo | URL, init: RequestInit = {}): Promise<Response> {
  const context = desktopContext();
  const headers = new Headers(init.headers ?? (input instanceof Request ? input.headers : undefined));
  if (context && apiUrl(input, context)) {
    headers.set(DESKTOP_TOKEN_HEADER, context.token);
    // A custom header can otherwise survive a redirect to another origin. API endpoints do not
    // redirect; fail closed instead of following any redirect with the desktop credential.
    // Keep fetch's CORS mode even for this exact same-origin URL: with no-referrer,
    // WebKit serializes non-GET same-origin-mode requests with Origin: null.
    // URL scoping above plus redirect:'error' still prevent forwarding this secret elsewhere.
    return fetch(input, { ...init, headers, redirect: 'error', mode: 'cors' });
  }
  if (headers.has(DESKTOP_TOKEN_HEADER)) {
    headers.delete(DESKTOP_TOKEN_HEADER);
    return fetch(input, { ...init, headers });
  }
  return fetch(input, init);
}

/** Incremental SSE framing; JSON interpretation remains the existing application's responsibility. */
export function stateEventParser(onState: (data: string) => void): (chunk: string) => void {
  let pending: string[] = [];
  let skipLineFeed = false;
  let event = '';
  let data: string[] = [];
  const line = (value: string) => {
    if (!value) {
      if (event === 'state' && data.length) onState(data.join('\n'));
      event = '';
      data = [];
      return;
    }
    if (value.startsWith(':')) return;
    const colon = value.indexOf(':');
    const field = colon < 0 ? value : value.slice(0, colon);
    let text = colon < 0 ? '' : value.slice(colon + 1);
    if (text.startsWith(' ')) text = text.slice(1);
    if (field === 'event') event = text;
    if (field === 'data') data.push(text);
  };
  return (chunk) => {
    if (!chunk) return;
    if (skipLineFeed) {
      if (chunk.startsWith('\n')) chunk = chunk.slice(1);
      skipLineFeed = false;
    }
    const newline = /[\r\n]/g;
    let start = 0;
    for (let match = newline.exec(chunk); match; match = newline.exec(chunk)) {
      const part = chunk.slice(start, match.index);
      line(pending.length ? pending.join('') + part : part);
      pending = [];
      if (match[0] === '\r') {
        if (chunk[match.index + 1] === '\n') newline.lastIndex++;
        else if (match.index === chunk.length - 1) skipLineFeed = true;
      }
      start = newline.lastIndex;
    }
    if (start < chunk.length) pending.push(chunk.slice(start));
  };
}

export interface StateEventHandlers {
  onState: (data: string) => void;
  onOpen: () => void;
  onError: () => void;
}

/** Same /api/events wire protocol in both hosts; only desktop needs a header-capable SSE reader. */
export function subscribeStateEvents(handlers: StateEventHandlers): () => void {
  if (!desktopContext()) {
    const source = new EventSource('/api/events');
    source.addEventListener('state', (event) => handlers.onState((event as MessageEvent<string>).data));
    source.onopen = () => handlers.onOpen();
    source.onerror = () => handlers.onError();
    return () => source.close();
  }
  const controller = new AbortController();
  let closed = false;
  let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
  let retry: ReturnType<typeof setTimeout> | undefined;
  const connect = async () => {
    let connectionReader: ReadableStreamDefaultReader<Uint8Array> | undefined;
    try {
      const response = await apiFetch('/api/events', {
        headers: { Accept: 'text/event-stream' }, credentials: 'same-origin', signal: controller.signal,
      });
      if (closed) {
        await response.body?.cancel();
        return;
      }
      if (!response.ok || !response.body || response.headers.get('Content-Type')?.split(';')[0].trim().toLowerCase() !== 'text/event-stream') {
        await response.body?.cancel();
        throw new Error('State event stream is unavailable.');
      }
      reader = connectionReader = response.body.getReader();
      const decoder = new TextDecoder();
      const parse = stateEventParser((data) => { if (!closed) handlers.onState(data); });
      handlers.onOpen();
      while (!closed) {
        const { done, value } = await connectionReader.read();
        if (done) {
          parse(decoder.decode());
          break;
        }
        parse(decoder.decode(value, { stream: true }));
      }
      if (!closed) handlers.onError();
    } catch {
      if (!closed) handlers.onError();
    } finally {
      if (connectionReader) {
        await connectionReader.cancel().catch(() => undefined);
        connectionReader.releaseLock();
        if (reader === connectionReader) reader = undefined;
      }
      // Every connection starts with a fresh snapshot, so reconnect without replay/Last-Event-ID.
      if (!closed) retry = setTimeout(() => { void connect(); }, 1000);
    }
  };
  void connect();
  return () => {
    if (closed) return;
    closed = true;
    if (retry !== undefined) clearTimeout(retry);
    controller.abort();
    void reader?.cancel().catch(() => undefined);
  };
}

export interface MediaUrl {
  url: string;
  revoke: () => void;
}

/** The consumer owns the URL; abort also revokes it and prevents late responses creating one. */
export async function loadMediaUrl(src: string, signal: AbortSignal): Promise<MediaUrl> {
  signal.throwIfAborted();
  if (!needsDesktopAuthentication(src)) return { url: src, revoke: () => undefined };
  const response = await apiFetch(src, { credentials: 'same-origin', signal });
  if (!response.ok) {
    const body: unknown = await response.json().catch(() => null);
    const message = body && typeof body === 'object' && 'error' in body && typeof body.error === 'string'
      ? body.error : `파일을 불러오지 못했습니다. HTTP ${response.status}`;
    throw new Error(message);
  }
  const blob = await response.blob();
  signal.throwIfAborted();
  const url = URL.createObjectURL(blob);
  let revoked = false;
  const revoke = () => {
    if (revoked) return;
    revoked = true;
    signal.removeEventListener('abort', revoke);
    URL.revokeObjectURL(url);
  };
  signal.addEventListener('abort', revoke, { once: true });
  return { url, revoke };
}
