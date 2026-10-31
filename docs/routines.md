# Routines

## Understood as

After the existing baseline commit, make bot creation and bot conversation usable from the running network HTTP application and add durable routine creation, scheduling, execution, and history. Preserve the existing Grok Bot visual system and local file approval boundary.

## Contract

`shared/types.ts` defines `RoutineSchedule`, `Routine`, `RoutineInput`, `RoutineRun`, and `RoutineRunResult`. `AppState` includes `routines` and `routineRuns`. Old version-1 state migrates by defaulting these arrays to empty. Existing bots, messages, provider settings, and execution sessions remain intact.

Schedules support a single future ISO timestamp, an interval of 1–10080 minutes, and a daily local time with an explicit IANA timezone. The backend calculates and persists `nextRunAt`. The client never predicts scheduling authority. The server must remain running to execute a schedule. Missed recurring occurrences coalesce into at most one queued run per routine. Daily schedules use the IANA timezone: a nonexistent spring-forward time shifts by the DST gap, and a repeated fall-back time runs at its earlier occurrence once per local date.

## Ownership and concurrency

An app-owned scheduler uses the HTTP command queue so it cannot race manual messages, settings, or other routine claims. A claim creates one new conversation and one `RoutineRun`, advances the schedule or completes a one-time schedule, and persists the claim before starting Grok. Queued and active run status derives from the linked conversation. Its `idle` status means queued. `RoutineRun.executionRunId` binds history to one actual run, and `RoutineRun.result` freezes terminal status, timestamps, and error before allowing a follow-up conversation to replace `Conversation.run`. Store persistence reconciles terminal routine results in the same write as run completion or startup interruption. Each routine admits at most one queued or active execution. A globally active run delays queued work. Manual runs do not alter recurring schedule times.

On restart, queued conversations may start, while already-started runs retain the store's interrupted state. Started side effects are never replayed automatically. Pausing stops future schedule claims and retains any active execution. Deleting a routine requires no queued or active execution, and retains previous run metadata and conversations. Invalid provider configuration produces a visible failed run and does not spin in a retry loop. File writes keep the existing one-use browser approvals.

## API

All existing Host, Origin, JSON, and mutation-header checks apply.

| Method | Route | Body | Response |
| --- | --- | --- | --- |
| POST | `/api/routines` | `RoutineInput` | `AppState`, 201 |
| PUT | `/api/routines/:id` | `RoutineInput` | `AppState` |
| PATCH | `/api/routines/:id` | `{ enabled: boolean }` | `AppState` |
| DELETE | `/api/routines/:id` | `{}` | `AppState` |
| POST | `/api/routines/:id/run` | `{}` | `RoutineRunResult`, 202 |

The frontend provides a sidebar Routines view, a list, an inline create/edit form, pause/resume, Run now, delete confirmation, and execution history with a View conversation action. Errors and empty states remain visible. Returning to a bot restores its draft and selected conversation, including an unsent new-chat draft when routines create background conversations. Mobile uses the same navigation semantics.

Capacity limits retain existing history: 1,000 routines, 10,000 executions, and 64 MiB of persisted state. Admission includes serialized prompt bytes and startup metadata for each queued execution before accepting work. Model responses and tool output still contribute to the finite state limit. A capacity refusal leaves new due claims and their schedule cursor unchanged. Accepted queued work continues dispatching, and a committed manual claim still returns 202. `AppState.routineSchedulingError` reports the scheduling capacity problem until a later tick succeeds. Persistence failures halt the scheduler and require data-directory recovery before restart. Ordinary message requests for queued routine conversations return 409 to preserve their saved prompt.

## Implementation boundaries

- Backend owner edits `server/store.ts`, `server/app.ts`, a new `server/routines.ts`, and routine backend tests. Existing runtime execution remains the sole agent loop.
- Frontend owner edits `web/` for routines and navigation. It preserves independent app state and shared contracts.
- Coordinator owns shared types, browser HTTP compatibility fixes, documentation, end-to-end tests, live server restart, and final integration.

## Verification

Use deterministic clocks and disposable private state for scheduler tests. Verify due-time boundaries, overlapping manual and scheduled triggers, restart after claim, interrupted execution, disabled schedules, update/delete validation, retention of history, invalid setup, and timezone/DST transitions. Browser tests exercise bot creation, real Grok conversation through a loopback OpenAI-compatible fixture, routine creation/editing/pause/resume/Run now, history navigation, scheduled firing, reload, and desktop/mobile views. A representative non-loopback HTTP origin must work without secure-context-only browser APIs. No real provider credentials or model spend are used.
