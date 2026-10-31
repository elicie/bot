import { constants } from 'node:fs';
import { open } from 'node:fs/promises';
import { randomUUID } from 'node:crypto';
import path from 'node:path';
import { z } from 'zod';
import type { AppState, Bot, Conversation, PublicSettings, RuntimeInfo, SettingsInput } from '../shared/types.js';
import { atomicPrivateFile, canonicalDirectory, HttpError, isMissing, privateDirectory } from './files.js';

const toolSchema = z.object({
  id: z.string(), title: z.string(), kind: z.string(), status: z.enum(['pending', 'in_progress', 'completed', 'failed']),
  input: z.string().optional(), output: z.string().optional(), paths: z.array(z.string()).optional(),
}).strict();
const permissionSchema = z.object({
  id: z.string(), title: z.string(), kind: z.string(), description: z.string(), paths: z.array(z.string()),
  options: z.array(z.object({ id: z.string(), label: z.string(), kind: z.string() }).strict()),
}).strict();
const botSchema = z.object({ id: z.string().uuid(), name: z.string(), color: z.string(), instructions: z.string(), createdAt: z.string() }).strict();
const conversationSchema = z.object({
  id: z.string().uuid(), botId: z.string().uuid(), title: z.string(), updatedAt: z.string(),
  sessionId: z.string().optional(), sessionFingerprint: z.string().optional(),
  messages: z.array(z.object({
    id: z.string().uuid(), role: z.enum(['user', 'assistant']), text: z.string(), createdAt: z.string(), tools: z.array(toolSchema),
  }).strict()),
  run: z.object({
    id: z.string(), status: z.enum(['idle', 'starting', 'running', 'awaiting_permission', 'completed', 'cancelled', 'error', 'interrupted']),
    error: z.string().optional(), permission: permissionSchema.optional(), startedAt: z.string().optional(), finishedAt: z.string().optional(),
  }).strict(),
}).strict();
const settingsSchema = z.object({ baseUrl: z.string(), model: z.string(), workspace: z.string(), apiKey: z.string() }).strict();
const diskSchema = z.object({
  version: z.literal(1), revision: z.number().int().nonnegative().default(0), settings: settingsSchema, bots: z.array(botSchema).min(1), conversations: z.array(conversationSchema),
}).strict();
type DiskState = z.infer<typeof diskSchema>;
export type PrivateSettings = DiskState['settings'];
export const activeStatus = (status: Conversation['run']['status']) => ['starting', 'running', 'awaiting_permission'].includes(status);

export function baseUrl(input: string): string {
  let url: URL;
  try { url = new URL(input); } catch { throw new HttpError(400, 'Base URL must be an absolute HTTP or HTTPS URL.'); }
  if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.search || url.hash) {
    throw new HttpError(400, 'Base URL must use HTTP or HTTPS without credentials, query parameters, or fragments.');
  }
  return url.href.replace(/\/+$/, '');
}

export function loopback(host: string): boolean {
  return host === 'localhost' || host === '127.0.0.1' || host === '[::1]' || host === '::1';
}

export class Store {
  private disk: DiskState;
  private writeTail: Promise<void> = Promise.resolve();
  private writeError?: Error;
  private listeners = new Set<(state: AppState) => void>();
  runtime: RuntimeInfo = { available: false, error: 'Checking Grok Build.' };

  private constructor(readonly dataDir: string, disk: DiskState) { this.disk = disk; }

  static async open(dataDir: string): Promise<Store> {
    const root = await privateDirectory(dataDir);
    let disk: DiskState;
    try {
      const file = await open(path.join(root, 'state.json'), constants.O_RDONLY | constants.O_NOFOLLOW);
      try {
        const stat = await file.stat();
        if (!stat.isFile() || stat.nlink !== 1 || stat.size > 64 * 1024 * 1024) throw new Error('Invalid application state file.');
        await file.chmod(0o600);
        disk = diskSchema.parse(JSON.parse(await file.readFile('utf8')));
      } finally { await file.close(); }
      if (disk.settings.baseUrl) baseUrl(disk.settings.baseUrl);
      const ids = new Set(disk.bots.map(bot => bot.id));
      if (ids.size !== disk.bots.length || new Set(disk.conversations.map(c => c.id)).size !== disk.conversations.length || disk.conversations.some(c => !ids.has(c.botId))) {
        throw new Error('Application state contains invalid references.');
      }
    } catch (error) {
      if (!isMissing(error)) throw new Error('Cannot load application state. The existing state.json was not replaced.', { cause: error });
      disk = {
        version: 1, revision: 0, settings: { baseUrl: '', model: '', workspace: '', apiKey: '' }, conversations: [],
        bots: [{ id: randomUUID(), name: 'Grok', color: '#8b5cf6', instructions: '', createdAt: new Date().toISOString() }],
      };
    }
    const store = new Store(root, disk);
    for (const conversation of disk.conversations) {
      if (!activeStatus(conversation.run.status)) continue;
      conversation.run.status = 'interrupted';
      conversation.run.error = 'The server stopped before this run finished.';
      conversation.run.finishedAt = new Date().toISOString();
      delete conversation.run.permission;
      for (const message of conversation.messages) for (const tool of message.tools) {
        if (tool.status === 'in_progress' || tool.status === 'pending') tool.status = 'failed';
      }
    }
    await store.changed();
    return store;
  }

  get settings(): Readonly<PrivateSettings> { return this.disk.settings; }
  get conversations(): Conversation[] { return this.disk.conversations; }
  get bots(): Bot[] { return this.disk.bots; }
  get active(): Conversation | undefined { return this.conversations.find(c => activeStatus(c.run.status)); }

  snapshot(): AppState {
    const { apiKey, ...settings } = this.disk.settings;
    const state = { revision: this.disk.revision, bots: this.bots, conversations: this.conversations, settings: { ...settings, hasApiKey: Boolean(apiKey) }, runtime: this.runtime };
    return JSON.parse(JSON.stringify(state, (field, value: unknown) => {
      if (['kind', 'status', 'role', 'color', 'hash'].includes(field)) return value;
      if (['id', 'botId', 'sessionId', 'sessionFingerprint'].includes(field) && typeof value === 'string' && /^(?:[a-f\d]{8}-[a-f\d]{4}-[a-f\d]{4}-[a-f\d]{4}-[a-f\d]{12}|[a-f\d]{64})$/i.test(value)) return value;
      return typeof value === 'string' && apiKey ? value.split(apiKey).join('[redacted]') : value;
    })) as AppState;
  }

  subscribe(listener: (state: AppState) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  changed(): Promise<void> {
    this.disk.revision += 1;
    const bytes = JSON.stringify(this.disk);
    const state = this.snapshot();
    const write = this.writeTail.then(async () => {
      if (this.writeError) throw this.writeError;
      try { await atomicPrivateFile(path.join(this.dataDir, 'state.json'), bytes); }
      catch (error) {
        this.writeError = error instanceof Error ? error : new Error('State persistence failed.');
        this.runtime = { ...this.runtime, available: false, error: 'Application state could not be saved. Check the data directory before continuing.' };
        for (const listener of this.listeners) listener(this.snapshot());
        throw this.writeError;
      }
      for (const listener of this.listeners) listener(state);
    });
    this.writeTail = write.catch(() => {});
    return write;
  }

  async flush(): Promise<void> { await this.writeTail; if (this.writeError) throw this.writeError; }

  assertWritable(): void {
    if (this.writeError) throw new HttpError(503, 'State could not be persisted. Restart after checking the data directory.');
  }

  assertIdle(): void {
    this.assertWritable();
    if (this.active) throw new HttpError(409, 'Stop the active run before changing settings, bots, or files.');
  }

  conversation(id: string): Conversation {
    const conversation = this.conversations.find(c => c.id === id);
    if (!conversation) throw new HttpError(404, 'Conversation does not exist.');
    return conversation;
  }

  bot(id: string): Bot {
    const bot = this.bots.find(b => b.id === id);
    if (!bot) throw new HttpError(404, 'Bot does not exist.');
    return bot;
  }

  async updateSettings(input: SettingsInput): Promise<void> {
    this.assertIdle();
    const canonicalUrl = baseUrl(input.baseUrl.trim());
    if (!input.model.trim()) throw new HttpError(400, 'Choose a model ID.');
    const workspace = await canonicalDirectory(input.workspace.trim());
    if (workspace === this.dataDir || workspace.startsWith(`${this.dataDir}${path.sep}`) || this.dataDir.startsWith(`${workspace}${path.sep}`) || workspace === path.parse(workspace).root) {
      throw new HttpError(400, 'Workspace must not contain the application data directory or be the filesystem root.');
    }
    const sameProvider = this.settings.baseUrl && new URL(this.settings.baseUrl).origin === new URL(canonicalUrl).origin;
    const apiKey = input.clearApiKey ? '' : input.apiKey?.trim() || (sameProvider ? this.settings.apiKey : '');
    if (!apiKey && !loopback(new URL(canonicalUrl).hostname)) throw new HttpError(400, 'An API key is required for non-loopback providers.');
    this.disk.settings = { baseUrl: canonicalUrl, model: input.model.trim(), workspace, apiKey };
    for (const conversation of this.conversations) { delete conversation.sessionId; delete conversation.sessionFingerprint; }
    await this.changed();
  }

  async addBot(input: Pick<Bot, 'name' | 'color' | 'instructions'>): Promise<Bot> {
    this.assertIdle();
    const bot = { ...input, id: randomUUID(), createdAt: new Date().toISOString() };
    this.bots.push(bot);
    await this.changed();
    return bot;
  }

  async updateBot(id: string, input: Partial<Pick<Bot, 'name' | 'color' | 'instructions'>>): Promise<void> {
    this.assertIdle();
    Object.assign(this.bot(id), input);
    for (const conversation of this.conversations.filter(c => c.botId === id)) { delete conversation.sessionId; delete conversation.sessionFingerprint; }
    await this.changed();
  }

  async addConversation(botId: string): Promise<Conversation> {
    this.assertWritable();
    this.bot(botId);
    const conversation: Conversation = {
      id: randomUUID(), botId, title: 'New conversation', messages: [], run: { id: '', status: 'idle' }, updatedAt: new Date().toISOString(),
    };
    this.conversations.push(conversation);
    await this.changed();
    return conversation;
  }
}

export async function checkProvider(settings: Pick<PublicSettings, 'baseUrl' | 'model'> & { apiKey: string }): Promise<{ ok: boolean; models: string[]; error?: string }> {
  const url = baseUrl(settings.baseUrl);
  try {
    const response = await fetch(`${url}/models`, {
      headers: settings.apiKey ? { Authorization: `Bearer ${settings.apiKey}` } : {},
      signal: AbortSignal.timeout(6000), redirect: 'error',
    });
    if (!response.ok) { await response.body?.cancel(); return { ok: false, models: [], error: `Provider returned HTTP ${response.status}. Check its URL and credentials.` }; }
    const reader = response.body?.getReader();
    if (!reader) throw new Error('Empty response');
    const chunks: Uint8Array[] = [];
    let length = 0;
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      length += value.byteLength;
      if (length > MAX_PROVIDER_BYTES) { await reader.cancel(); return { ok: false, models: [], error: 'Provider model list exceeded 1 MiB.' }; }
      chunks.push(value);
    }
    const result = z.object({ data: z.array(z.object({ id: z.string() })).max(10000) }).parse(JSON.parse(Buffer.concat(chunks).toString('utf8')));
    const models = [...new Set(result.data.map(model => model.id))].sort();
    if (settings.model && !models.includes(settings.model)) return { ok: false, models, error: `The configured model is not in the provider model list.` };
    return { ok: true, models };
  } catch (error) {
    if (error instanceof z.ZodError || error instanceof SyntaxError) return { ok: false, models: [], error: 'Provider did not return an OpenAI-compatible model list.' };
    return { ok: false, models: [], error: 'Provider could not be reached within 6 seconds. Check its URL and that it is running.' };
  }
}
const MAX_PROVIDER_BYTES = 1024 * 1024;
