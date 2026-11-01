import { randomBytes } from 'node:crypto';
import { lookup } from 'node:dns/promises';
import { createServer, request as httpRequest } from 'node:http';
import type { IncomingHttpHeaders, IncomingMessage, ServerResponse } from 'node:http';
import { connect, isIP } from 'node:net';
import type { AddressInfo, Socket } from 'node:net';
import { HttpError } from './files.js';

export type ComputerLookup = (hostname: string) => Promise<readonly { address: string; family: number }[]>;

const ipv4Denied: readonly [number, number][] = [
  [0x00000000, 8], [0x0a000000, 8], [0x64400000, 10], [0x7f000000, 8],
  [0xa9fe0000, 16], [0xac100000, 12], [0xc0000000, 24], [0xc0000200, 24],
  [0xc0586300, 24], [0xc0a80000, 16], [0xc6120000, 15], [0xc6336400, 24],
  [0xcb007100, 24], [0xe0000000, 4], [0xf0000000, 4],
];

function ipv6Number(address: string): bigint {
  let input = address;
  if (input.includes('.')) {
    const colon = input.lastIndexOf(':');
    const bytes = input.slice(colon + 1).split('.').map(Number);
    input = `${input.slice(0, colon)}:${((bytes[0] << 8) | bytes[1]).toString(16)}:${((bytes[2] << 8) | bytes[3]).toString(16)}`;
  }
  const [left, right] = input.split('::');
  const before = left ? left.split(':') : [];
  const after = right ? right.split(':') : [];
  const parts = input.includes('::') ? [...before, ...Array(8 - before.length - after.length).fill('0'), ...after] : before;
  return parts.reduce((value, part) => (value << 16n) | BigInt(`0x${part}`), 0n);
}

export function isGlobalComputerAddress(address: string): boolean {
  if (address.includes('%')) return false;
  const family = isIP(address);
  if (family === 4) {
    const value = address.split('.').reduce((total, byte) => total * 256 + Number(byte), 0);
    return !ipv4Denied.some(([network, bits]) => Math.floor(value / 2 ** (32 - bits)) === Math.floor(network / 2 ** (32 - bits)));
  }
  if (family !== 6) return false;
  const value = ipv6Number(address);
  const inRange = (network: string, bits: number) => value >> BigInt(128 - bits) === ipv6Number(network) >> BigInt(128 - bits);
  return inRange('2000::', 3)
    && !inRange('2001::', 23)
    && !inRange('2001:db8::', 32)
    && !inRange('2002::', 16)
    && !inRange('3fff::', 20);
}

export function computerUrl(input: string): URL {
  if (typeof input !== 'string' || input.length > 4096 || /[\u0000-\u0020\u007f\\]/.test(input)) throw new HttpError(400, 'Use an HTTP or HTTPS URL without credentials.');
  let url: URL;
  try { url = new URL(input); } catch { throw new HttpError(400, 'Use a valid HTTP or HTTPS URL.'); }
  if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password) throw new HttpError(403, 'Only HTTP and HTTPS URLs without credentials are allowed.');
  return url;
}

export function computerOrigin(input: string): string {
  const url = computerUrl(input);
  if (url.pathname !== '/' || url.search || url.hash) throw new HttpError(400, 'Allowlist entries must be origins without a path, query, or fragment.');
  return url.origin;
}

export function computerTestMode(): boolean {
  return process.env.NODE_ENV === 'test' || process.env.NODE_TEST_CONTEXT !== undefined;
}

export class ComputerNetworkPolicy {
  readonly origins: readonly string[];
  private readonly allowed: Set<string>;
  private readonly fixtures: Set<string>;
  private readonly lookup: ComputerLookup;

  constructor(origins: readonly string[], options: { allowTestOrigins?: readonly string[]; lookup?: ComputerLookup } = {}) {
    if (!Array.isArray(origins) || origins.length < 1 || origins.length > 16) throw new HttpError(400, 'Select between one and sixteen allowed public origins.');
    this.origins = [...new Set(origins.map(computerOrigin))];
    this.allowed = new Set(this.origins);
    this.fixtures = new Set((options.allowTestOrigins ?? []).map(computerOrigin));
    if (this.fixtures.size && !computerTestMode()) throw new HttpError(403, 'Local fixture origins are available only to tests.');
    for (const origin of this.fixtures) {
      const url = new URL(origin);
      if (url.protocol !== 'http:' || !['127.0.0.1', '[::1]'].includes(url.hostname) || !url.port) throw new HttpError(400, 'Fixture origins must be explicit loopback HTTP ports.');
    }
    this.lookup = options.lookup ?? (hostname => lookup(hostname, { all: true, verbatim: true }));
  }

  allows(input: string): boolean {
    try { return this.allowed.has(computerUrl(input).origin); } catch { return false; }
  }

  async resolve(input: string): Promise<{ url: URL; address: string; family: 4 | 6; port: number }> {
    const url = computerUrl(input);
    if (!this.allowed.has(url.origin)) throw new HttpError(403, 'The destination origin is not allowed.');
    const hostname = url.hostname.replace(/^\[|\]$/g, '');
    let answers: readonly { address: string; family: number }[];
    try { answers = isIP(hostname) ? [{ address: hostname, family: isIP(hostname) }] : await this.lookup(hostname); }
    catch { throw new HttpError(403, 'The destination could not be resolved safely.'); }
    if (!answers.length || answers.some(answer => {
      const family = isIP(answer.address);
      if (!family || family !== answer.family) return true;
      return !(this.fixtures.has(url.origin) && ['127.0.0.1', '::1'].includes(answer.address)) && !isGlobalComputerAddress(answer.address);
    })) throw new HttpError(403, 'The destination resolves to a non-public address.');
    return { url, address: answers[0].address, family: answers[0].family as 4 | 6, port: Number(url.port || (url.protocol === 'https:' ? 443 : 80)) };
  }
}

const hopHeaders = new Set([
  'connection', 'keep-alive', 'proxy-authenticate', 'proxy-authorization', 'te', 'trailer',
  'transfer-encoding', 'upgrade', 'authorization', 'cookie',
]);

function safeHeaders(input: IncomingHttpHeaders): IncomingHttpHeaders {
  const stripped = new Set([...hopHeaders, ...(input.connection ?? '').toLowerCase().split(',').map(value => value.trim())]);
  return Object.fromEntries(Object.entries(input).filter(([name]) => !stripped.has(name.toLowerCase())));
}

export class ComputerProxy {
  private readonly server = createServer((request, response) => { void this.forward(request, response); });
  private readonly sockets = new Set<Socket>();
  private readonly credentials = randomBytes(24).toString('hex');
  private readonly probe = randomBytes(24).toString('hex');
  private stopped = false;
  private closing?: Promise<void>;

  constructor(private readonly policy: ComputerNetworkPolicy) {
    this.server.on('connection', socket => this.track(socket));
    this.server.on('connect', (request, socket, head) => { void this.tunnel(request, socket as Socket, head); });
    this.server.on('upgrade', (_request, socket) => socket.destroy());
    this.server.on('clientError', (_error, socket) => socket.destroy());
    this.server.requestTimeout = 15_000;
    this.server.headersTimeout = 10_000;
    this.server.maxHeadersCount = 100;
  }

  get configuration(): { server: string; username: string; password: string } {
    const address = this.server.address() as AddressInfo | null;
    if (!address || this.stopped) throw new HttpError(409, 'The computer proxy is stopped.');
    return { server: `http://127.0.0.1:${address.port}`, username: 'computer', password: this.credentials };
  }

  get probeUrl(): string { return `${this.configuration.server}/proxy-check/${this.probe}`; }
  get probeBody(): string { return `managed-computer-proxy-${this.probe}`; }

  async start(): Promise<void> {
    if (this.stopped) throw new HttpError(409, 'The computer proxy is stopped.');
    await new Promise<void>((resolve, reject) => {
      this.server.once('error', reject);
      this.server.listen(0, '127.0.0.1', () => { this.server.off('error', reject); resolve(); });
    });
    if (this.stopped) { await this.stop(); throw new HttpError(409, 'The computer proxy is stopped.'); }
  }

  stop(): Promise<void> {
    this.stopped = true;
    for (const socket of this.sockets) socket.destroy();
    this.server.closeAllConnections();
    this.closing ??= new Promise<void>(resolve => this.server.close(() => resolve()));
    return this.closing;
  }

  private track(socket: Socket): Socket {
    this.sockets.add(socket);
    socket.on('error', () => {});
    socket.once('close', () => this.sockets.delete(socket));
    if (this.stopped) socket.destroy();
    return socket;
  }

  private authenticated(request: IncomingMessage): boolean {
    return request.headers['proxy-authorization'] === `Basic ${Buffer.from(`computer:${this.credentials}`).toString('base64')}`;
  }

  private async forward(request: IncomingMessage, response: ServerResponse): Promise<void> {
    try {
      if (!this.authenticated(request)) {
        response.writeHead(407, { 'Proxy-Authenticate': 'Basic realm="managed-computer"', Connection: 'close' });
        response.end();
        return;
      }
      if (this.stopped) throw new HttpError(409, 'The computer proxy is stopped.');
      if (!['GET', 'HEAD', 'OPTIONS'].includes(request.method ?? '') || request.headers.upgrade || request.headers['content-length'] && request.headers['content-length'] !== '0' || request.headers['transfer-encoding']) throw new HttpError(403, 'Only public read-only browsing is allowed.');
      if (request.url === this.probeUrl && request.method === 'GET') {
        response.writeHead(200, { 'Content-Type': 'text/plain', 'Cache-Control': 'no-store' });
        response.end(this.probeBody);
        return;
      }
      const target = await this.policy.resolve(request.url ?? '');
      if (this.stopped) throw new HttpError(409, 'The computer proxy is stopped.');
      if (target.url.protocol !== 'http:') throw new HttpError(403, 'HTTPS requires a pinned CONNECT tunnel.');
      const upstream = httpRequest({
        hostname: target.address, family: target.family, port: target.port,
        method: request.method, path: target.url.pathname + target.url.search,
        headers: { ...safeHeaders(request.headers), host: target.url.host, connection: 'close' },
        agent: false, timeout: 12_000,
      }, remote => {
        if (/attachment/i.test(String(remote.headers['content-disposition'] ?? ''))) {
          remote.destroy();
          response.writeHead(403, { Connection: 'close' });
          response.end('Downloads are not allowed.');
          return;
        }
        const headers = safeHeaders(remote.headers);
        delete headers['set-cookie'];
        response.writeHead(remote.statusCode ?? 502, { ...headers, connection: 'close' });
        remote.on('error', () => response.destroy());
        remote.pipe(response);
      });
      upstream.on('socket', socket => this.track(socket));
      upstream.on('timeout', () => upstream.destroy());
      upstream.on('error', () => {
        if (!response.headersSent) { response.writeHead(502, { Connection: 'close' }); response.end('The upstream request failed.'); }
        else response.destroy();
      });
      response.once('close', () => upstream.destroy());
      upstream.end();
    } catch (error) {
      if (response.headersSent) { response.destroy(); return; }
      response.writeHead(error instanceof HttpError ? error.status : 403, { Connection: 'close' });
      response.end('Network access was denied by the managed computer policy.');
    }
  }

  private async tunnel(request: IncomingMessage, socket: Socket, head: Buffer): Promise<void> {
    try {
      if (!this.authenticated(request)) {
        socket.end('HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm="managed-computer"\r\nConnection: close\r\n\r\n');
        return;
      }
      const authority = request.url ?? '';
      if (!/^(?:\[[0-9a-f:]+\]|[a-z0-9.-]+):[0-9]+$/i.test(authority)) throw new HttpError(403, 'Invalid tunnel authority.');
      const target = await this.policy.resolve(`https://${authority}/`);
      if (this.stopped || socket.destroyed) { socket.destroy(); return; }
      const upstream = this.track(connect({ host: target.address, family: target.family, port: target.port }));
      upstream.setTimeout(30_000, () => upstream.destroy());
      upstream.once('connect', () => {
        if (this.stopped || socket.destroyed) { upstream.destroy(); return; }
        socket.write('HTTP/1.1 200 Connection Established\r\n\r\n');
        if (head.length) upstream.write(head);
        socket.pipe(upstream);
        upstream.pipe(socket);
      });
      upstream.once('error', () => socket.destroy());
      upstream.once('close', () => socket.destroy());
      socket.once('close', () => upstream.destroy());
    } catch { socket.end('HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n'); }
  }
}
