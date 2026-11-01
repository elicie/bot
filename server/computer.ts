import { randomUUID } from 'node:crypto';
import { chmod, mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { chromium } from 'playwright-core';
import type { Browser, BrowserContext, Page } from 'playwright-core';
import { z } from 'zod';
import {
  COMPUTER_KEYS, COMPUTER_VIEWPORT,
} from '../shared/computer.js';
import type {
  ComputerAction, ComputerActionRequest, ComputerControl, ComputerFrame,
  ComputerFrameRequest, ComputerObservation, ComputerStartInput, ComputerStatus,
} from '../shared/computer.js';
import { ComputerNetworkPolicy, ComputerProxy, computerTestMode } from './computer-network.js';
import { HttpError } from './files.js';

export interface ManagedComputerOptions {
  allowTestOrigins?: readonly string[];
  testBrowser?: {
    executablePath?: string;
    channel?: 'chrome' | 'chromium';
    chromiumSandbox?: boolean;
  };
}

export interface ComputerScreenshot extends ComputerObservation {
  screenshotBuffer: Buffer;
}

const uuid = z.string().uuid();
const frameRequest = z.object({ requestId: uuid, epoch: uuid, frameId: uuid.nullable() }).strict();
const actionSchema = z.discriminatedUnion('type', [
  z.object({ type: z.literal('navigate'), url: z.string().max(4096) }).strict(),
  z.object({ type: z.literal('click'), x: z.number().finite().min(0).lt(COMPUTER_VIEWPORT.width), y: z.number().finite().min(0).lt(COMPUTER_VIEWPORT.height) }).strict(),
  z.object({ type: z.literal('type'), text: z.string().min(1).max(2000).refine(value => !/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/.test(value)) }).strict(),
  z.object({ type: z.literal('key'), key: z.enum(COMPUTER_KEYS) }).strict(),
  z.object({ type: z.literal('scroll'), deltaY: z.number().int().min(-1200).max(1200).refine(value => value !== 0) }).strict(),
  z.object({ type: z.literal('observe') }).strict(),
]);
const actionRequest = frameRequest.extend({ action: actionSchema }).strict();
const startRequest = z.object({ requestId: uuid, userInitiated: z.literal(true), origins: z.array(z.string()).min(1).max(16) }).strict();
const protectionArguments = [
  '--enable-automation',
  '--proxy-bypass-list=<-loopback>',
  '--disable-quic',
  '--disable-http2',
  '--force-webrtc-ip-handling-policy=disable_non_proxied_udp',
  '--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE 127.0.0.1',
  '--disable-background-networking',
  '--disable-component-update',
  '--disable-sync',
  '--no-first-run',
  '--no-default-browser-check',
];

interface Session {
  epoch: string;
  policy: ComputerNetworkPolicy;
  proxy: ComputerProxy;
  abort: AbortController;
  sandboxed: boolean;
  directory?: Promise<string>;
  opening?: Promise<Browser>;
  browser?: Browser;
  context?: BrowserContext;
  page?: Page;
  cleanup?: Promise<void>;
  tail: Promise<void>;
  pending: number;
  navigation: number;
  frame: ComputerFrame | null;
}

function parse<T>(schema: z.ZodType<T>, input: unknown): T {
  const result = schema.safeParse(input);
  if (!result.success) throw new HttpError(400, 'Invalid computer request. Use a UUID request ID and the current epoch and frame.');
  return result.data;
}

function stoppedError(): HttpError { return new HttpError(409, 'The computer session was stopped or replaced.'); }

function safeLabel(input: string): string { return input.replace(/[\u0000-\u001f\u007f]/g, ' ').slice(0, 200); }

export class ManagedComputer {
  private state: ComputerStatus['state'] = 'stopped';
  private epoch: string = randomUUID();
  private session: Session | null = null;
  private readonly usedRequests = new Set<string>();
  private readonly options: ManagedComputerOptions;
  private error?: string;
  private stopping?: Promise<ComputerStatus>;

  constructor(options: ManagedComputerOptions = {}) {
    if ((options.allowTestOrigins?.length || options.testBrowser) && !computerTestMode()) throw new HttpError(403, 'Browser test overrides are available only to tests.');
    this.options = {
      allowTestOrigins: options.allowTestOrigins ? [...options.allowTestOrigins] : undefined,
      testBrowser: options.testBrowser ? { ...options.testBrowser } : undefined,
    };
  }

  status(): ComputerStatus {
    const session = this.session;
    return {
      state: this.state, epoch: this.epoch,
      frameId: this.state === 'running' ? session?.frame?.id ?? null : null,
      viewport: { ...COMPUTER_VIEWPORT },
      origins: this.state === 'running' || this.state === 'starting' ? [...session!.policy.origins] : [],
      url: this.state === 'running' ? this.visibleUrl(session!) : null,
      busy: this.state === 'starting' || this.state === 'stopping' || !!session?.pending,
      sandboxed: this.state === 'running' && !!session?.sandboxed,
      ...(this.error ? { error: this.error } : {}),
    };
  }

  async start(input: ComputerStartInput): Promise<ComputerStatus> {
    const request = parse(startRequest, input);
    this.consume(request.requestId);
    if (this.state !== 'stopped') throw new HttpError(409, 'Stop the current computer before starting another.');
    const policy = new ComputerNetworkPolicy(request.origins, { allowTestOrigins: this.options.allowTestOrigins });
    const session: Session = {
      epoch: randomUUID(), policy, proxy: new ComputerProxy(policy), abort: new AbortController(),
      sandboxed: this.options.testBrowser?.chromiumSandbox !== false,
      tail: Promise.resolve(), pending: 0, navigation: 0, frame: null,
    };
    this.session = session;
    this.epoch = session.epoch;
    this.state = 'starting';
    this.error = undefined;
    try {
      for (const origin of policy.origins) await this.interruptible(session, policy.resolve(origin));
      await this.interruptible(session, session.proxy.start());
      const testBrowser = this.options.testBrowser;
      session.directory = mkdtemp(path.join(tmpdir(), 'grok-managed-computer-')).then(async directory => { await chmod(directory, 0o700); return directory; });
      const directory = await this.interruptible(session, session.directory);
      this.assertLive(session);
      session.opening = chromium.launch({
        ...(testBrowser?.executablePath ? { executablePath: testBrowser.executablePath } : { channel: testBrowser?.channel ?? 'chrome' }),
        headless: true, chromiumSandbox: session.sandboxed, timeout: 15_000,
        proxy: session.proxy.configuration, args: protectionArguments,
        env: { PATH: process.env.PATH ?? '/usr/bin:/bin', HOME: directory, XDG_CONFIG_HOME: directory, XDG_CACHE_HOME: directory, TMPDIR: directory, LANG: 'C.UTF-8' },
      });
      session.browser = await this.interruptible(session, session.opening);
      session.browser.on('disconnected', () => {
        if (this.session === session && !session.abort.signal.aborted) void this.terminate('The managed Chrome process disconnected.');
      });
      session.context = await this.interruptible(session, session.browser.newContext({
        viewport: { ...COMPUTER_VIEWPORT }, screen: { ...COMPUTER_VIEWPORT }, deviceScaleFactor: 1,
        acceptDownloads: false, serviceWorkers: 'block', permissions: [],
        ignoreHTTPSErrors: false, locale: 'en-US', timezoneId: 'UTC', reducedMotion: 'reduce',
      }));
      const context = session.context;
      session.page = await this.interruptible(session, context.newPage());
      await this.verifyLaunch(session);
      await this.installGuards(session, context);
      const page = session.page;
      await this.interruptible(session, page.goto('about:blank'));
      context.on('page', extra => { if (extra !== page) void extra.close().catch(() => {}); });
      page.on('dialog', dialog => { void dialog.dismiss().catch(() => {}); });
      page.on('download', download => { void download.cancel().catch(() => {}); });
      page.on('close', () => {
        if (this.session === session && !session.abort.signal.aborted) void this.terminate('The managed computer page closed.');
      });
      page.on('framenavigated', frame => {
        if (frame === page.mainFrame()) { session.navigation++; session.frame = null; }
      });
      this.assertLive(session);
      this.state = 'running';
      await this.interruptible(session, this.capture(session));
      return this.status();
    } catch (error) {
      if (session.abort.signal.aborted) throw stoppedError();
      const message = error instanceof HttpError ? error.message : 'Managed Chrome could not start with the required sandbox and network protections. Install a supported Chrome and enable the host sandbox. No insecure fallback was attempted.';
      await this.terminate(message);
      throw new HttpError(error instanceof HttpError ? error.status : 503, message);
    }
  }

  stop(): Promise<ComputerStatus> { return this.terminate(); }

  async perform(input: ComputerActionRequest): Promise<ComputerObservation> {
    const request = parse(actionRequest, input);
    return this.enqueue(request, request.action.type === 'observe', async session => {
      await this.apply(session, request.action);
      const result = await this.capture(session);
      return { status: this.status(), frame: result.frame };
    });
  }

  async screenshot(input: ComputerFrameRequest): Promise<ComputerScreenshot> {
    const request = parse(frameRequest, input);
    return this.enqueue(request, true, async session => {
      const result = await this.capture(session);
      return { status: this.status(), ...result };
    });
  }

  private consume(requestId: string): void {
    const id = requestId.toLowerCase();
    if (this.usedRequests.has(id)) throw new HttpError(409, 'This computer request ID was already used. Requests are never replayed.');
    if (this.usedRequests.size >= 10_000) throw new HttpError(429, 'This computer reached its request limit. Create a new service instance.');
    this.usedRequests.add(id);
  }

  private assertLive(session: Session): void {
    if (this.session !== session || session.abort.signal.aborted) throw stoppedError();
  }

  private visibleUrl(session: Session): string {
    const url = session.page?.url() ?? 'about:blank';
    return session.policy.allows(url) ? url : 'about:blank';
  }

  private interruptible<T>(session: Session, work: Promise<T>): Promise<T> {
    if (session.abort.signal.aborted) { void work.catch(() => {}); return Promise.reject(stoppedError()); }
    return new Promise<T>((resolve, reject) => {
      const abort = () => { clearTimeout(timer); reject(stoppedError()); };
      const timer = setTimeout(() => { session.abort.signal.removeEventListener('abort', abort); reject(new HttpError(504, 'The computer operation timed out.')); }, 20_000);
      session.abort.signal.addEventListener('abort', abort, { once: true });
      work.then(resolve, reject).finally(() => {
        clearTimeout(timer);
        session.abort.signal.removeEventListener('abort', abort);
      });
    });
  }

  private enqueue<T>(request: ComputerFrameRequest, observe: boolean, work: (session: Session) => Promise<T>): Promise<T> {
    this.consume(request.requestId);
    const session = this.session;
    if (!session || this.state !== 'running' || request.epoch !== session.epoch) throw stoppedError();
    const currentFrame = session.frame?.id ?? null;
    if (request.frameId !== currentFrame) throw new HttpError(409, 'The computer frame is stale. Use the latest status and screenshot.');
    if (request.frameId === null && !observe) throw new HttpError(409, 'Observe the computer before performing another action.');
    if (session.pending >= 8) throw new HttpError(429, 'Too many computer operations are queued.');
    session.pending++;
    const queued = session.tail.then(async () => {
      this.assertLive(session);
      if (request.frameId !== (session.frame?.id ?? null)) throw new HttpError(409, 'The computer frame is stale. Observe the current frame before acting.');
      session.frame = null;
      try { return await this.interruptible(session, work(session)); }
      catch (error) {
        session.frame = null;
        if (session.abort.signal.aborted) throw stoppedError();
        if (error instanceof HttpError) throw error;
        throw new HttpError(502, 'The computer operation failed. Stop and restart the computer if no current frame is available.');
      }
    });
    session.tail = queued.then(() => {}, () => {});
    const result = this.interruptible(session, queued);
    return result.finally(() => { session.pending--; });
  }

  private terminate(error?: string): Promise<ComputerStatus> {
    if (this.stopping) return this.stopping;
    const session = this.session;
    if (!session) return Promise.resolve(this.status());
    this.state = 'stopping';
    this.epoch = randomUUID();
    this.error = error;
    session.frame = null;
    session.abort.abort();
    this.stopping = this.cleanup(session).then(() => {
      if (this.session === session) { this.session = null; this.state = 'stopped'; }
      this.stopping = undefined;
      return this.status();
    });
    return this.stopping;
  }

  private cleanup(session: Session): Promise<void> {
    session.cleanup ??= (async () => {
      await session.proxy.stop();
      const browser = session.browser ?? await session.opening?.catch(() => undefined);
      if (browser) await browser.close().catch(() => {});
      const directory = await session.directory?.catch(() => undefined);
      if (directory) await rm(directory, { recursive: true, force: true });
    })();
    return session.cleanup;
  }

  private async verifyLaunch(session: Session): Promise<void> {
    const cdp = await this.interruptible(session, session.browser!.newBrowserCDPSession());
    try {
      const result = await this.interruptible(session, cdp.send('Browser.getBrowserCommandLine'));
      const args = result.arguments;
      if (!protectionArguments.every(flag => args.includes(flag))
        || !args.includes(`--proxy-server=${session.proxy.configuration.server}`)
        || args.filter(arg => arg.startsWith('--proxy-bypass-list=')).some(arg => arg !== '--proxy-bypass-list=<-loopback>')
        || session.sandboxed && args.some(arg => ['--no-sandbox', '--disable-setuid-sandbox', '--disable-seccomp-filter-sandbox'].includes(arg))) {
        throw new HttpError(503, 'Chrome did not enable the required sandbox and network protections.');
      }
      await this.interruptible(session, cdp.send('Browser.setDownloadBehavior', { behavior: 'deny', eventsEnabled: false }));
      const page = session.page!;
      if (session.sandboxed && process.platform === 'linux') {
        await this.interruptible(session, page.goto('chrome://sandbox', { timeout: 8000, waitUntil: 'domcontentloaded' }));
        const report = await this.interruptible(session, page.locator('body').innerText());
        if (!/You are adequately sandboxed\./.test(report) || !/Seccomp-BPF sandbox\s+Yes/.test(report)) throw new HttpError(503, 'Chrome could not verify an active Linux sandbox.');
      }
      await this.interruptible(session, page.goto(session.proxy.probeUrl, { timeout: 8000, waitUntil: 'domcontentloaded' }));
      const body = await this.interruptible(session, page.locator('body').innerText());
      if (body !== session.proxy.probeBody) throw new HttpError(503, 'Chrome did not use the mandatory loopback proxy.');
      try { await this.interruptible(session, page.goto(`http://unsupported-dns-${randomUUID()}.invalid/`, { timeout: 4000, waitUntil: 'domcontentloaded' })); }
      catch { throw new HttpError(503, 'Chrome did not apply the mandatory proxy DNS policy.'); }
      const deniedBody = await this.interruptible(session, page.locator('body').innerText());
      if (deniedBody !== 'Network access was denied by the managed computer policy.') throw new HttpError(503, 'Chrome did not apply the mandatory proxy DNS policy.');
    } finally { await cdp.detach().catch(() => {}); }
  }

  private async installGuards(session: Session, context: BrowserContext): Promise<void> {
    context.setDefaultTimeout(5000);
    context.setDefaultNavigationTimeout(12_000);
    await this.interruptible(session, context.route('**/*', async route => {
      try {
        const request = route.request();
        if (session.abort.signal.aborted || !session.policy.allows(request.url())
          || !['GET', 'HEAD', 'OPTIONS'].includes(request.method()) || request.postDataBuffer()?.length
          || ['websocket', 'other'].includes(request.resourceType())
          || request.isNavigationRequest() && (request.frame().page() !== session.page || request.frame().parentFrame() !== null)) {
          await route.abort('blockedbyclient');
          return;
        }
        await session.policy.resolve(request.url());
        if (session.abort.signal.aborted) { await route.abort('blockedbyclient'); return; }
        const headers = { ...request.headers() };
        for (const name of ['authorization', 'cookie', 'proxy-authorization']) delete headers[name];
        await route.continue({ headers });
      } catch { await route.abort('blockedbyclient').catch(() => {}); }
    }));
    await this.interruptible(session, context.routeWebSocket('**/*', socket => socket.close({ code: 1008, reason: 'WebSockets are disabled.' })));
    await this.interruptible(session, context.addInitScript({ content: `(() => {
      const origins = ${JSON.stringify(session.policy.origins)};
      const denied = () => { throw new Error('This API is disabled in the managed computer.'); };
      for (const name of ['WebSocket', 'WebTransport', 'RTCPeerConnection', 'webkitRTCPeerConnection', 'RTCIceGatherer', 'RTCIceTransport']) {
        Object.defineProperty(window, name, { value: denied, writable: false, configurable: false });
      }
      Object.defineProperty(window, 'open', { value: () => null, writable: false, configurable: false });
      Object.defineProperty(navigator, 'sendBeacon', { value: () => false, writable: false, configurable: false });
      Object.defineProperty(navigator, 'serviceWorker', { value: undefined, writable: false, configurable: false });
      const sensitive = element => {
        const field = element.closest('input,textarea,[contenteditable],iframe,form');
        if (!field) return false;
        return field.matches('iframe,input[type="password"],input[type="file"],input[type="email"],input[type="tel"]')
          || /pass|secret|token|credential|login|username|one.?time|credit|card|cvv|cvc|ssn|api.?key|^pin$/i.test(['name', 'id', 'autocomplete'].map(key => field.getAttribute(key) ?? '').join(' '))
          || !!field.closest('form')?.querySelector('input[type="password"],input[type="file"]');
      };
      for (const name of ['click', 'auxclick', 'beforeinput', 'paste', 'drop', 'dragover']) {
        window.addEventListener(name, event => {
          const element = event.composedPath().find(item => item instanceof Element);
          if (!element) return;
          const anchor = element.closest('a');
          let blocked = sensitive(element) || name === 'drop' || name === 'dragover' || name === 'paste';
          if (anchor) {
            try { const url = new URL(anchor.href); blocked ||= !origins.includes(url.origin) || !['http:', 'https:'].includes(url.protocol) || !!url.username || !!url.password || anchor.hasAttribute('download') || !!anchor.target && anchor.target !== '_self'; }
            catch { blocked = true; }
          }
          if (blocked) { event.preventDefault(); event.stopImmediatePropagation(); }
        }, true);
      }
      window.addEventListener('submit', event => { event.preventDefault(); event.stopImmediatePropagation(); }, true);
    })();` }));
  }

  private async target(session: Session, point?: { x: number; y: number }): Promise<{ editable: boolean; blocked: boolean }> {
    return this.interruptible(session, session.page!.evaluate(({ point, origins }) => {
      const element = point ? document.elementFromPoint(point.x, point.y) : document.activeElement;
      if (!element) return { editable: false, blocked: true };
      const field = element.closest('input,textarea,[contenteditable],iframe');
      const blocked = !!field && (field.matches('iframe,input[type="password"],input[type="file"],input[type="email"],input[type="tel"]')
        || /pass|secret|token|credential|login|username|one.?time|credit|card|cvv|cvc|ssn|api.?key|^pin$/i.test(['name', 'id', 'autocomplete'].map(key => field.getAttribute(key) ?? '').join(' '))
        || !!field.closest('form')?.querySelector('input[type="password"],input[type="file"]'));
      const anchor = element.closest('a');
      let blockedLink = false;
      if (anchor) {
        try { const url = new URL(anchor.href); blockedLink = !origins.includes(url.origin) || !['http:', 'https:'].includes(url.protocol) || !!url.username || !!url.password || anchor.hasAttribute('download') || !!anchor.target && anchor.target !== '_self'; }
        catch { blockedLink = true; }
      }
      const editable = !blocked && (field instanceof HTMLTextAreaElement && !field.readOnly && !field.disabled
        || field instanceof HTMLInputElement && ['text', 'search', 'url', 'number'].includes(field.type) && !field.readOnly && !field.disabled
        || element instanceof HTMLElement && element.isContentEditable);
      return { editable: !!editable, blocked: blocked || blockedLink };
    }, { point, origins: [...session.policy.origins] }));
  }

  private async apply(session: Session, action: ComputerAction): Promise<void> {
    const page = session.page!;
    switch (action.type) {
      case 'navigate': {
        const target = await this.interruptible(session, session.policy.resolve(action.url));
        const response = await this.interruptible(session, page.goto(target.url.href, { waitUntil: 'domcontentloaded', timeout: 12_000 }));
        if (!session.policy.allows(page.url())) throw new HttpError(403, 'Navigation left the selected public origins.');
        if (response && response.status() >= 400) throw new HttpError(502, 'The destination returned an error or was blocked by the network policy.');
        break;
      }
      case 'click': {
        const target = await this.target(session, action);
        if (target.blocked) throw new HttpError(403, 'Credentials, uploads, downloads, popups, and unapproved destinations are blocked.');
        await this.interruptible(session, page.mouse.click(action.x, action.y));
        break;
      }
      case 'type': {
        const target = await this.target(session);
        if (!target.editable || target.blocked) throw new HttpError(403, 'Typing requires a focused non-sensitive text control.');
        await this.interruptible(session, page.keyboard.insertText(action.text));
        break;
      }
      case 'key': {
        const target = await this.target(session);
        if (target.blocked && !['Tab', 'Shift+Tab', 'Escape'].includes(action.key)) throw new HttpError(403, 'Keyboard input to sensitive controls is blocked.');
        await this.interruptible(session, page.keyboard.press(action.key));
        break;
      }
      case 'scroll':
        await this.interruptible(session, page.mouse.move(COMPUTER_VIEWPORT.width / 2, COMPUTER_VIEWPORT.height / 2));
        await this.interruptible(session, page.mouse.wheel(0, action.deltaY));
        break;
      case 'observe': break;
    }
  }

  private async capture(session: Session): Promise<{ frame: ComputerFrame; screenshotBuffer: Buffer }> {
    this.assertLive(session);
    const page = session.page!;
    for (let attempt = 0; attempt < 3; attempt++) {
      const navigation = session.navigation;
      const view = await this.interruptible(session, page.evaluate(({ origins, viewport }) => {
        const controls: ComputerControl[] = [];
        for (const element of document.querySelectorAll('a,button,input,textarea,select,[role="button"],[role="link"],[contenteditable="true"],iframe')) {
          const bounds = element.getBoundingClientRect();
          const style = getComputedStyle(element);
          if (bounds.width <= 0 || bounds.height <= 0 || bounds.bottom <= 0 || bounds.right <= 0 || bounds.x >= viewport.width || bounds.y >= viewport.height || style.visibility !== 'visible' || style.display === 'none' || style.opacity === '0') continue;
          const field = element as HTMLInputElement;
          const sensitive = element.matches('iframe,input[type="password"],input[type="file"],input[type="email"],input[type="tel"]')
            || /pass|secret|token|credential|login|username|one.?time|credit|card|cvv|cvc|ssn|api.?key|^pin$/i.test(['name', 'id', 'autocomplete'].map(key => element.getAttribute(key) ?? '').join(' '))
            || !!element.closest('form')?.querySelector('input[type="password"],input[type="file"]');
          let blocked = sensitive;
          if (element instanceof HTMLAnchorElement) {
            try { const url = new URL(element.href); blocked ||= !origins.includes(url.origin) || !['http:', 'https:'].includes(url.protocol) || !!url.username || !!url.password || element.hasAttribute('download') || !!element.target && element.target !== '_self'; }
            catch { blocked = true; }
          }
          const editable = !sensitive && (element instanceof HTMLTextAreaElement || element instanceof HTMLInputElement && ['text', 'search', 'url', 'number'].includes(element.type) || element instanceof HTMLElement && element.isContentEditable) && !field.readOnly && !field.disabled;
          const role: ComputerControl['role'] = element.matches('a,[role="link"]') ? 'link'
            : element.matches('button,[role="button"],input[type="button"],input[type="submit"]') ? 'button'
            : element.matches('input[type="checkbox"]') ? 'checkbox'
            : element.matches('input[type="radio"]') ? 'radio'
            : element.matches('select') ? 'select' : editable ? 'textbox' : 'control';
          const labels = element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement || element instanceof HTMLSelectElement ? [...element.labels ?? []].map(label => label.textContent ?? '').join(' ') : '';
          const label = element.getAttribute('aria-label') || labels || element.getAttribute('placeholder') || (element.matches('input,textarea,select') ? '' : element.textContent ?? '');
          controls.push({
            id: String(controls.length + 1), role, label: label.replace(/[\u0000-\u001f\u007f]/g, ' ').slice(0, 160),
            bounds: { x: Math.max(0, bounds.x), y: Math.max(0, bounds.y), width: Math.min(bounds.right, viewport.width) - Math.max(0, bounds.x), height: Math.min(bounds.bottom, viewport.height) - Math.max(0, bounds.y) },
            disabled: !!field.disabled, editable, blocked,
          });
          if (controls.length === 100) break;
        }
        return { title: document.title, controls };
      }, { origins: [...session.policy.origins], viewport: COMPUTER_VIEWPORT }));
      const screenshotBuffer = await this.interruptible(session, page.screenshot({ type: 'png', fullPage: false, animations: 'disabled', timeout: 6000 }));
      this.assertLive(session);
      if (navigation !== session.navigation) continue;
      const frame: ComputerFrame = {
        id: randomUUID(), epoch: session.epoch, url: this.visibleUrl(session), title: safeLabel(view.title),
        viewport: { ...COMPUTER_VIEWPORT }, controls: view.controls, mimeType: 'image/png',
      };
      session.frame = frame;
      return { frame, screenshotBuffer };
    }
    throw new HttpError(409, 'The page changed during observation. Stop and restart the computer before acting.');
  }
}
