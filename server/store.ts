import { constants } from 'node:fs';
import { open } from 'node:fs/promises';
import { randomUUID } from 'node:crypto';
import path from 'node:path';
import { z } from 'zod';
import type { AppState, Bot, BotDelivery, BotExchange, Conversation, PublicSettings, Routine, RoutineInput, RoutineRun, RuntimeInfo, SettingsInput, Skill } from '../shared/types.js';
import { atomicPrivateFile, canonicalDirectory, HttpError, isMissing, privateDirectory } from './files.js';
import { MAX_SKILLS_PER_BOT, newSkill, nextSkill, skillSchema, skillPatchSchema, validateSkillSet, type SkillInput } from './skills.js';
import { latestRoutineTime, nextRoutineTime, routineInputSchema, routineRunSchema, routineSchema } from './routines.js';
import { botDeliverySchema, botExchangeSchema, botMessageInputSchema, existingBotDelivery, MAX_BOT_DELIVERIES, MAX_BOT_HOPS, MAX_BOT_MESSAGE_BYTES, MAX_BOT_REPLY_CHARACTERS, reconcileBotDeliveries, validBotMessageReferences, type BotMessageInput } from './bot-messages.js';

/** New app inputs are encoded JSON, so redact decoded keys/strings before serializing again. */
function publicAppInput(input: string, apiKey: string): string {
  const redact = (value: unknown): unknown => {
    if (typeof value === 'string') return value.split(apiKey).join('[redacted]');
    if (Array.isArray(value)) return value.map(redact);
    if (value && typeof value === 'object') {
      const entries = Object.entries(value).map(([key, item]) => [key.split(apiKey).join('[redacted]'), redact(item)] as const);
      if (new Set(entries.map(([key]) => key)).size !== entries.length) throw new Error('Redacted key collision');
      return Object.fromEntries(entries);
    }
    return value;
  };
  try {
    const parsed: unknown = JSON.parse(input);
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return '{}';
    return JSON.stringify(redact(parsed));
  } catch { return '{}'; }
}

const toolSchema = z.object({
  id: z.string(), title: z.string(), kind: z.string(), status: z.enum(['pending', 'in_progress', 'completed', 'failed']),
  input: z.string().optional(), output: z.string().optional(), paths: z.array(z.string()).optional(), localOnly: z.boolean().optional(),
  mcpApp: z.object({
    botId: z.string().uuid(), serverId: z.string().uuid(), serverUpdatedAt: z.string(), server: z.string().max(32), tool: z.string().max(128),
    uri: z.string().max(2048).startsWith('ui://'), html: z.string().refine(value => Buffer.byteLength(value) <= 200_000),
    input: z.string().refine(value => Buffer.byteLength(value) <= 32 * 1024), output: z.string().refine(value => Buffer.byteLength(value) <= 64 * 1024),
  }).strict().optional(),
}).strict();
const permissionSchema = z.object({
  id: z.string(), title: z.string(), kind: z.string(), description: z.string(), paths: z.array(z.string()),
  options: z.array(z.object({ id: z.string(), label: z.string(), kind: z.string() }).strict()),
  localOnly: z.boolean().optional(),
}).strict();
export const avatarShapeSchema = z.enum(['blob', 'cloud', 'square', 'spark', 'clover', 'heart', 'flower', 'droplet', 'pill', 'wedge', 'house', 'star', 'hexagon', 'pumpkin']);
const botSchema = z.object({ id: z.string().uuid(), name: z.string(), color: z.string(), instructions: z.string(), shape: avatarShapeSchema.optional(), skills: z.array(skillSchema).max(MAX_SKILLS_PER_BOT).optional(), pinned: z.boolean().optional(), hidden: z.boolean().optional(), createdAt: z.string() }).strict();
const conversationSchema = z.object({
  id: z.string().uuid(), botId: z.string().uuid(), title: z.string(), updatedAt: z.string(),
  sessionId: z.string().optional(), sessionFingerprint: z.string().optional(),
  messages: z.array(z.object({
    id: z.string().uuid(), role: z.enum(['user', 'assistant']), text: z.string(), createdAt: z.string(), tools: z.array(toolSchema),
    botExchangeIds: z.array(z.string().uuid()).max(1).optional(), sourceBotId: z.string().uuid().optional(),
  }).strict()),
  run: z.object({
    id: z.string(), status: z.enum(['idle', 'starting', 'running', 'awaiting_permission', 'completed', 'cancelled', 'error', 'interrupted']),
    error: z.string().optional(), permission: permissionSchema.optional(), startedAt: z.string().optional(), finishedAt: z.string().optional(),
    todos: z.array(z.object({ content: z.string(), status: z.enum(['pending', 'in_progress', 'completed', 'cancelled']) }).strict()).optional(),
  }).strict(),
}).strict();
const settingsSchema = z.object({ baseUrl: z.string(), model: z.string(), workspace: z.string(), apiKey: z.string() }).strict();
const MAX_STATE_BYTES = 64 * 1024 * 1024;
const MAX_ROUTINES = 1000;
const MAX_ROUTINE_RUNS = 10000;
const diskSchema = z.object({
  version: z.literal(1), revision: z.number().int().nonnegative().default(0), settings: settingsSchema, bots: z.array(botSchema).min(1), conversations: z.array(conversationSchema),
  routines: z.array(routineSchema).max(MAX_ROUTINES).default([]), routineRuns: z.array(routineRunSchema).max(MAX_ROUTINE_RUNS).default([]),
  botExchanges: z.array(botExchangeSchema).max(MAX_BOT_DELIVERIES).default([]), botDeliveries: z.array(botDeliverySchema).max(MAX_BOT_DELIVERIES).default([]),
}).strict();
type DiskState = Pick<AppState, 'revision' | 'bots' | 'conversations' | 'routines' | 'routineRuns'> & {
  version: 1;
  botExchanges: BotExchange[];
  botDeliveries: BotDelivery[];
  settings: z.infer<typeof settingsSchema>;
};
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
  routineSchedulingError?: string;

  private constructor(readonly dataDir: string, disk: DiskState, private readonly now: () => Date) { this.disk = disk; }

  static async open(dataDir: string, options: { now?: () => Date } = {}): Promise<Store> {
    const now = options.now ?? (() => new Date());
    const root = await privateDirectory(dataDir);
    let disk: DiskState;
    try {
      const file = await open(path.join(root, 'state.json'), constants.O_RDONLY | constants.O_NOFOLLOW);
      try {
        const stat = await file.stat();
        if (!stat.isFile() || stat.nlink !== 1 || stat.size > MAX_STATE_BYTES) throw new Error('Invalid application state file.');
        await file.chmod(0o600);
        disk = diskSchema.parse(JSON.parse(await file.readFile('utf8')));
      } finally { await file.close(); }
      if (disk.settings.baseUrl) baseUrl(disk.settings.baseUrl);
      for (const bot of disk.bots) validateSkillSet(bot.skills ?? []);
      const botIds = new Set(disk.bots.map(bot => bot.id));
      const conversationIds = new Set(disk.conversations.map(conversation => conversation.id));
      const entityIds = [...botIds, ...conversationIds, ...disk.routines.map(routine => routine.id), ...disk.routineRuns.map(run => run.id), ...disk.botExchanges.map(exchange => exchange.id), ...disk.botDeliveries.map(delivery => delivery.id)];
      const uniqueIds = new Set(entityIds);
      const linkedConversations = new Set<string>();
      const pendingRoutines = new Set<string>();
      const invalidRuns = disk.routineRuns.some(run => {
        const conversation = disk.conversations.find(item => item.id === run.conversationId);
        const routine = disk.routines.find(item => item.id === run.routineId);
        const linkedExecution = conversation && (!run.executionRunId || run.executionRunId === conversation.run.id);
        const pending = !run.result && linkedExecution && (conversation.run.status === 'idle' || activeStatus(conversation.run.status));
        const invalid = !botIds.has(run.botId) || !conversation || conversation.botId !== run.botId || linkedConversations.has(run.conversationId)
          || (!run.result && !linkedExecution) || (pending && (!routine || pendingRoutines.has(run.routineId)));
        linkedConversations.add(run.conversationId);
        if (pending) pendingRoutines.add(run.routineId);
        return invalid;
      });
      if (botIds.size !== disk.bots.length || conversationIds.size !== disk.conversations.length || uniqueIds.size !== entityIds.length
        || disk.routineRuns.some(run => uniqueIds.has(run.routineId) && !disk.routines.some(routine => routine.id === run.routineId))
        || disk.conversations.some(conversation => !botIds.has(conversation.botId))
        || disk.routines.some(routine => !botIds.has(routine.botId) || (routine.enabled && !routine.nextRunAt)
          || (routine.schedule.kind === 'once' && routine.nextRunAt && Date.parse(routine.nextRunAt) !== Date.parse(routine.schedule.at)))
        || invalidRuns || !validBotMessageReferences(disk.botDeliveries, disk.botExchanges, disk.conversations, botIds, linkedConversations)) {
        throw new Error('Application state contains invalid references.');
      }
    } catch (error) {
      if (!isMissing(error)) throw new Error('Cannot load application state. The existing state.json was not replaced.', { cause: error });
      disk = {
        version: 1, revision: 0, settings: { baseUrl: '', model: '', workspace: '', apiKey: '' }, conversations: [], routines: [], routineRuns: [], botExchanges: [], botDeliveries: [],
        bots: [{ id: randomUUID(), name: 'Grok', color: '#8b5cf6', instructions: '', createdAt: now().toISOString() }],
      };
    }
    const store = new Store(root, disk, now);
    for (const conversation of disk.conversations) {
      if (!activeStatus(conversation.run.status)) continue;
      conversation.run.status = 'interrupted';
      conversation.run.error = 'The server stopped before this run finished.';
      conversation.run.finishedAt = now().toISOString();
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
  get routines(): Routine[] { return this.disk.routines; }
  get routineRuns(): RoutineRun[] { return this.disk.routineRuns; }
  get botExchanges(): BotExchange[] { return this.disk.botExchanges; }
  get botDeliveries(): BotDelivery[] { return this.disk.botDeliveries; }
  get active(): Conversation | undefined { return this.conversations.find(c => activeStatus(c.run.status)); }

  snapshot(): AppState {
    const { apiKey, ...settings } = this.disk.settings;
    const state: AppState = {
      revision: this.disk.revision, bots: this.bots, conversations: this.conversations, routines: this.routines, routineRuns: this.routineRuns, botExchanges: this.botExchanges, botDeliveries: this.botDeliveries,
      settings: { ...settings, hasApiKey: Boolean(apiKey) }, runtime: this.runtime,
      ...(this.routineSchedulingError ? { routineSchedulingError: this.routineSchedulingError } : {}),
    };
    const publicLinkArrays = new Set<unknown>([...this.botExchanges.map(exchange => exchange.botIds),
      ...this.conversations.flatMap(conversation => conversation.messages.flatMap(message => message.botExchangeIds ? [message.botExchangeIds] : []))]);
    return JSON.parse(JSON.stringify(state, function (this: unknown, field, value: unknown) {
      if (publicLinkArrays.has(this)) return value;
      if (field === 'mcpApp' && apiKey && value && typeof value === 'object' && 'input' in value && typeof value.input === 'string') {
        return { ...value, input: publicAppInput(value.input, apiKey) };
      }
      if (['kind', 'status', 'role', 'color', 'hash'].includes(field)) return value;
      if (['id', 'botId', 'serverId', 'routineId', 'conversationId', 'executionRunId', 'sessionId', 'sessionFingerprint', 'exchangeId', 'sourceConversationId', 'sourceMessageId', 'recipientConversationId', 'fromBotId', 'toBotId', 'chainId', 'sourceBotId'].includes(field) && typeof value === 'string' && /^(?:[a-f\d]{8}-[a-f\d]{4}-[a-f\d]{4}-[a-f\d]{4}-[a-f\d]{12}|[a-f\d]{64})$/i.test(value)) return value;
      return typeof value === 'string' && apiKey ? value.split(apiKey).join('[redacted]') : value;
    })) as AppState;
  }

  subscribe(listener: (state: AppState) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  changed(): Promise<void> {
    for (const routineRun of this.routineRuns) {
      if (routineRun.result) continue;
      const execution = this.conversation(routineRun.conversationId).run;
      if (routineRun.executionRunId && routineRun.executionRunId !== execution.id) continue;
      if (execution.id) routineRun.executionRunId = execution.id;
      if (execution.status === 'completed' || execution.status === 'cancelled' || execution.status === 'error' || execution.status === 'interrupted') {
        routineRun.result = {
          status: execution.status,
          ...(execution.startedAt ? { startedAt: execution.startedAt } : {}),
          finishedAt: execution.finishedAt ?? this.now().toISOString(),
          ...(execution.error ? { error: execution.error } : {}),
        };
      }
    }
    reconcileBotDeliveries(this.botDeliveries, this.botExchanges, this.conversations, this.now());
    this.disk.revision += 1;
    const bytes = JSON.stringify(this.disk);
    const state = this.snapshot();
    const write = this.writeTail.then(async () => {
      if (this.writeError) throw this.writeError;
      try {
        if (Buffer.byteLength(bytes, 'utf8') > MAX_STATE_BYTES) throw new Error('Application state exceeds the 64 MiB limit.');
        await atomicPrivateFile(path.join(this.dataDir, 'state.json'), bytes);
      }
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

  async setRoutineSchedulingError(error?: string): Promise<void> {
    if (this.routineSchedulingError === error) return;
    this.routineSchedulingError = error;
    await this.changed();
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
    const workspace = input.workspace.trim() ? await canonicalDirectory(input.workspace.trim()) : '';
    if (workspace && (workspace === this.dataDir || workspace.startsWith(`${this.dataDir}${path.sep}`) || this.dataDir.startsWith(`${workspace}${path.sep}`) || workspace === path.parse(workspace).root)) {
      throw new HttpError(400, 'Workspace must not contain the application data directory or be the filesystem root.');
    }
    const sameProvider = this.settings.baseUrl && new URL(this.settings.baseUrl).origin === new URL(canonicalUrl).origin;
    const apiKey = input.clearApiKey ? '' : input.apiKey?.trim() || (sameProvider ? this.settings.apiKey : '');
    if (!apiKey && !loopback(new URL(canonicalUrl).hostname)) throw new HttpError(400, 'An API key is required for non-loopback providers.');
    this.disk.settings = { baseUrl: canonicalUrl, model: input.model.trim(), workspace, apiKey };
    for (const conversation of this.conversations) { delete conversation.sessionId; delete conversation.sessionFingerprint; }
    await this.changed();
  }

  async addBot(input: Pick<Bot, 'name' | 'color' | 'instructions'> & Partial<Pick<Bot, 'shape' | 'pinned'>>): Promise<Bot> {
    this.assertIdle();
    const bot = { ...input, id: randomUUID(), createdAt: new Date().toISOString() };
    this.bots.push(bot);
    await this.changed();
    return bot;
  }

  async updateBot(id: string, input: Partial<Pick<Bot, 'name' | 'color' | 'instructions' | 'shape' | 'pinned' | 'hidden'>>): Promise<void> {
    this.assertIdle();
    const bot = this.bot(id);
    const instructionsChanged = input.instructions !== undefined && input.instructions !== bot.instructions;
    Object.assign(bot, input);
    if (instructionsChanged) for (const conversation of this.conversations.filter(c => c.botId === id)) { delete conversation.sessionId; delete conversation.sessionFingerprint; }
    await this.changed();
  }

  skill(botId: string, id: string): Skill {
    const skill = this.bot(botId).skills?.find(item => item.id === id);
    if (!skill) throw new HttpError(404, 'Private skill does not exist for this bot.');
    return skill;
  }

  async addSkill(botId: string, input: SkillInput, withinRun = false): Promise<Skill> {
    if (withinRun) this.assertWritable(); else this.assertIdle();
    const bot = this.bot(botId);
    const skill = newSkill(input, this.now().toISOString());
    const skills = [...(bot.skills ?? []), skill];
    validateSkillSet(skills);
    this.assertSkillSize(botId, skills);
    bot.skills = skills;
    await this.changed();
    return skill;
  }

  async updateSkill(botId: string, id: string, input: z.infer<typeof skillPatchSchema>): Promise<void> {
    this.assertIdle();
    const bot = this.bot(botId);
    const next = nextSkill(this.skill(botId, id), input, this.now().toISOString());
    const skills = bot.skills!.map(skill => skill.id === id ? next : skill);
    validateSkillSet(skills);
    this.assertSkillSize(botId, skills);
    bot.skills = skills;
    await this.changed();
  }

  async deleteSkill(botId: string, id: string, revision: number): Promise<void> {
    this.assertIdle();
    if (this.skill(botId, id).revision !== revision) throw new HttpError(409, 'The skill changed since it was opened. Reload before deleting.');
    this.bot(botId).skills = this.bot(botId).skills!.filter(skill => skill.id !== id);
    await this.changed();
  }

  private assertSkillSize(botId: string, skills: readonly Skill[]): void {
    const bots = this.bots.map(bot => bot.id === botId ? { ...bot, skills: [...skills] } : bot);
    this.checkCapacity(this.routines, this.routineRuns, this.conversations, this.botExchanges, this.botDeliveries, bots);
  }

  routine(id: string): Routine {
    const routine = this.routines.find(item => item.id === id);
    if (!routine) throw new HttpError(404, 'Routine does not exist.');
    return routine;
  }

  private pendingRoutine(id: string): RoutineRun | undefined {
    return this.routineRuns.find(run => {
      if (run.routineId !== id || run.result) return false;
      const execution = this.conversation(run.conversationId).run;
      if (run.executionRunId && run.executionRunId !== execution.id) return false;
      const status = execution.status;
      return status === 'idle' || activeStatus(status);
    });
  }

  private assertRoutineIdle(id: string): void {
    this.assertWritable();
    this.routine(id);
    if (this.pendingRoutine(id)) throw new HttpError(409, 'Wait for or cancel this routine’s queued or active run first.');
  }

  private checkCapacity(
    routines = this.routines, routineRuns = this.routineRuns, conversations = this.conversations,
    botExchanges = this.botExchanges, botDeliveries = this.botDeliveries, bots = this.bots,
  ): void {
    if (routines.length > MAX_ROUTINES) throw new HttpError(409, 'The application supports at most 1000 routines.');
    if (routineRuns.length > MAX_ROUTINE_RUNS) throw new HttpError(409, 'Routine history is full. Existing history and conversations are retained.');
    if (botDeliveries.length > MAX_BOT_DELIVERIES) throw new HttpError(409, 'Bot delivery history is full. Existing exchanges and conversations are retained.');
    const queued = new Set(conversations.filter(conversation => conversation.run.status === 'idle').map(conversation => conversation.id));
    const dispatchBytes = routineRuns.reduce((bytes, run) => bytes + (
      !run.result && !run.executionRunId && queued.has(run.conversationId)
        ? Buffer.byteLength(JSON.stringify(run.prompt), 'utf8') + 4096
        : 0
    ), 0) + botDeliveries.reduce((bytes, delivery) => bytes + (
      delivery.status === 'queued' || delivery.status === 'running'
        ? (delivery.status === 'queued' ? Buffer.byteLength(JSON.stringify(delivery.text), 'utf8') : 0)
          + MAX_BOT_REPLY_CHARACTERS * 12 + 8192
        : 0
    ), 0);
    if (Buffer.byteLength(JSON.stringify({ ...this.disk, bots, routines, routineRuns, conversations, botExchanges, botDeliveries, revision: this.disk.revision + 1 }), 'utf8') + dispatchBytes > MAX_STATE_BYTES) {
      throw new HttpError(413, 'Application state and reserved queued work would exceed the 64 MiB limit. Existing history and conversations are retained.');
    }
  }

  private routineFields(input: RoutineInput, now: Date): RoutineInput & { nextRunAt?: string } {
    const parsed = routineInputSchema.parse(input);
    this.bot(parsed.botId);
    if (parsed.schedule.kind === 'once' && Date.parse(parsed.schedule.at) <= now.getTime()) throw new HttpError(400, 'A one-time schedule must be in the future.');
    return { ...parsed, ...(parsed.enabled ? { nextRunAt: nextRoutineTime(parsed.schedule, now) } : {}) };
  }

  async addRoutine(input: RoutineInput): Promise<Routine> {
    this.assertWritable();
    const now = this.now();
    const routine: Routine = { ...this.routineFields(input, now), id: randomUUID(), createdAt: now.toISOString(), updatedAt: now.toISOString() };
    this.checkCapacity([...this.routines, routine]);
    this.routines.push(routine);
    await this.changed();
    return routine;
  }

  async updateRoutine(id: string, input: RoutineInput): Promise<void> {
    this.assertRoutineIdle(id);
    const routine = this.routine(id);
    const now = this.now();
    const updated: Routine = { ...routine, ...this.routineFields(input, now), updatedAt: now.toISOString() };
    if (!updated.enabled) delete updated.nextRunAt;
    this.checkCapacity(this.routines.map(item => item.id === id ? updated : item));
    Object.assign(routine, updated);
    if (!updated.enabled) delete routine.nextRunAt;
    await this.changed();
  }

  async setRoutineEnabled(id: string, enabled: boolean): Promise<void> {
    this.assertWritable();
    const routine = this.routine(id);
    if (routine.enabled === enabled) return;
    const now = this.now();
    const updated: Routine = { ...routine, enabled, updatedAt: now.toISOString() };
    if (enabled) {
      if (routine.schedule.kind === 'once' && Date.parse(routine.schedule.at) <= now.getTime()) throw new HttpError(400, 'A one-time schedule must be in the future. Edit its time before enabling it.');
      updated.nextRunAt = nextRoutineTime(routine.schedule, now);
    } else delete updated.nextRunAt;
    this.checkCapacity(this.routines.map(item => item.id === id ? updated : item));
    Object.assign(routine, updated);
    if (!enabled) delete routine.nextRunAt;
    await this.changed();
  }

  async deleteRoutine(id: string): Promise<void> {
    this.assertRoutineIdle(id);
    this.disk.routines = this.routines.filter(item => item.id !== id);
    await this.changed();
  }

  private routineClaim(routine: Routine, trigger: RoutineRun['trigger'], scheduledFor: string, now: Date): { run: RoutineRun; conversation: Conversation } {
    const createdAt = now.toISOString();
    const conversation: Conversation = {
      id: randomUUID(), botId: routine.botId, title: routine.name, messages: [], run: { id: '', status: 'idle' }, updatedAt: createdAt,
    };
    const run: RoutineRun = {
      id: randomUUID(), routineId: routine.id, routineName: routine.name, botId: routine.botId, conversationId: conversation.id,
      prompt: routine.prompt, trigger, scheduledFor, createdAt,
    };
    return { run, conversation };
  }

  async runRoutine(id: string, now = this.now()): Promise<RoutineRun> {
    this.assertRoutineIdle(id);
    const routine = this.routine(id);
    const { run, conversation } = this.routineClaim(routine, 'manual', now.toISOString(), now);
    const updated = { ...routine };
    if (routine.schedule.kind === 'once') { updated.enabled = false; delete updated.nextRunAt; updated.updatedAt = now.toISOString(); }
    this.checkCapacity(this.routines.map(item => item.id === id ? updated : item), [...this.routineRuns, run], [...this.conversations, conversation]);
    Object.assign(routine, updated);
    if (routine.schedule.kind === 'once') delete routine.nextRunAt;
    this.routineRuns.push(run);
    this.conversations.push(conversation);
    await this.changed();
    return run;
  }

  async claimDueRoutines(now = this.now()): Promise<void> {
    this.assertWritable();
    const due = this.routines.filter(routine => routine.enabled && routine.nextRunAt && Date.parse(routine.nextRunAt) <= now.getTime())
      .sort((a, b) => Date.parse(a.nextRunAt!) - Date.parse(b.nextRunAt!));
    const updates = new Map<string, Routine>();
    const runs: RoutineRun[] = [];
    const conversations: Conversation[] = [];
    for (const routine of due) {
      const pending = this.pendingRoutine(routine.id);
      const previous = this.routineRuns.findLast(run => run.routineId === routine.id);
      const previousConversation = previous && this.conversation(previous.conversationId);
      const coveredByRun = previous && latestRoutineTime(routine.schedule, routine.nextRunAt!, now)
        <= Date.parse(previous.result?.finishedAt ?? previousConversation?.run.finishedAt ?? previous.createdAt);
      if (!pending && !coveredByRun) {
        const claim = this.routineClaim(routine, 'scheduled', routine.nextRunAt!, now);
        runs.push(claim.run);
        conversations.push(claim.conversation);
      }
      const updated = { ...routine, updatedAt: now.toISOString() };
      if (routine.schedule.kind === 'once') { updated.enabled = false; delete updated.nextRunAt; }
      else updated.nextRunAt = nextRoutineTime(routine.schedule, now, routine.nextRunAt);
      updates.set(routine.id, updated);
    }
    if (!updates.size) return;
    this.checkCapacity(this.routines.map(item => updates.get(item.id) ?? item), [...this.routineRuns, ...runs], [...this.conversations, ...conversations]);
    this.disk.routines = this.routines.map(item => updates.get(item.id) ?? item);
    this.routineRuns.push(...runs);
    this.conversations.push(...conversations);
    await this.changed();
  }

  async cancelQueuedRoutine(conversationId: string): Promise<boolean> {
    return this.cancelQueuedWork(conversationId);
  }

  async cancelQueuedWork(conversationId: string): Promise<boolean> {
    this.assertWritable();
    const conversation = this.conversation(conversationId);
    const queued = this.routineRuns.some(run => !run.result && !run.executionRunId && run.conversationId === conversationId)
      || this.botDeliveries.some(delivery => delivery.status === 'queued' && delivery.recipientConversationId === conversationId);
    if (conversation.run.status !== 'idle' || !queued) return false;
    const timestamp = this.now().toISOString();
    conversation.run = { ...conversation.run, status: 'cancelled', finishedAt: timestamp };
    conversation.updatedAt = timestamp;
    await this.changed();
    return true;
  }

  async failRoutineRun(run: RoutineRun, error: string, now = this.now()): Promise<void> {
    this.assertWritable();
    const conversation = this.conversation(run.conversationId);
    const key = this.settings.apiKey;
    const timestamp = now.toISOString();
    conversation.run = {
      id: conversation.run.id || run.id, status: 'error', error: (key ? error.split(key).join('[redacted]') : error).slice(0, 1000),
      startedAt: conversation.run.startedAt ?? timestamp, finishedAt: timestamp,
    };
    conversation.updatedAt = timestamp;
    await this.changed();
  }

  async queueBotMessage(conversationId: string, executionRunId: string, sourceMessageId: string, input: BotMessageInput): Promise<BotDelivery> {
    this.assertWritable();
    const parsed = botMessageInputSchema.parse(input);
    if (Buffer.byteLength(parsed.text, 'utf8') > MAX_BOT_MESSAGE_BYTES) throw new HttpError(413, 'Bot messages must be at most 160 KiB of UTF-8 text so the complete message can be reviewed.');
    const source = this.conversation(conversationId);
    const message = source.messages.findLast(item => item.role === 'assistant');
    if (source.run.id !== executionRunId || !activeStatus(source.run.status) || message?.id !== sourceMessageId) throw new HttpError(409, 'The sending run is no longer active.');
    const existing = existingBotDelivery(this.botDeliveries, sourceMessageId, parsed);
    if (existing) return existing;
    const recipient = this.bot(parsed.toBotId);
    if (recipient.id === source.botId) throw new HttpError(400, 'Choose another bot as the recipient.');
    const parent = this.botDeliveries.find(delivery => delivery.recipientConversationId === source.id);
    const hop = (parent?.hop ?? 0) + 1;
    if (hop > MAX_BOT_HOPS) throw new HttpError(409, 'Bot message chains are limited to four hops.');
    const timestamp = this.now().toISOString();
    const recipientConversation: Conversation = {
      id: randomUUID(), botId: recipient.id, title: `Message from ${this.bot(source.botId).name}`.slice(0, 100),
      messages: [], run: { id: '', status: 'idle' }, updatedAt: timestamp,
    };
    const exchange: BotExchange = {
      id: randomUUID(), botIds: [source.botId, recipient.id], sourceConversationId: source.id, sourceMessageId,
      messages: [{ id: randomUUID(), botId: source.botId, text: parsed.text, createdAt: timestamp }], updatedAt: timestamp,
    };
    const id = randomUUID();
    const delivery: BotDelivery = {
      id, exchangeId: exchange.id, sourceConversationId: source.id, sourceMessageId, recipientConversationId: recipientConversation.id,
      fromBotId: source.botId, toBotId: recipient.id, text: parsed.text, chainId: parent?.chainId ?? id, hop, createdAt: timestamp,
      status: 'queued', ...(parsed.requestId ? { requestId: parsed.requestId } : {}),
    };
    const conversations = this.conversations.map(conversation => conversation.id === source.id
      ? { ...source, messages: source.messages.map(item => item.id === sourceMessageId ? { ...item, botExchangeIds: [exchange.id] } : item) }
      : conversation);
    conversations.push(recipientConversation);
    this.checkCapacity(this.routines, this.routineRuns, conversations, [...this.botExchanges, exchange], [...this.botDeliveries, delivery]);
    message.botExchangeIds = [exchange.id];
    this.conversations.push(recipientConversation);
    this.botExchanges.push(exchange);
    this.botDeliveries.push(delivery);
    await this.changed();
    return delivery;
  }

  async failBotDelivery(delivery: BotDelivery, error: string, now = this.now()): Promise<void> {
    this.assertWritable();
    const conversation = this.conversation(delivery.recipientConversationId);
    const key = this.settings.apiKey;
    const timestamp = now.toISOString();
    conversation.run = {
      id: conversation.run.id, status: 'error', error: (key ? error.split(key).join('[redacted]') : error).slice(0, 1000), finishedAt: timestamp,
    };
    conversation.updatedAt = timestamp;
    await this.changed();
  }

  isReservedConversation(id: string): boolean {
    return this.botDeliveries.some(delivery => delivery.recipientConversationId === id)
      || this.routineRuns.some(run => run.conversationId === id && !run.result && !run.executionRunId);
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
