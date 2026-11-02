import express, { type ErrorRequestHandler, type Request, type Response } from 'express';
import { createServer, type IncomingMessage, type Server } from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { z } from 'zod';
import { HttpError, WorkspaceFiles } from './files.js';
import { avatarShapeSchema, baseUrl, checkProvider, Store } from './store.js';
import { GrokRuntime, type RuntimeOptions } from './runtime.js';
import { ApplicationComputer, installComputerRoutes, isLocalComputerRequest } from './computer-api.js';
import type { ManagedComputerOptions } from './computer.js';
import { projectComputerPermissions, projectResponse } from './public-state.js';
import { importSkillMarkdown, skillInputSchema, skillPatchSchema } from './skills.js';
import { RoutineScheduler, routineInputSchema, type EnqueueCommand, type RoutineSchedulerOptions } from './routines.js';
import { canvasInputSchema, canvasRestoreSchema } from './canvas.js';

const name = z.string().trim().min(1).max(100);
const color = z.string().regex(/^#[\da-fA-F]{6}$/);
const instructions = z.string().max(30000);
const settingsInput = z.object({
  baseUrl: z.string().max(2048), model: z.string().trim().min(1).max(256), workspace: z.string().max(4096),
  apiKey: z.string().max(16384).refine(value => !/[\r\n\0]/.test(value), 'API key contains invalid characters.').optional(), clearApiKey: z.boolean().optional(),
}).strict();
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

export interface BackendOptions {
  dataDir: string;
  runtime?: RuntimeOptions;
  computer?: ManagedComputerOptions;
  routines?: RoutineSchedulerOptions;
  dev?: boolean;
  serveWeb?: boolean;
  allowedHosts?: string[];
}

export async function createBackend(options: BackendOptions): Promise<{
  server: Server;
  store: Store;
  runtime: GrokRuntime;
  routines: RoutineScheduler;
  close: () => Promise<void>;
}> {
  const store = await Store.open(options.dataDir, { now: options.routines?.now });
  const app = express();
  const server = createServer(app);
  const streams = new Set<Response>();
  let vite: { close: () => Promise<void> } | undefined;
  let closing = false;
  let commandTail: Promise<unknown> = Promise.resolve();
  const command: EnqueueCommand = operation => {
    const result = commandTail.then(operation);
    commandTail = result.catch(() => {});
    return result;
  };
  const computer = new ApplicationComputer(store, options.computer);
  const runtime = new GrokRuntime(store, options.runtime, command, computer);
  await runtime.initialize();
  const routines = new RoutineScheduler(store, runtime, command, options.routines);
  app.disable('x-powered-by');
  app.set('trust proxy', false);
  const normalizeHost = (value: string) => value.toLowerCase().replace(/\.$/, '');
  const allowedHosts = new Set(['localhost', '127.0.0.1', '[::1]', ...(options.allowedHosts ?? [])].map(normalizeHost));
  const originError = (request: IncomingMessage): string | undefined => {
    const host = request.headers.host;
    let parsed: URL;
    try {
      if (!host || /[\s/@?#\\]/.test(host)) return 'Host is not allowed.';
      parsed = new URL(`http://${host}`);
    } catch { return 'Host is not allowed.'; }
    if (!allowedHosts.has(normalizeHost(parsed.hostname))) return 'Host is not allowed.';
    const address = server.address();
    if (address && typeof address !== 'string' && Number(parsed.port || 80) !== address.port) return 'Host does not match this server.';
    if (request.headers.origin !== undefined && request.headers.origin !== parsed.origin) return 'Cross-origin requests are not accepted.';
    return undefined;
  };
  server.on('upgrade', (request, socket) => {
    if (originError(request) || !request.headers.origin) {
      socket.end('HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n');
      socket.destroy();
    }
  });
  app.use((request, response, next) => {
    const error = originError(request);
    if (error) { response.status(403).json({ error }); return; }
    response.setHeader('X-Content-Type-Options', 'nosniff');
    response.setHeader('Referrer-Policy', 'no-referrer');
    response.setHeader('Cross-Origin-Resource-Policy', 'same-origin');
    response.setHeader('X-Frame-Options', 'DENY');
    next();
  });
  app.use('/api', (request, response, next) => {
    response.setHeader('Cache-Control', 'no-store');
    if (!['GET', 'HEAD'].includes(request.method)) {
      if (request.headers['x-grok-bot'] !== '1' || !request.headers.origin || !request.is('application/json')) {
        response.status(403).json({ error: 'Mutations require same-origin JSON and X-Grok-Bot: 1.' }); return;
      }
      if (closing) { response.status(503).json({ error: 'The server is shutting down.' }); return; }
    }
    next();
  });
  app.use('/api', (request, response, next) => {
    const local = isLocalComputerRequest(request);
    const json = response.json.bind(response);
    response.json = body => json(projectResponse(body, local));
    next();
  });
  app.use(express.json({ limit: '2mb', strict: true }));
  const state = (_request: Request, response: Response) => { response.json(store.snapshot()); };
  const workspace = () => {
    if (!store.settings.workspace) throw new HttpError(400, 'Choose a workspace in settings first.');
    return new WorkspaceFiles(store.settings.workspace);
  };
  installComputerRoutes(app, store, computer, command);
  app.get('/api/state', state);
  app.get('/api/events', (request, response) => {
    response.status(200).set({ 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store', Connection: 'keep-alive', 'X-Accel-Buffering': 'no' });
    response.flushHeaders();
    const send = (snapshot: ReturnType<Store['snapshot']>) => {
      if (response.writableLength > 4 * 1024 * 1024) { response.destroy(); return; }
      response.write(`event: state\ndata: ${JSON.stringify(projectComputerPermissions(snapshot, isLocalComputerRequest(request)))}\n\n`);
    };
    send(store.snapshot());
    streams.add(response);
    const unsubscribe = store.subscribe(send);
    const heartbeat = setInterval(() => response.write(': heartbeat\n\n'), 15000);
    heartbeat.unref();
    request.on('close', () => { clearInterval(heartbeat); unsubscribe(); streams.delete(response); });
  });
  app.put('/api/settings', async (request, response) => {
    await command(() => store.updateSettings(settingsInput.parse(request.body)));
    response.json(store.snapshot());
  });
  app.post('/api/provider/test', async (request, response) => {
    const input = z.object({ baseUrl: z.string().max(2048).optional(), apiKey: settingsInput.shape.apiKey }).strict().parse(request.body);
    const target = baseUrl(input.baseUrl?.trim() || store.settings.baseUrl);
    const key = input.apiKey?.trim() || (target === store.settings.baseUrl ? store.settings.apiKey : '');
    const result = await checkProvider({ baseUrl: target, apiKey: key, model: target === store.settings.baseUrl ? store.settings.model : '' });
    response.json(result);
  });
  app.post('/api/bots', async (request, response) => {
    const input = z.object({ name, color: color.default('#8b5cf6'), instructions: instructions.default(''), shape: avatarShapeSchema.optional(), pinned: z.boolean().optional() }).strict().parse(request.body);
    await command(() => store.addBot(input));
    response.status(201).json(store.snapshot());
  });
  app.patch('/api/bots/:id', async (request, response) => {
    const input = z.object({ name: name.optional(), color: color.optional(), instructions: instructions.optional(), shape: avatarShapeSchema.optional(), pinned: z.boolean().optional(), hidden: z.boolean().optional() }).strict().parse(request.body);
    if (!Object.keys(input).length) throw new HttpError(400, 'Provide a bot field to update.');
    await command(() => store.updateBot(String(request.params.id), input));
    response.json(store.snapshot());
  });
  app.post('/api/routines', async (request, response) => {
    const input = routineInputSchema.parse(request.body);
    await command(() => store.addRoutine(input));
    response.status(201).json(store.snapshot());
  });
  app.put('/api/routines/:id', async (request, response) => {
    const input = routineInputSchema.parse(request.body);
    await command(() => store.updateRoutine(String(request.params.id), input));
    response.json(store.snapshot());
  });
  app.patch('/api/routines/:id', async (request, response) => {
    const input = z.object({ enabled: z.boolean() }).strict().parse(request.body);
    await command(() => store.setRoutineEnabled(String(request.params.id), input.enabled));
    response.json(store.snapshot());
  });
  app.delete('/api/routines/:id', async (request, response) => {
    z.object({}).strict().parse(request.body);
    await command(() => store.deleteRoutine(String(request.params.id)));
    response.json(store.snapshot());
  });
  app.post('/api/routines/:id/run', async (request, response) => {
    z.object({}).strict().parse(request.body);
    const run = await routines.run(String(request.params.id));
    const state = store.snapshot();
    response.status(202).json({ run: state.routineRuns.find(item => item.id === run.id)!, state });
  });
  app.post('/api/bots/:botId/skills/import', async (request, response) => {
    const input = z.object({ markdown: z.string().max(20 * 1024) }).strict().parse(request.body);
    const skill = importSkillMarkdown(input.markdown);
    await command(() => store.addSkill(String(request.params.botId), skill));
    response.status(201).json(store.snapshot());
  });
  app.post('/api/bots/:botId/skills', async (request, response) => {
    const input = skillInputSchema.parse(request.body);
    await command(() => store.addSkill(String(request.params.botId), input));
    response.status(201).json(store.snapshot());
  });
  app.patch('/api/bots/:botId/skills/:id', async (request, response) => {
    const input = skillPatchSchema.parse(request.body);
    await command(() => store.updateSkill(String(request.params.botId), String(request.params.id), input));
    response.json(store.snapshot());
  });
  app.delete('/api/bots/:botId/skills/:id', async (request, response) => {
    const input = z.object({ revision: z.number().int().min(1) }).strict().parse(request.body);
    await command(() => store.deleteSkill(String(request.params.botId), String(request.params.id), input.revision));
    response.json(store.snapshot());
  });
  app.post('/api/conversations', async (request, response) => {
    const input = z.object({ botId: z.string().uuid() }).strict().parse(request.body);
    const conversation = await command(() => store.addConversation(input.botId));
    response.status(201).json({ conversation, state: store.snapshot() });
  });
  app.put('/api/conversations/:id/canvas', async (request, response) => {
    const input = canvasInputSchema.parse(request.body);
    await command(() => store.updateCanvas(String(request.params.id), input));
    response.json(store.snapshot());
  });
  app.post('/api/conversations/:id/canvas/restore', async (request, response) => {
    const input = canvasRestoreSchema.parse(request.body);
    await command(() => store.restoreCanvas(String(request.params.id), input));
    response.json(store.snapshot());
  });
  app.post('/api/conversations/:id/messages', async (request, response) => {
    const input = z.object({ text: z.string().trim().min(1).max(100000) }).strict().parse(request.body);
    await command(() => {
      const id = String(request.params.id);
      if (store.isReservedConversation(id)) {
        throw new HttpError(409, 'This conversation is reserved for queued work or a read-only bot delivery.');
      }
      return runtime.start(id, input.text);
    });
    response.status(202).json(store.snapshot());
  });
  app.post('/api/conversations/:id/cancel', async (request, response) => {
    z.object({}).strict().parse(request.body);
    const cancellation = await command(async () => {
      const id = String(request.params.id);
      return { done: await store.cancelQueuedWork(id) ? Promise.resolve() : runtime.cancel(id) };
    });
    await cancellation.done;
    response.json(store.snapshot());
  });
  app.post('/api/conversations/:id/permissions/:permissionId', async (request, response) => {
    const input = z.object({ optionId: z.string().min(1).max(256) }).strict().parse(request.body);
    await command(() => runtime.resolve(String(request.params.id), String(request.params.permissionId), input.optionId, isLocalComputerRequest(request)));
    response.json(store.snapshot());
  });
  app.get('/api/files', async (request, response) => {
    const input = z.object({ path: z.string().default('') }).strict().parse(request.query);
    response.json(await workspace().list(input.path));
  });
  app.get('/api/file', async (request, response) => {
    const input = z.object({ path: z.string() }).strict().parse(request.query);
    response.json(await workspace().read(input.path));
  });
  app.put('/api/file', async (request, response) => {
    const input = z.object({ path: z.string().max(4096), content: z.string(), hash: z.string().regex(/^(?:[\da-f]{64})?$/) }).strict().parse(request.body);
    response.json(await command(() => { store.assertIdle(); return workspace().save(input.path, input.content, input.hash); }));
  });
  app.post('/internal/mcp/:runId', async (request, response) => {
    const result = await runtime.mcp(String(request.params.runId), request.headers.authorization, request.body);
    if (result === undefined) response.status(202).end();
    else response.json(result);
  });
  app.get('/internal/mcp/:runId', (_request, response) => { response.status(405).end(); });
  app.use('/api', (_request, response) => { response.status(404).json({ error: 'API endpoint does not exist.' }); });
  if (options.serveWeb !== false) {
    if (options.dev) {
      const { createServer: createViteServer } = await import('vite');
      const dev = await createViteServer({ root, cacheDir: path.join(store.dataDir, 'vite-cache'), server: { middlewareMode: true, allowedHosts: [...allowedHosts], ws: { server } }, appType: 'spa' });
      vite = dev;
      app.use(dev.middlewares);
    } else {
      app.use(express.static(path.join(root, 'dist'), { index: false }));
      app.get('/{*path}', (request, response, next) => {
        if (request.path.startsWith('/internal')) { response.status(404).end(); return; }
        response.sendFile(path.join(root, 'dist/index.html'), error => { if (error) next(error); });
      });
    }
  }
  const handleError: ErrorRequestHandler = (error: unknown, _request, response, _next) => {
    if (response.headersSent) { response.end(); return; }
    if (error instanceof HttpError) { response.status(error.status).json({ error: error.message }); return; }
    if (error instanceof z.ZodError) { response.status(400).json({ error: error.issues.map(issue => `${issue.path.join('.') || 'Request'}: ${issue.message}`).join('; ').slice(0, 1000) }); return; }
    const status = (error as { status?: number }).status;
    const code = (error as NodeJS.ErrnoException).code;
    if (status === 413) { response.status(413).json({ error: 'JSON request exceeds the 2 MiB limit.' }); return; }
    if (status === 400) { response.status(400).json({ error: 'Request body must be valid JSON.' }); return; }
    if (code === 'ENOENT') { response.status(404).json({ error: 'File or directory does not exist. Build the frontend before production startup.' }); return; }
    if (code === 'EACCES' || code === 'EPERM') { response.status(403).json({ error: 'File access was denied.' }); return; }
    response.status(500).json({ error: 'The operation failed. Check that the workspace and data directory are accessible.' });
  };
  app.use(handleError);
  server.on('listening', () => {
    const address = server.address();
    if (address && typeof address !== 'string') runtime.setOrigin(`http://127.0.0.1:${address.port}`);
    routines.start();
  });
  return {
    server, store, runtime, routines,
    close: async () => {
      closing = true;
      try {
        await routines.close();
        await commandTail;
        await runtime.shutdown();
      } finally {
        await computer.stop();
        for (const response of streams) response.end();
        await vite?.close();
        await new Promise<void>((resolve, reject) => {
          if (!server.listening) { resolve(); return; }
          server.close(error => { if (error) reject(error); else resolve(); });
          server.closeIdleConnections();
        });
      }
    },
  };
}
