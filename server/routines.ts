import { CronDate, CronExpressionParser } from 'cron-parser';
import { z } from 'zod';
import type { RoutineInput, RoutineRun, RoutineSchedule } from '../shared/types.js';
import { HttpError } from './files.js';
import type { Store } from './store.js';
import type { GrokRuntime } from './runtime.js';

const timestamp = z.iso.datetime({ offset: true });
const timezone = z.string().trim().min(1).max(100).refine(value => {
  if (/^[+-]/.test(value)) return false;
  try { new Intl.DateTimeFormat('en', { timeZone: value }).format(); return true; }
  catch { return false; }
}, 'Choose a valid IANA timezone.');
export const routineScheduleSchema = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('once'), at: timestamp }).strict(),
  z.object({ kind: z.literal('interval'), minutes: z.number().int().min(1).max(10080) }).strict(),
  z.object({ kind: z.literal('daily'), time: z.string().regex(/^(?:[01]\d|2[0-3]):[0-5]\d$/, 'Use HH:mm in 24-hour time.'), timezone }).strict(),
]) satisfies z.ZodType<RoutineSchedule>;
export const routineInputSchema = z.object({
  botId: z.string().uuid(), name: z.string().trim().min(1).max(100), prompt: z.string().trim().min(1).max(100000),
  schedule: routineScheduleSchema, enabled: z.boolean(),
}).strict() satisfies z.ZodType<RoutineInput>;
export const routineSchema = routineInputSchema.extend({
  id: z.string().uuid(), nextRunAt: timestamp.optional(), createdAt: timestamp, updatedAt: timestamp,
});
export const routineRunSchema = z.object({
  id: z.string().uuid(), routineId: z.string().uuid(), routineName: z.string().min(1).max(100), botId: z.string().uuid(),
  conversationId: z.string().uuid(), prompt: z.string().min(1).max(100000), trigger: z.enum(['manual', 'scheduled']),
  scheduledFor: timestamp, createdAt: timestamp, executionRunId: z.string().optional(),
  result: z.object({
    status: z.enum(['completed', 'cancelled', 'error', 'interrupted']), startedAt: timestamp.optional(), finishedAt: timestamp, error: z.string().optional(),
  }).strict().optional(),
}).strict() satisfies z.ZodType<RoutineRun>;

function dailyExpression(schedule: Extract<RoutineSchedule, { kind: 'daily' }>, currentDate: Date) {
  const [hour, minute] = schedule.time.split(':').map(Number);
  return CronExpressionParser.parse(`0 ${minute} ${hour} * * *`, { currentDate, tz: schedule.timezone });
}

function dailyTimes(schedule: Extract<RoutineSchedule, { kind: 'daily' }>, now: Date): { next: string; latest: number } {
  const day = new CronDate(now, schedule.timezone);
  day.setStartOfDay();
  day.subtractDay();
  day.setStartOfDay();
  day.subtractSecond();
  // Include yesterday and today's offset transition without revisiting a repeated hour.
  const expression = dailyExpression(schedule, day.toDate());
  let latest = Number.NEGATIVE_INFINITY;
  let occurrence = expression.next();
  while (occurrence.getTime() <= now.getTime()) {
    latest = occurrence.getTime();
    occurrence = expression.next();
  }
  return { next: occurrence.toDate().toISOString(), latest };
}

export function nextRoutineTime(schedule: RoutineSchedule, now: Date, previous?: string): string {
  if (schedule.kind === 'once') return new Date(schedule.at).toISOString();
  if (schedule.kind === 'daily') return dailyTimes(schedule, now).next;
  const period = schedule.minutes * 60000;
  const anchor = previous ? Date.parse(previous) : now.getTime();
  return new Date(anchor + Math.max(1, Math.floor((now.getTime() - anchor) / period) + 1) * period).toISOString();
}

export function latestRoutineTime(schedule: RoutineSchedule, due: string, now: Date): number {
  if (schedule.kind === 'once') return Date.parse(due);
  if (schedule.kind === 'daily') return Math.max(Date.parse(due), dailyTimes(schedule, now).latest);
  const period = schedule.minutes * 60000;
  return Date.parse(due) + Math.floor((now.getTime() - Date.parse(due)) / period) * period;
}

export interface RoutineSchedulerOptions {
  now?: () => Date;
  pollIntervalMs?: number;
  autoStart?: boolean;
}
export type EnqueueCommand = <T>(operation: () => Promise<T>) => Promise<T>;

export class RoutineScheduler {
  private timer?: NodeJS.Timeout;
  private inFlight?: Promise<void>;
  private closed = false;
  private failure?: Error;
  private readonly now: () => Date;

  constructor(
    private readonly store: Store,
    private readonly runtime: Pick<GrokRuntime, 'start'>,
    private readonly enqueue: EnqueueCommand,
    private readonly options: RoutineSchedulerOptions = {},
  ) {
    this.now = options.now ?? (() => new Date());
    if (!Number.isInteger(options.pollIntervalMs ?? 1000) || (options.pollIntervalMs ?? 1000) < 1 || (options.pollIntervalMs ?? 1000) > 2147483647) {
      throw new Error('Routine polling interval must be a positive timer interval.');
    }
  }

  get error(): Error | undefined { return this.failure; }

  start(): void {
    if (this.closed || this.failure || this.timer || this.options.autoStart === false) return;
    const poll = () => { void this.tick().catch(() => { console.error('Routine scheduler stopped. Check application state before restarting.'); }); };
    this.timer = setInterval(poll, this.options.pollIntervalMs ?? 1000);
    this.timer.unref();
    poll();
  }

  tick(): Promise<void> {
    if (this.failure) return Promise.reject(this.failure);
    if (this.closed) return Promise.resolve();
    if (this.inFlight) return this.inFlight;
    this.inFlight = this.enqueue(async () => {
      if (!this.closed) await this.process(this.now());
    }).then(() => { this.inFlight = undefined; }, error => { this.inFlight = undefined; this.halt(error); throw error; });
    return this.inFlight;
  }

  run(id: string): Promise<RoutineRun> {
    const observedRunId = this.store.routineRuns.findLast(run => run.routineId === id)?.id;
    return this.enqueue(async () => {
      if (this.closed) throw new HttpError(503, 'The server is shutting down.');
      if (this.failure) throw new HttpError(503, 'Routine scheduling stopped. Restart after checking the application data directory.');
      if (this.store.routineRuns.findLast(run => run.routineId === id)?.id !== observedRunId) {
        throw new HttpError(409, 'This routine was just claimed by another command.');
      }
      const now = this.now();
      let run: RoutineRun;
      try { run = await this.store.runRoutine(id, now); }
      catch (error) {
        try { this.store.assertWritable(); }
        catch { this.halt(error); }
        throw error;
      }
      try { await this.process(now); }
      catch (error) { this.halt(error); throw error; }
      return run;
    });
  }

  private async process(now: Date): Promise<void> {
    this.store.assertWritable();
    let schedulingError: string | undefined;
    try { await this.store.claimDueRoutines(now); }
    catch (error) {
      if (!(error instanceof HttpError) || (error.status !== 409 && error.status !== 413)) throw error;
      schedulingError = error.message;
    }
    await this.store.setRoutineSchedulingError(schedulingError);
    if (this.closed || this.store.active) return;
    const run = this.store.routineRuns.filter(item => !item.result && !item.executionRunId && this.store.conversation(item.conversationId).run.status === 'idle')
      .sort((a, b) => Date.parse(a.createdAt) - Date.parse(b.createdAt))[0];
    if (!run) return;
    try { await this.runtime.start(run.conversationId, run.prompt); }
    catch (error) {
      this.store.assertWritable();
      await this.store.failRoutineRun(run, error instanceof HttpError ? error.message : 'Routine could not start. Check runtime and provider settings.', now);
    }
  }

  private halt(error: unknown): void {
    this.failure = error instanceof Error ? error : new Error('Routine scheduling failed.');
    clearInterval(this.timer);
    this.timer = undefined;
  }

  async close(): Promise<void> {
    this.closed = true;
    clearInterval(this.timer);
    this.timer = undefined;
    await this.inFlight?.then(() => undefined, error => { this.halt(error); });
  }
}
