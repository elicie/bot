import assert from 'node:assert/strict';
import test from 'node:test';
import { randomUUID } from 'node:crypto';
import { mkdtemp, mkdir, readFile, rename, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { RoutineInput, RoutineSchedule } from '../shared/types.js';
import { HttpError } from '../server/files.js';
import { RoutineScheduler, latestRoutineTime, nextRoutineTime, routineInputSchema, type EnqueueCommand } from '../server/routines.js';
import { Store } from '../server/store.js';

function commandQueue(): EnqueueCommand {
  let tail: Promise<unknown> = Promise.resolve();
  return operation => {
    const result = tail.then(operation);
    tail = result.catch(() => {});
    return result;
  };
}

async function fixture() {
  const root = await mkdtemp(join(tmpdir(), 'grok-routines-'));
  const dataDir = join(root, 'data');
  const workspace = join(root, 'workspace');
  await mkdir(workspace);
  let current = new Date('2026-01-01T00:00:00.000Z');
  const now = () => new Date(current);
  const store = await Store.open(dataDir, { now });
  const starts: Array<{ id: string; prompt: string }> = [];
  const runtime = {
    async start(id: string, prompt: string) {
      store.assertIdle();
      const persisted = JSON.parse(await readFile(join(dataDir, 'state.json'), 'utf8'));
      assert.ok(persisted.routineRuns.some((run: { conversationId: string }) => run.conversationId === id));
      assert.equal(persisted.conversations.find((conversation: { id: string }) => conversation.id === id).run.status, 'idle');
      starts.push({ id, prompt });
      store.conversation(id).run = { id: randomUUID(), status: 'running', startedAt: now().toISOString() };
      await store.changed();
    },
  };
  const scheduler = new RoutineScheduler(store, runtime, commandQueue(), { now, autoStart: false });
  const input = (schedule: RoutineSchedule, extra: Partial<RoutineInput> = {}): RoutineInput => ({
    botId: store.bots[0].id, name: 'Routine', prompt: 'Routine prompt', schedule, enabled: true, ...extra,
  });
  const finish = async (id: string) => {
    const conversation = store.conversation(id);
    conversation.run.status = 'completed';
    conversation.run.finishedAt = now().toISOString();
    await store.changed();
  };
  return {
    root, dataDir, workspace, store, scheduler, starts, now, runtime, input, finish,
    setTime: (value: string) => { current = new Date(value); },
    close: async () => { await scheduler.close(); await rm(root, { recursive: true, force: true }); },
  };
}

const conflict = (error: unknown) => error instanceof HttpError && error.status === 409;

test('version-1 migration retains bots, settings, secrets, messages, and session attachments', async () => {
  const f = await fixture();
  try {
    await f.store.updateSettings({ baseUrl: 'http://127.0.0.1:12345/v1', model: 'saved-model', workspace: f.workspace, apiKey: 'legacy-private-key' });
    const bot = await f.store.addBot({ name: 'Saved bot', color: '#123456', instructions: 'Saved instructions' });
    const conversation = await f.store.addConversation(bot.id);
    conversation.sessionId = 'saved-session';
    conversation.sessionFingerprint = 'a'.repeat(64);
    conversation.messages.push({ id: randomUUID(), role: 'user', text: 'Saved message', createdAt: f.now().toISOString(), tools: [] });
    conversation.run = { id: 'saved-run', status: 'completed' };
    await f.store.changed();
    const before = JSON.parse(await readFile(join(f.dataDir, 'state.json'), 'utf8'));
    delete before.routines;
    delete before.routineRuns;
    await writeFile(join(f.dataDir, 'state.json'), JSON.stringify(before));
    const reopened = await Store.open(f.dataDir, { now: f.now });
    const after = JSON.parse(await readFile(join(f.dataDir, 'state.json'), 'utf8'));
    assert.deepEqual(after.bots, before.bots);
    assert.deepEqual(after.settings, before.settings);
    assert.deepEqual(after.conversations, before.conversations);
    assert.deepEqual(after.routines, []);
    assert.deepEqual(after.routineRuns, []);
    assert.ok(after.revision > before.revision);
    assert.equal(JSON.stringify(reopened.snapshot()).includes('legacy-private-key'), false);
  } finally { await f.close(); }
});

test('routine disk references, unique IDs, and single pending claims are validated without replacing corrupt state', async () => {
  const f = await fixture();
  try {
    const secondBot = await f.store.addBot({ name: 'Other bot', color: '#654321', instructions: '' });
    const routine = await f.store.addRoutine(f.input({ kind: 'interval', minutes: 1 }));
    const run = await f.store.runRoutine(routine.id);
    const good = JSON.parse(await readFile(join(f.dataDir, 'state.json'), 'utf8'));
    const invalid: Array<(state: typeof good) => void> = [
      state => { state.routines.push({ ...state.routines[0] }); },
      state => { state.routines[0].id = state.bots[0].id; },
      state => { state.routineRuns[0].id = state.conversations[0].id; },
      state => { state.routines[0].botId = randomUUID(); },
      state => { state.routineRuns[0].botId = secondBot.id; },
      state => { state.routineRuns[0].conversationId = randomUUID(); },
      state => { state.routineRuns[0].routineId = state.bots[0].id; },
      state => { state.routineRuns[0].executionRunId = randomUUID(); },
      state => { state.routineRuns.push({ ...state.routineRuns[0], id: randomUUID() }); },
      state => { state.routines = []; },
      state => { delete state.routines[0].nextRunAt; },
      state => {
        const conversationId = randomUUID();
        state.conversations.push({ ...state.conversations[0], id: conversationId });
        state.routineRuns.push({ ...state.routineRuns[0], id: randomUUID(), conversationId });
      },
    ];
    for (const corrupt of invalid) {
      const state = structuredClone(good);
      corrupt(state);
      const bytes = JSON.stringify(state);
      await writeFile(join(f.dataDir, 'state.json'), bytes);
      await assert.rejects(Store.open(f.dataDir, { now: f.now }), /existing state.json was not replaced/);
      assert.equal(await readFile(join(f.dataDir, 'state.json'), 'utf8'), bytes);
    }
    assert.equal(run.routineId, routine.id);
  } finally { await f.close(); }
});

test('schedule validation enforces intervals, explicit real timezones, nonempty fields, and future one-time creation and edits', async () => {
  const f = await fixture();
  try {
    const badSchedules = [
      { kind: 'interval', minutes: 0 }, { kind: 'interval', minutes: 10081 }, { kind: 'interval', minutes: 1.1 },
      { kind: 'daily', time: '24:00', timezone: 'UTC' }, { kind: 'daily', time: '9:00', timezone: 'UTC' },
      { kind: 'daily', time: '09:00', timezone: 'Mars/Olympus' }, { kind: 'daily', time: '09:00', timezone: '+04:00' },
      { kind: 'daily', time: '09:00' }, { kind: 'once', at: 'tomorrow' }, { kind: 'once', at: '2026-01-02T12:00:00' },
    ];
    for (const schedule of badSchedules) assert.equal(routineInputSchema.safeParse({ ...f.input({ kind: 'interval', minutes: 1 }), schedule }).success, false);
    for (const field of ['name', 'prompt']) assert.equal(routineInputSchema.safeParse({ ...f.input({ kind: 'interval', minutes: 1 }), [field]: '  ' }).success, false);
    await assert.rejects(f.store.addRoutine(f.input({ kind: 'once', at: f.now().toISOString() })), error => error instanceof HttpError && error.status === 400);
    await assert.rejects(f.store.addRoutine(f.input({ kind: 'interval', minutes: 1 }, { botId: randomUUID() })), error => error instanceof HttpError && error.status === 404);
    const routine = await f.store.addRoutine(f.input({ kind: 'interval', minutes: 10080 }));
    await assert.rejects(f.store.updateRoutine(routine.id, f.input({ kind: 'once', at: '2025-01-01T00:00:00Z' }, { enabled: false })), error => error instanceof HttpError && error.status === 400);
    assert.equal(f.store.routine(routine.id).schedule.kind, 'interval');
  } finally { await f.close(); }
});

test('one-time deadline claims persist before start, disable the schedule, and overlapping ticks start only once', async () => {
  const f = await fixture();
  try {
    const routine = await f.store.addRoutine(f.input({ kind: 'once', at: '2026-01-01T00:01:00Z' }));
    f.setTime('2026-01-01T00:00:59.999Z');
    await f.scheduler.tick();
    assert.equal(f.starts.length, 0);
    f.setTime('2026-01-01T00:01:00Z');
    await Promise.all(Array.from({ length: 20 }, () => f.scheduler.tick()));
    assert.equal(f.starts.length, 1);
    assert.equal(f.starts[0].prompt, routine.prompt);
    const snapshot = f.store.snapshot();
    assert.equal(snapshot.routineRuns.length, 1);
    assert.equal(snapshot.routineRuns[0].trigger, 'scheduled');
    assert.equal(snapshot.routineRuns[0].scheduledFor, '2026-01-01T00:01:00.000Z');
    assert.equal(snapshot.conversations[0].run.status, 'running');
    assert.equal(snapshot.routines[0].enabled, false);
    assert.equal(snapshot.routines[0].nextRunAt, undefined);
    await f.finish(f.starts[0].id);
    f.setTime('2026-01-02T00:00:00Z');
    await f.scheduler.tick();
    assert.equal(f.starts.length, 1);
  } finally { await f.close(); }
});

test('missed interval slots coalesce and retain their phase while pending, active, and finishing between polls', async () => {
  const f = await fixture();
  try {
    const routine = await f.store.addRoutine(f.input({ kind: 'interval', minutes: 1 }));
    f.setTime('2026-01-01T00:20:30Z');
    await f.scheduler.tick();
    assert.equal(f.store.routineRuns.length, 1);
    assert.equal(f.store.routineRuns[0].scheduledFor, '2026-01-01T00:01:00.000Z');
    assert.equal(f.store.routine(routine.id).nextRunAt, '2026-01-01T00:21:00.000Z');
    f.setTime('2026-01-01T00:40:30Z');
    await f.scheduler.tick();
    assert.equal(f.store.routine(routine.id).nextRunAt, '2026-01-01T00:41:00.000Z');
    assert.equal(f.starts.length, 1);
    f.setTime('2026-01-01T01:00:30Z');
    await f.finish(f.starts[0].id);
    await f.scheduler.tick();
    assert.equal(f.starts.length, 1);
    assert.equal(f.store.routine(routine.id).nextRunAt, '2026-01-01T01:01:00.000Z');
    f.setTime('2026-01-01T01:01:00Z');
    await f.scheduler.tick();
    assert.equal(f.starts.length, 2);
    assert.equal(f.store.routineRuns.length, 2);
  } finally { await f.close(); }
});

test('daily IANA schedules handle spring-forward gaps and fall-back repeated hours without two daily runs', () => {
  const spring: RoutineSchedule = { kind: 'daily', time: '02:30', timezone: 'America/New_York' };
  const fall: RoutineSchedule = { kind: 'daily', time: '01:30', timezone: 'America/New_York' };
  assert.equal(nextRoutineTime(spring, new Date('2026-03-08T06:59:00Z')), '2026-03-08T07:30:00.000Z');
  assert.equal(nextRoutineTime(spring, new Date('2026-03-08T07:30:00Z')), '2026-03-09T06:30:00.000Z');
  assert.equal(nextRoutineTime(fall, new Date('2026-11-01T04:00:00Z')), '2026-11-01T05:30:00.000Z');
  assert.equal(nextRoutineTime(fall, new Date('2026-11-01T05:30:00Z')), '2026-11-02T06:30:00.000Z');
  assert.equal(nextRoutineTime(fall, new Date('2026-11-01T06:15:00Z')), '2026-11-02T06:30:00.000Z');
  assert.equal(nextRoutineTime({ kind: 'daily', time: '09:15', timezone: 'Asia/Kathmandu' }, new Date('2026-01-01T00:00:00Z')), '2026-01-01T03:30:00.000Z');
  assert.equal(latestRoutineTime(spring, '2026-03-08T07:30:00Z', new Date('2026-03-08T07:30:00Z')), Date.parse('2026-03-08T07:30:00Z'));
});

test('daily scheduler ticks fire through spring gaps and only once across repeated fall hours', async () => {
  const cases = [
    { start: '2026-03-08T05:00:00Z', time: '02:30', before: '2026-03-08T07:29:59Z', due: '2026-03-08T07:30:00Z', later: '2026-03-08T08:00:00Z', next: '2026-03-09T06:30:00.000Z' },
    { start: '2026-11-01T04:00:00Z', time: '01:30', before: '2026-11-01T05:29:59Z', due: '2026-11-01T05:30:00Z', later: '2026-11-01T06:30:00Z', next: '2026-11-02T06:30:00.000Z' },
  ];
  for (const scenario of cases) {
    const f = await fixture();
    try {
      f.setTime(scenario.start);
      const routine = await f.store.addRoutine(f.input({ kind: 'daily', time: scenario.time, timezone: 'America/New_York' }));
      assert.equal(f.store.routine(routine.id).nextRunAt, new Date(scenario.due).toISOString());
      f.setTime(scenario.before);
      await f.scheduler.tick();
      assert.equal(f.starts.length, 0);
      f.setTime(scenario.due);
      await f.scheduler.tick();
      assert.equal(f.starts.length, 1);
      assert.equal(f.store.conversation(f.starts[0].id).run.status, 'running');
      await f.finish(f.starts[0].id);
      f.setTime(scenario.later);
      await f.scheduler.tick();
      assert.equal(f.starts.length, 1);
      assert.equal(f.store.routine(routine.id).nextRunAt, scenario.next);
    } finally { await f.close(); }
  }
});

test('manual runs queue behind global contention, preserve recurring deadlines, and older claims dispatch first', async () => {
  const f = await fixture();
  try {
    const blocker = await f.store.addConversation(f.store.bots[0].id);
    blocker.run = { id: randomUUID(), status: 'running', startedAt: f.now().toISOString() };
    await f.store.changed();
    const first = await f.store.addRoutine(f.input({ kind: 'interval', minutes: 5 }, { name: 'First' }));
    const second = await f.store.addRoutine(f.input({ kind: 'interval', minutes: 5 }, { name: 'Second' }));
    const deadline = f.store.routine(first.id).nextRunAt;
    const firstRun = await f.scheduler.run(first.id);
    f.setTime('2026-01-01T00:00:01Z');
    const secondRun = await f.scheduler.run(second.id);
    assert.equal(f.starts.length, 0);
    assert.equal(f.store.conversation(firstRun.conversationId).run.status, 'idle');
    assert.equal(f.store.routine(first.id).nextRunAt, deadline);
    await assert.rejects(f.scheduler.run(first.id), conflict);
    await f.store.setRoutineEnabled(first.id, false);
    f.setTime('2026-01-01T00:20:00Z');
    await f.scheduler.tick();
    assert.equal(f.store.routineRuns.length, 2);
    assert.equal(f.store.routine(second.id).nextRunAt, '2026-01-01T00:25:00.000Z');
    await f.finish(blocker.id);
    await f.scheduler.tick();
    assert.equal(f.starts.length, 1);
    assert.equal(f.starts[0].id, firstRun.conversationId);
    await f.finish(firstRun.conversationId);
    await f.scheduler.tick();
    assert.equal(f.starts.length, 2);
    assert.equal(f.starts[1].id, secondRun.conversationId);
  } finally { await f.close(); }
});

test('one tick dispatches at most one queued execution even when the runtime finishes synchronously', async () => {
  const f = await fixture();
  try {
    const first = await f.store.addRoutine(f.input({ kind: 'once', at: '2026-01-01T00:01:00Z' }));
    const second = await f.store.addRoutine(f.input({ kind: 'once', at: '2026-01-01T00:02:00Z' }));
    let starts = 0;
    const scheduler = new RoutineScheduler(f.store, { start: async id => { starts++; await f.finish(id); } }, commandQueue(), { now: f.now });
    try {
      f.setTime('2026-01-01T00:10:00Z');
      await scheduler.tick();
      assert.equal(starts, 1);
      assert.equal(f.store.routineRuns[0].routineId, first.id);
      assert.equal(f.store.routineRuns[1].routineId, second.id);
      assert.equal(f.store.conversation(f.store.routineRuns[1].conversationId).run.status, 'idle');
      await scheduler.tick();
      assert.equal(starts, 2);
    } finally { await scheduler.close(); }
  } finally { await f.close(); }
});

test('manual and scheduled one-time races produce one pending run in either command order', async () => {
  for (const manualFirst of [true, false]) {
    const f = await fixture();
    try {
      const routine = await f.store.addRoutine(f.input({ kind: 'once', at: '2026-01-01T00:01:00Z' }));
      f.setTime('2026-01-01T00:01:00Z');
      if (manualFirst) {
        await Promise.all([f.scheduler.run(routine.id), f.scheduler.tick()]);
        assert.equal(f.store.routineRuns[0].trigger, 'manual');
      } else {
        const tick = f.scheduler.tick();
        await assert.rejects(f.scheduler.run(routine.id), conflict);
        await tick;
        assert.equal(f.store.routineRuns[0].trigger, 'scheduled');
      }
      assert.equal(f.starts.length, 1);
      assert.equal(f.store.routineRuns.length, 1);
      assert.equal(f.store.routine(routine.id).enabled, false);
      assert.equal(f.store.routine(routine.id).nextRunAt, undefined);
    } finally { await f.close(); }
  }
});

test('concurrent manual and one-time scheduled commands cannot double-start even with immediate completion', async () => {
  for (const manualFirst of [true, false]) {
    const f = await fixture();
    try {
      const routine = await f.store.addRoutine(f.input({ kind: 'once', at: '2026-01-01T00:01:00Z' }));
      let starts = 0;
      const scheduler = new RoutineScheduler(f.store, { start: async id => { starts++; await f.finish(id); } }, commandQueue(), { now: f.now });
      try {
        f.setTime('2026-01-01T00:01:00Z');
        if (manualFirst) await Promise.all([scheduler.run(routine.id), scheduler.tick()]);
        else {
          const tick = scheduler.tick();
          await assert.rejects(scheduler.run(routine.id), conflict);
          await tick;
        }
        assert.equal(starts, 1);
        assert.equal(f.store.routineRuns.length, 1);
      } finally { await scheduler.close(); }
    } finally { await f.close(); }
  }
});

test('manual runs at a recurring deadline coalesce the due slot without shifting the recurring phase', async () => {
  const f = await fixture();
  try {
    const routine = await f.store.addRoutine(f.input({ kind: 'interval', minutes: 5 }));
    f.setTime('2026-01-01T00:05:30Z');
    const run = await f.scheduler.run(routine.id);
    assert.equal(f.store.routineRuns.length, 1);
    assert.equal(run.trigger, 'manual');
    assert.equal(f.store.routine(routine.id).nextRunAt, '2026-01-01T00:10:00.000Z');
    await f.finish(run.conversationId);
    await f.scheduler.tick();
    assert.equal(f.starts.length, 1);
  } finally { await f.close(); }
});

test('pause only blocks future claims, resume uses a fresh future deadline, and expired one-time schedules cannot resume', async () => {
  const f = await fixture();
  try {
    const routine = await f.store.addRoutine(f.input({ kind: 'interval', minutes: 5 }));
    await f.store.setRoutineEnabled(routine.id, false);
    f.setTime('2026-01-01T01:00:00Z');
    await f.scheduler.tick();
    assert.equal(f.starts.length, 0);
    await f.store.setRoutineEnabled(routine.id, true);
    assert.equal(f.store.routine(routine.id).nextRunAt, '2026-01-01T01:05:00.000Z');
    await f.scheduler.tick();
    assert.equal(f.starts.length, 0);
    const once = await f.store.addRoutine(f.input({ kind: 'once', at: '2026-01-01T01:01:00Z' }, { enabled: false }));
    f.setTime('2026-01-01T01:05:00Z');
    await assert.rejects(f.store.setRoutineEnabled(once.id, true), error => error instanceof HttpError && error.status === 400);
    await f.scheduler.tick();
    await f.store.setRoutineEnabled(routine.id, false);
    assert.equal(f.store.conversation(f.starts[0].id).run.status, 'running');
  } finally { await f.close(); }
});

test('unrelated routine edits remain usable during execution while own edits, deletion, and bot mutations are guarded', async () => {
  const f = await fixture();
  try {
    const first = await f.store.addRoutine(f.input({ kind: 'interval', minutes: 5 }));
    const run = await f.scheduler.run(first.id);
    const unrelated = await f.store.addRoutine(f.input({ kind: 'interval', minutes: 2 }));
    await f.store.updateRoutine(unrelated.id, f.input({ kind: 'daily', time: '09:00', timezone: 'UTC' }, { name: 'Edited unrelated' }));
    assert.equal(f.store.routine(unrelated.id).name, 'Edited unrelated');
    await assert.rejects(f.store.updateRoutine(first.id, f.input({ kind: 'interval', minutes: 7 })), conflict);
    await assert.rejects(f.store.deleteRoutine(first.id), conflict);
    await assert.rejects(f.store.updateBot(first.botId, { name: 'Blocked' }), conflict);
    await f.finish(run.conversationId);
    await f.store.deleteRoutine(first.id);
    assert.equal(f.store.routineRuns.length, 1);
    assert.equal(f.store.conversations.length, 1);
    const reopened = await Store.open(f.dataDir, { now: f.now });
    assert.equal(reopened.routines.some(item => item.id === first.id), false);
    assert.equal(reopened.routineRuns[0].routineName, first.name);
    assert.equal(reopened.conversation(run.conversationId).run.status, 'completed');
  } finally { await f.close(); }
});

test('startup failures create visible terminal errors and timestamps without messages, leaked secrets, or retries', async () => {
  const f = await fixture();
  try {
    const secret = 'private-fixture-provider-key';
    await f.store.updateSettings({ baseUrl: 'http://127.0.0.1:12345/v1', model: 'fixture', workspace: f.workspace, apiKey: secret });
    const routine = await f.store.addRoutine(f.input({ kind: 'once', at: '2026-01-01T00:01:00Z' }, { name: `Name ${secret}`, prompt: `Prompt ${secret}` }));
    let starts = 0;
    const scheduler = new RoutineScheduler(f.store, { start: async () => { starts++; throw new HttpError(400, `Invalid provider ${secret}`); } }, commandQueue(), { now: f.now });
    try {
      f.setTime('2026-01-01T00:01:00Z');
      await scheduler.tick();
      await scheduler.tick();
      assert.equal(starts, 1);
      const run = f.store.routineRuns[0];
      const conversation = f.store.conversation(run.conversationId);
      assert.equal(conversation.run.status, 'error');
      assert.equal(conversation.run.startedAt, f.now().toISOString());
      assert.equal(conversation.run.finishedAt, f.now().toISOString());
      assert.equal(conversation.messages.length, 0);
      assert.equal(conversation.run.error, 'Invalid provider [redacted]');
      assert.equal(JSON.stringify(f.store.snapshot()).includes(secret), false);
      assert.equal(f.store.routine(routine.id).enabled, false);
      const persisted = JSON.parse(await readFile(join(f.dataDir, 'state.json'), 'utf8'));
      assert.equal(persisted.settings.apiKey, secret);
      assert.equal(persisted.conversations[0].run.error.includes(secret), false);
      await f.store.deleteRoutine(routine.id);
    } finally { await scheduler.close(); }
  } finally { await f.close(); }
});

test('public routine and conversation link UUIDs survive secret substring redaction', async () => {
  const f = await fixture();
  try {
    const routine = await f.store.addRoutine(f.input({ kind: 'interval', minutes: 5 }));
    const key = routine.id.slice(0, 8);
    await f.store.updateSettings({ baseUrl: 'http://127.0.0.1:12345/v1', model: 'fixture', workspace: f.workspace, apiKey: key });
    const run = await f.store.runRoutine(routine.id);
    const executionRunId = `${key}-1234-4123-8123-123456789abc`;
    f.store.conversation(run.conversationId).run = { id: executionRunId, status: 'running', startedAt: f.now().toISOString() };
    await f.store.changed();
    const state = f.store.snapshot();
    assert.equal(state.routines[0].id, routine.id);
    assert.equal(state.routineRuns[0].routineId, routine.id);
    assert.equal(state.routineRuns[0].conversationId, run.conversationId);
    assert.equal(state.routineRuns[0].executionRunId, executionRunId);
  } finally { await f.close(); }
});

test('queued claims survive restart while already-started claims become interrupted and never replay', async () => {
  const f = await fixture();
  try {
    const first = await f.store.addRoutine(f.input({ kind: 'once', at: '2026-01-01T00:01:00Z' }));
    const second = await f.store.addRoutine(f.input({ kind: 'once', at: '2026-01-01T00:02:00Z' }));
    const active = await f.store.runRoutine(first.id);
    const queued = await f.store.runRoutine(second.id);
    f.store.conversation(active.conversationId).run = { id: randomUUID(), status: 'awaiting_permission', startedAt: f.now().toISOString() };
    await f.store.changed();
    await f.scheduler.close();
    const reopened = await Store.open(f.dataDir, { now: f.now });
    assert.equal(reopened.conversation(active.conversationId).run.status, 'interrupted');
    assert.equal(reopened.conversation(queued.conversationId).run.status, 'idle');
    const starts: string[] = [];
    const scheduler = new RoutineScheduler(reopened, { start: async id => {
      starts.push(id);
      reopened.conversation(id).run = { id: randomUUID(), status: 'running', startedAt: f.now().toISOString() };
      await reopened.changed();
    } }, commandQueue(), { now: f.now });
    try {
      f.setTime('2026-01-02T00:00:00Z');
      await scheduler.tick();
      await scheduler.tick();
      assert.deepEqual(starts, [queued.conversationId]);
      assert.equal(reopened.routineRuns.length, 2);
      assert.equal(reopened.conversation(active.conversationId).run.status, 'interrupted');
    } finally { await scheduler.close(); }
  } finally { await f.close(); }
});

test('frozen routine results survive follow-up chat, routine deletion, restart, and future schedule calculations', async () => {
  const f = await fixture();
  try {
    const routine = await f.store.addRoutine(f.input({ kind: 'interval', minutes: 1 }));
    const run = await f.scheduler.run(routine.id);
    f.setTime('2026-01-01T00:00:30Z');
    await f.finish(run.conversationId);
    const result = structuredClone(run.result);
    assert.equal(result?.status, 'completed');
    const executionRunId = run.executionRunId;
    const conversation = f.store.conversation(run.conversationId);
    f.setTime('2026-01-01T00:01:00Z');
    conversation.run = { id: randomUUID(), status: 'running', startedAt: f.now().toISOString() };
    await f.store.changed();
    await f.store.updateRoutine(routine.id, f.input({ kind: 'interval', minutes: 1 }, { name: 'Edited after routine completion' }));
    await f.store.deleteRoutine(routine.id);
    const reopened = await Store.open(f.dataDir, { now: f.now });
    assert.deepEqual(reopened.routineRuns[0].result, result);
    assert.equal(reopened.routineRuns[0].executionRunId, executionRunId);
    assert.equal(reopened.conversation(conversation.id).run.status, 'interrupted');
    assert.equal(reopened.routines.length, 0);

    const recurring = await f.store.addRoutine(f.input({ kind: 'interval', minutes: 1 }));
    await f.finish(conversation.id);
    const recurringRun = await f.scheduler.run(recurring.id);
    f.setTime('2026-01-01T00:01:30Z');
    await f.finish(recurringRun.conversationId);
    f.setTime('2026-01-01T00:05:00Z');
    f.store.conversation(recurringRun.conversationId).run = { id: randomUUID(), status: 'completed', finishedAt: f.now().toISOString() };
    await f.store.changed();
    await f.scheduler.tick();
    assert.equal(f.store.routineRuns.filter(item => item.routineId === recurring.id).length, 2);
    assert.equal(recurringRun.result?.finishedAt, '2026-01-01T00:01:30.000Z');
  } finally { await f.close(); }
});

test('failed claim persistence prevents runtime start and halts further scheduler ticks', async () => {
  const f = await fixture();
  try {
    await f.store.addRoutine(f.input({ kind: 'once', at: '2026-01-01T00:01:00Z' }));
    await rename(join(f.dataDir, 'state.json'), join(f.dataDir, 'saved-state.json'));
    await mkdir(join(f.dataDir, 'state.json'));
    f.setTime('2026-01-01T00:01:00Z');
    await assert.rejects(f.scheduler.tick());
    assert.ok(f.scheduler.error);
    assert.equal(f.starts.length, 0);
    await rm(join(f.dataDir, 'state.json'), { recursive: true });
    await rename(join(f.dataDir, 'saved-state.json'), join(f.dataDir, 'state.json'));
    await assert.rejects(f.scheduler.tick());
    await assert.rejects(f.store.flush());
    assert.equal(f.starts.length, 0);
    assert.equal(f.store.snapshot().runtime.available, false);
  } finally { await f.close(); }
});

test('closing the scheduler during a durable claim prevents dispatch and leaves recoverable queued work', async () => {
  const f = await fixture();
  try {
    await f.store.addRoutine(f.input({ kind: 'once', at: '2026-01-01T00:01:00Z' }));
    let entered!: () => void;
    let release!: () => void;
    const claimed = new Promise<void>(resolve => { entered = resolve; });
    const held = new Promise<void>(resolve => { release = resolve; });
    const claim = f.store.claimDueRoutines.bind(f.store);
    f.store.claimDueRoutines = async now => { await claim(now); entered(); await held; };
    f.setTime('2026-01-01T00:01:00Z');
    const ticking = f.scheduler.tick();
    await claimed;
    const closing = f.scheduler.close();
    release();
    await Promise.all([ticking, closing]);
    assert.equal(f.starts.length, 0);
    assert.equal(f.store.routineRuns.length, 1);
    assert.equal(f.store.conversations[0].run.status, 'idle');
    const reopened = await Store.open(f.dataDir, { now: f.now });
    assert.equal(reopened.routineRuns.length, 1);
    assert.equal(reopened.conversations[0].run.status, 'idle');
  } finally { await f.close(); }
});

test('routine count is bounded and oversize routine mutations leave existing state untouched', async () => {
  const f = await fixture();
  try {
    const routine = await f.store.addRoutine(f.input({ kind: 'interval', minutes: 1 }));
    for (let i = 1; i < 1000; i++) f.store.routines.push({ ...structuredClone(routine), id: randomUUID() });
    await f.store.changed();
    const before = await readFile(join(f.dataDir, 'state.json'), 'utf8');
    await assert.rejects(f.store.addRoutine(f.input({ kind: 'interval', minutes: 1 })), error => error instanceof HttpError && error.status === 409);
    assert.equal(f.store.routines.length, 1000);
    assert.equal(await readFile(join(f.dataDir, 'state.json'), 'utf8'), before);
    f.store.conversations.push({
      id: randomUUID(), botId: routine.botId, title: 'Large retained transcript', messages: [
        { id: randomUUID(), role: 'assistant', text: 'x'.repeat(64 * 1024 * 1024), createdAt: f.now().toISOString(), tools: [] },
      ], run: { id: '', status: 'completed' }, updatedAt: f.now().toISOString(),
    });
    await assert.rejects(f.store.updateRoutine(routine.id, f.input({ kind: 'interval', minutes: 1 }, { name: 'Would exceed capacity' })), error => error instanceof HttpError && error.status === 413);
    assert.equal(f.store.routine(routine.id).name, 'Routine');
    assert.equal(await readFile(join(f.dataDir, 'state.json'), 'utf8'), before);
    await assert.rejects(f.store.changed(), /64 MiB/);
    assert.equal(await readFile(join(f.dataDir, 'state.json'), 'utf8'), before);
  } finally { await f.close(); }
});

test('accepted manual work dispatches when another due routine reaches history capacity', async () => {
  const f = await fixture();
  try {
    const routine = await f.store.addRoutine(f.input({ kind: 'interval', minutes: 5 }));
    const other = await f.store.addRoutine(f.input({ kind: 'once', at: '2026-01-01T00:01:00Z' }, { name: 'Unadmitted due routine' }));
    const original = await f.store.runRoutine(routine.id);
    await f.finish(original.conversationId);
    for (let i = 1; i < 9999; i++) {
      const conversationId = randomUUID();
      f.store.conversations.push({ ...structuredClone(f.store.conversation(original.conversationId)), id: conversationId });
      f.store.routineRuns.push({ ...original, id: randomUUID(), conversationId });
    }
    await f.store.changed();
    f.setTime('2026-01-01T00:01:00Z');
    const accepted = await f.scheduler.run(routine.id);
    assert.equal(f.store.routineRuns.length, 10000);
    assert.deepEqual(f.starts, [{ id: accepted.conversationId, prompt: routine.prompt }]);
    assert.equal(f.scheduler.error, undefined);
    assert.match(f.store.snapshot().routineSchedulingError ?? '', /history is full/);
    assert.equal(f.store.routine(other.id).enabled, true);
    assert.equal(f.store.routine(other.id).nextRunAt, '2026-01-01T00:01:00.000Z');
    await f.finish(accepted.conversationId);
    await f.scheduler.tick();
    assert.equal(f.starts.length, 1);
    assert.equal(f.scheduler.error, undefined);
    await f.store.setRoutineEnabled(other.id, false);
    await f.scheduler.tick();
    assert.equal(f.store.snapshot().routineSchedulingError, undefined);
  } finally { await f.close(); }
});

test('history capacity rejects new claims without pruning conversations or leaving partial state', async () => {
  const f = await fixture();
  try {
    const routine = await f.store.addRoutine(f.input({ kind: 'interval', minutes: 1 }));
    const original = await f.store.runRoutine(routine.id);
    await f.finish(original.conversationId);
    for (let i = 1; i < 10000; i++) {
      const conversationId = randomUUID();
      f.store.conversations.push({ ...structuredClone(f.store.conversation(original.conversationId)), id: conversationId });
      f.store.routineRuns.push({ ...original, id: randomUUID(), conversationId });
    }
    await f.store.changed();
    const count = f.store.conversations.length;
    await assert.rejects(f.store.runRoutine(routine.id), error => error instanceof HttpError && error.status === 409);
    assert.equal(f.store.conversations.length, count);
    assert.equal(f.store.routineRuns.length, 10000);
    assert.equal(f.store.routine(routine.id).nextRunAt, '2026-01-01T00:01:00.000Z');
    f.setTime('2026-01-01T00:01:00Z');
    await f.scheduler.tick();
    assert.equal(f.scheduler.error, undefined);
    assert.match(f.store.snapshot().routineSchedulingError ?? '', /history is full/);
    assert.equal(f.store.routine(routine.id).nextRunAt, '2026-01-01T00:01:00.000Z');
    assert.equal(f.store.conversations.length, count);
    assert.equal(f.starts.length, 0);
  } finally { await f.close(); }
});
