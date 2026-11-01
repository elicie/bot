import type { Express, Request } from 'express';
import { randomUUID } from 'node:crypto';
import { z } from 'zod';
import { ManagedComputer, type ManagedComputerOptions } from './computer.js';
import { Store } from './store.js';
import { HttpError } from './files.js';
import type { EnqueueCommand } from './routines.js';
import type { ComputerAction, ComputerObservation, ComputerStatus } from '../shared/computer.js';

export class ApplicationComputer {
  readonly service: ManagedComputer;
  private ownerId?: string;
  private ownerToken?: symbol;
  private frame?: { epoch: string; frameId: string; buffer: Buffer };
  private version = 0;
  private tail: Promise<unknown> = Promise.resolve();
  constructor(private readonly store: Store, options?: ManagedComputerOptions) { this.service = new ManagedComputer(options); }

  get generation(): number { return this.version; }
  private current(generation: number): void {
    if (generation !== this.version) throw new HttpError(409, 'The managed browser was stopped or replaced. Submit a new request.');
  }
  private owner(id: string): void {
    this.store.bot(id);
    if (this.ownerId && this.ownerId !== id && this.service.status().state !== 'stopped') throw new HttpError(409, 'Another bot owns the managed browser. Stop that browser before switching owners.');
  }
  status(id: string): ComputerStatus { this.owner(id); return this.service.status(); }
  private serialized<T>(generation: number, operation: () => Promise<T>): Promise<T> {
    const result = this.tail.then(() => { this.current(generation); return operation(); });
    this.tail = result.catch(() => {});
    return result;
  }
  start(id: string, input: { requestId: string; userInitiated: true; origins: string[] }, generation = this.version): Promise<ComputerStatus> {
    return this.serialized(generation, async () => {
      this.owner(id);
      if (this.service.status().state !== 'stopped') throw new HttpError(409, 'Stop the current browser before starting another.');
      this.ownerId = id;
      this.ownerToken = Symbol();
      this.frame = undefined;
      const status = await this.service.start(input);
      this.current(generation);
      await this.capture(id, generation, status.epoch);
      return this.service.status();
    });
  }
  async stop(id?: string): Promise<ComputerStatus> {
    if (id) this.owner(id);
    const generation = ++this.version;
    const ownerToken = this.ownerToken;
    this.frame = undefined;
    const value = await this.service.stop();
    if (this.version === generation && this.ownerToken === ownerToken) { this.ownerId = undefined; this.ownerToken = undefined; }
    return value;
  }
  private async capture(id: string, generation: number, epoch: string): Promise<ComputerObservation> {
    this.current(generation);
    this.owner(id);
    const status = this.service.status();
    if (status.epoch !== epoch) throw new HttpError(409, 'The managed browser session changed. Observe again.');
    const value = await this.service.screenshot({ requestId: randomUUID(), epoch, frameId: status.frameId });
    this.current(generation);
    this.frame = { epoch: value.frame.epoch, frameId: value.frame.id, buffer: value.screenshotBuffer };
    return { status: value.status, frame: value.frame };
  }
  preview(id: string, epoch: string, frameId: string): Buffer {
    this.owner(id);
    const status = this.service.status();
    if (status.state !== 'running' || !this.frame) throw new HttpError(409, 'Start the browser before viewing a frame.');
    if (epoch !== status.epoch || frameId !== status.frameId || epoch !== this.frame.epoch || frameId !== this.frame.frameId) throw new HttpError(409, 'The browser preview frame is stale. Refresh its status.');
    return this.frame.buffer;
  }
  perform(id: string, input: { requestId: string; epoch: string; frameId: string | null; action: ComputerAction }, generation = this.version): Promise<ComputerObservation> {
    return this.serialized(generation, async () => {
      this.owner(id);
      await this.service.perform(input);
      return this.capture(id, generation, input.epoch);
    });
  }
}

export function isLocalComputerRequest(request: Request): boolean {
  return ['127.0.0.1', '::1', '::ffff:127.0.0.1'].includes(request.socket.remoteAddress ?? '');
}

export function installComputerRoutes(app: Express, store: Store, computer: ApplicationComputer, command: EnqueueCommand): void {
  const owner = z.string().uuid();
  const local = (request: Request) => {
    if (!isLocalComputerRequest(request)) throw new HttpError(403, 'Managed browser control requires a local loopback connection. This network server has no user authentication.');
  };
  app.get('/api/computer', (request, response) => {
    local(request);
    const input = z.object({ owner }).strict().parse(request.query);
    response.json(computer.status(input.owner));
  });
  app.post('/api/computer', async (request, response) => {
    local(request);
    const { ownerId, ...input } = z.object({ ownerId: owner, requestId: z.string().uuid(), userInitiated: z.literal(true), origins: z.array(z.string()).min(1).max(16) }).strict().parse(request.body);
    const generation = computer.generation;
    response.json(await command(async () => { store.assertIdle(); return computer.start(ownerId, input, generation); }));
  });
  app.delete('/api/computer', async (request, response) => {
    local(request);
    const input = z.object({ ownerId: owner }).strict().parse(request.body);
    response.json(await computer.stop(input.ownerId));
  });
  app.get('/api/computer/preview', (request, response) => {
    local(request);
    const input = z.object({ owner, epoch: z.string().uuid(), frame: z.string().uuid() }).strict().parse(request.query);
    response.type('image/png').set('Cache-Control', 'no-store').send(computer.preview(input.owner, input.epoch, input.frame));
  });
  app.post('/api/computer/actions', async (request, response) => {
    local(request);
    const input = z.object({ ownerId: owner, requestId: z.string().uuid(), epoch: z.string().uuid(), frameId: z.string().uuid().nullable(), action: z.unknown() }).strict().parse(request.body);
    const generation = computer.generation;
    response.json(await command(async () => {
      store.assertIdle();
      return computer.perform(input.ownerId, { requestId: input.requestId, epoch: input.epoch, frameId: input.frameId, action: input.action as ComputerAction }, generation);
    }));
  });
}
