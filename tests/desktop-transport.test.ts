import assert from 'node:assert/strict';
import test, { type TestContext } from 'node:test';
import { setImmediate as immediate } from 'node:timers/promises';
import {
  apiFetch, DESKTOP_TOKEN_HEADER, desktopContext, loadMediaUrl,
  needsDesktopAuthentication, stateEventParser, subscribeStateEvents,
} from '../web/desktop-transport';

const origin = 'http://127.0.0.1:43127';
const token = 'ab'.repeat(32);
const boot = () => Object.freeze({ origin, token });

function globalValue(t: TestContext, key: string, value: unknown) {
  const descriptor = Object.getOwnPropertyDescriptor(globalThis, key);
  Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
  t.after(() => {
    if (descriptor) Object.defineProperty(globalThis, key, descriptor);
    else Reflect.deleteProperty(globalThis, key);
  });
}

function desktopWindow(t: TestContext, context: unknown = boot()) {
  const page = { location: { origin, href: `${origin}/?bot=example` }, __GROK_DESKTOP__: context, top: null as unknown };
  page.top = page;
  globalValue(t, 'window', page);
  return page;
}

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

const eventResponse = (stream: ReadableStream<Uint8Array>) => new Response(stream, { headers: { 'Content-Type': 'text/event-stream; charset=utf-8' } });

test('desktop context requires the frozen exact-loopback top-level host context', (t) => {
  const page = desktopWindow(t);
  assert.deepEqual(desktopContext(), { origin, token });
  page.top = {};
  assert.equal(desktopContext(), null, 'iframes cannot use the context');
  page.top = page;
  page.location.origin = 'http://127.0.0.1:43128';
  assert.equal(desktopContext(), null, 'another loopback port is another origin');
  page.location.origin = origin;
  for (const context of [undefined, { origin, token }, Object.freeze({ origin, token: 'short' }),
    Object.freeze({ origin: `${origin}/`, token }), Object.freeze({ origin: `${origin}/api`, token }),
    Object.freeze({ origin: 'https://127.0.0.1:43127', token }), Object.freeze({ origin: 'http://localhost:43127', token })]) {
    page.__GROK_DESKTOP__ = context;
    assert.equal(desktopContext(), null);
  }
});

test('desktop API fetch scopes its header to the exact API origin and rejects redirect forwarding', async (t) => {
  desktopWindow(t);
  const calls: Array<{ input: RequestInfo | URL; init?: RequestInit }> = [];
  t.mock.method(globalThis, 'fetch', async (input: RequestInfo | URL, init?: RequestInit) => {
    calls.push({ input, init });
    return new Response('{}');
  });
  for (const input of ['/api', '/api/state', '/api/events?state=1', `${origin}/api/file`, new URL(`${origin}/api/computer`), new Request(`${origin}/api/state`, { headers: { Accept: 'application/json' } })]) {
    assert.equal(needsDesktopAuthentication(input), true);
    await apiFetch(input, { headers: { 'X-Grok-Bot': '1' }, redirect: 'follow' });
    const headers = new Headers(calls.at(-1)!.init?.headers);
    assert.equal(headers.get(DESKTOP_TOKEN_HEADER), token);
    assert.equal(headers.get('X-Grok-Bot'), '1');
    assert.equal(calls.at(-1)!.init?.redirect, 'error');
    assert.equal(calls.at(-1)!.init?.mode, 'cors', 'WebKit must retain a non-null Origin for mutations under no-referrer');
  }
  await apiFetch(new Request(`${origin}/api/state`, { headers: { Accept: 'application/json' } }));
  assert.equal(new Headers(calls.at(-1)!.init?.headers).get('Accept'), 'application/json', 'Request headers survive optional authentication');
  await apiFetch(new Request('https://example.com/api/state', { headers: { [DESKTOP_TOKEN_HEADER]: token, Accept: 'text/plain' } }));
  assert.equal(new Headers(calls.at(-1)!.init?.headers).has(DESKTOP_TOKEN_HEADER), false, 'inherited Request credentials cannot leak externally');
  assert.equal(new Headers(calls.at(-1)!.init?.headers).get('Accept'), 'text/plain');
  for (const input of ['/apiary', '/apiculture', '/api%2fstate', '/api/../assets/app.js', '/assets/app.js', '/hooks/routines/id', '/internal/mcp/id',
    'http://127.0.0.1:43128/api/state', 'http://localhost:43127/api/state', 'https://example.com/api/state', '//example.com/api/state',
    'data:text/plain,hello', `blob:${origin}/image`, `http://user:password@127.0.0.1:43127/api/state`]) {
    assert.equal(needsDesktopAuthentication(input), false, input);
    await apiFetch(input, { headers: { [DESKTOP_TOKEN_HEADER]: token, Accept: 'image/png' } });
    const headers = new Headers(calls.at(-1)!.init?.headers);
    assert.equal(headers.has(DESKTOP_TOKEN_HEADER), false, input);
    assert.equal(headers.get('Accept'), 'image/png');
  }
});

test('ordinary web fetch and state subscription retain native behavior without a token', async (t) => {
  desktopWindow(t, null);
  const init = { method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Grok-Bot': '1' }, body: '{}' };
  const fetch = t.mock.method(globalThis, 'fetch', async (_input: RequestInfo | URL, _init?: RequestInit) => new Response('{}'));
  await apiFetch('/api/settings', init);
  assert.equal(fetch.mock.calls[0].arguments[1], init);
  let opened = 0;
  let errors = 0;
  let received = '';
  let instance!: NativeEvents;
  class NativeEvents {
    path: string;
    closed = false;
    onopen?: () => void;
    onerror?: () => void;
    state?: (event: MessageEvent<string>) => void;
    constructor(path: string) { this.path = path; instance = this; }
    addEventListener(kind: string, listener: (event: MessageEvent<string>) => void) { assert.equal(kind, 'state'); this.state = listener; }
    close() { this.closed = true; }
  }
  globalValue(t, 'EventSource', NativeEvents);
  const close = subscribeStateEvents({ onOpen: () => opened++, onError: () => errors++, onState: (data) => { received = data; } });
  assert.equal(instance.path, '/api/events');
  instance.onopen?.();
  instance.state?.({ data: '{"revision":1}' } as MessageEvent<string>);
  instance.onerror?.();
  close();
  assert.deepEqual({ opened, errors, received, closed: instance.closed }, { opened: 1, errors: 1, received: '{"revision":1}', closed: true });
  assert.equal(fetch.mock.callCount(), 1, 'native EventSource is not replaced with fetch on web');
});

test('SSE parser preserves state events across every CR/LF and multiline-data split', () => {
  const stream = ': heartbeat\r\nevent: ignored\rdata: ignored\r\r'
    + 'event: state\r\ndata: {"text":\r\ndata: "안녕"}\r\n\r\n'
    + 'data: default event is ignored\n\nevent: state\ndata:\n\nevent: state\ndata: unfinished';
  for (let split = 0; split <= stream.length; split++) {
    const events: string[] = [];
    const parse = stateEventParser((data) => events.push(data));
    parse(stream.slice(0, split));
    parse(stream.slice(split));
    assert.deepEqual(events, ['{"text":\n"안녕"}', ''], `split ${split}`);
  }
  const events: string[] = [];
  const parse = stateEventParser((data) => events.push(data));
  for (const character of stream) parse(character);
  assert.deepEqual(events, ['{"text":\n"안녕"}', '']);
});

test('desktop SSE decodes split UTF-8, ignores heartbeats, and aborts/cancels on close', async (t) => {
  desktopWindow(t);
  const opened = deferred<void>();
  const state = deferred<string>();
  let cancelled = false;
  let errors = 0;
  let stream!: ReadableStreamDefaultController<Uint8Array>;
  let signal!: AbortSignal;
  t.mock.method(globalThis, 'fetch', async (_input: RequestInfo | URL, init?: RequestInit) => {
    assert.equal(new Headers(init?.headers).get(DESKTOP_TOKEN_HEADER), token);
    signal = init!.signal!;
    return eventResponse(new ReadableStream({ start(controller) { stream = controller; }, cancel() { cancelled = true; } }));
  });
  const close = subscribeStateEvents({ onOpen: () => opened.resolve(), onState: (data) => state.resolve(data), onError: () => errors++ });
  t.after(close);
  await opened.promise;
  for (const byte of new TextEncoder().encode('\uFEFF: heartbeat\r\nevent: state\r\ndata: {"text":"안녕 👋"}\r\n\r\n')) stream.enqueue(Uint8Array.of(byte));
  assert.equal(await state.promise, '{"text":"안녕 👋"}');
  close();
  close();
  await immediate();
  assert.equal(signal.aborted, true);
  assert.equal(cancelled, true);
  assert.equal(errors, 0);
});

test('closing desktop SSE before headers arrive cancels a late response without events', async (t) => {
  desktopWindow(t);
  const response = deferred<Response>();
  let signal!: AbortSignal;
  let cancelled = false;
  const callbacks: string[] = [];
  t.mock.method(globalThis, 'fetch', (_input: RequestInfo | URL, init?: RequestInit) => { signal = init!.signal!; return response.promise; });
  const close = subscribeStateEvents({ onOpen: () => callbacks.push('open'), onState: () => callbacks.push('state'), onError: () => callbacks.push('error') });
  close();
  response.resolve(eventResponse(new ReadableStream({ cancel() { cancelled = true; } })));
  await immediate();
  assert.equal(signal.aborted, true);
  assert.equal(cancelled, true);
  assert.deepEqual(callbacks, []);
});

test('desktop SSE reconnects with the same wire endpoint after EOF and close stops retry timers', async (t) => {
  desktopWindow(t);
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const ended = deferred<void>();
  const reconnected = deferred<void>();
  let connections = 0;
  const paths: string[] = [];
  t.mock.method(globalThis, 'fetch', async (input: RequestInfo | URL) => {
    paths.push(String(input));
    connections++;
    return eventResponse(new ReadableStream({ start(controller) { if (connections === 1) controller.close(); } }));
  });
  const close = subscribeStateEvents({ onState: () => undefined, onOpen: () => { if (connections === 2) reconnected.resolve(); }, onError: () => ended.resolve() });
  t.after(close);
  await ended.promise;
  await immediate();
  assert.equal(connections, 1);
  t.mock.timers.tick(1000);
  await reconnected.promise;
  assert.deepEqual(paths, ['/api/events', '/api/events']);
  close();
  await immediate();
  t.mock.timers.tick(10_000);
  assert.equal(connections, 2);
});

test('invalid desktop SSE HTTP responses report errors and do not retry after close', async (t) => {
  desktopWindow(t);
  t.mock.timers.enable({ apis: ['setTimeout'] });
  for (const response of [new Response('{"error":"unauthorized"}', { status: 403 }), new Response('<html>wrong route</html>', { headers: { 'Content-Type': 'text/html' } })]) {
    const failed = deferred<void>();
    let opens = 0;
    const fetch = t.mock.method(globalThis, 'fetch', async () => response);
    const close = subscribeStateEvents({ onState: () => assert.fail('not SSE'), onOpen: () => opens++, onError: () => failed.resolve() });
    await failed.promise;
    await immediate(); // Let the reconnect timer be armed before closing it.
    close();
    t.mock.timers.tick(1000);
    assert.equal(opens, 0);
    assert.equal(fetch.mock.callCount(), 1);
    fetch.mock.restore();
  }
});

test('authenticated media retains MIME, uses a blob URL, and revokes exactly once on abort/release', async (t) => {
  desktopWindow(t);
  let created!: Blob;
  const revoked: string[] = [];
  t.mock.method(URL, 'createObjectURL', (blob: Blob) => { created = blob; return 'blob:private-media'; });
  t.mock.method(URL, 'revokeObjectURL', (url: string) => revoked.push(url));
  const controller = new AbortController();
  t.mock.method(globalThis, 'fetch', async (input: RequestInfo | URL, init?: RequestInit) => {
    assert.equal(input, '/api/conversations/id/attachments/file');
    assert.equal(new Headers(init?.headers).get(DESKTOP_TOKEN_HEADER), token);
    assert.equal(init?.signal, controller.signal);
    return new Response('private bytes', { headers: { 'Content-Type': 'text/plain; charset=utf-8' } });
  });
  const media = await loadMediaUrl('/api/conversations/id/attachments/file', controller.signal);
  assert.equal(media.url, 'blob:private-media');
  assert.equal(created.type, 'text/plain;charset=utf-8');
  assert.equal(await created.text(), 'private bytes');
  controller.abort();
  media.revoke();
  assert.deepEqual(revoked, ['blob:private-media']);
});

test('media aborted while bytes arrive never creates an object URL for a stale frame', async (t) => {
  desktopWindow(t);
  const bytes = deferred<Blob>();
  const started = deferred<void>();
  const controller = new AbortController();
  const create = t.mock.method(URL, 'createObjectURL', () => 'blob:must-not-exist');
  t.mock.method(globalThis, 'fetch', async () => ({ ok: true, blob: () => { started.resolve(); return bytes.promise; } }) as Response);
  const loading = loadMediaUrl('/api/computer/preview?epoch=old&frame=old', controller.signal);
  const rejected = assert.rejects(loading, { name: 'AbortError' });
  await started.promise;
  controller.abort();
  bytes.resolve(new Blob(['old frame'], { type: 'image/png' }));
  await rejected;
  assert.equal(create.mock.callCount(), 0);
});

test('media leaves data/blob/external URLs and ordinary web API element URLs untouched', async (t) => {
  const page = desktopWindow(t);
  const fetch = t.mock.method(globalThis, 'fetch', async () => { throw Error('must not fetch'); });
  const revoke = t.mock.method(URL, 'revokeObjectURL', () => undefined);
  for (const src of ['data:image/png;base64,AAAA', `blob:${origin}/diagram`, 'https://example.com/image.png', 'http://127.0.0.1:43128/api/image']) {
    const media = await loadMediaUrl(src, new AbortController().signal);
    assert.equal(media.url, src);
    media.revoke();
  }
  page.__GROK_DESKTOP__ = undefined;
  const media = await loadMediaUrl('/api/generated-images/id', new AbortController().signal);
  assert.equal(media.url, '/api/generated-images/id');
  media.revoke();
  assert.equal(fetch.mock.callCount(), 0);
  assert.equal(revoke.mock.callCount(), 0);
});

test('media surfaces API errors without creating object URLs', async (t) => {
  desktopWindow(t);
  t.mock.method(globalThis, 'fetch', async () => new Response('{"error":"Invalid desktop token."}', { status: 403, headers: { 'Content-Type': 'application/json' } }));
  const create = t.mock.method(URL, 'createObjectURL', () => 'blob:must-not-exist');
  await assert.rejects(loadMediaUrl('/api/generated-images/id', new AbortController().signal), /Invalid desktop token/);
  assert.equal(create.mock.callCount(), 0);
});
