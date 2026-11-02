# Issue #11: queued Bot follow-up cancellation

All images are actual Chrome application captures, not mockups. They show disposable synthetic conversations with a local scripted OpenAI-compatible provider and the real pinned application engine. No credentials or personal conversation content are included.

## Matched Before / After

- **Before source:** `95ffc464e8d55c1a315fddb31101fadb9c9c5b7b` (merged prerequisite PR #9). The unchanged frontend was built before implementation edits; the baseline Rust server was rebuilt from this exact revision in a detached worktree.
- **After implementation:** `9ecce524dc0390db52010c33e52b7d4d4b8acf42`. The following commit only adds this evidence directory.
- Same scenario: the user asks Grok to request a plan review from Reviewer, approves the outbound message, and receives Reviewer's answer. A separately queued, older routine holds the shared execution slot so the sender's automatic follow-up remains `queued`.
- Same browser locale `ko-KR`, timezone `UTC`, dark theme, and viewport: **1040 × 760** desktop / **390 × 844** mobile. Synthetic bot/conversation IDs are newly generated in each isolated run.
- The baseline has the same queued report in stored state but no pending-report status or cancellation action in the message summary. The changed application shows **이어서 답하기 · 대기 중 · 취소** next to the delivery summary.

| Viewport | Before | After |
| --- | --- | --- |
| Desktop, queued report | [Before, 1040 × 760](<./before-queued-1040.png>) | [After, 1040 × 760](<./after-queued-1040.png>) |
| Mobile, queued report | [Before, 390 × 844](<./before-queued-390.png>) | [After, 390 × 844](<./after-queued-390.png>) |

Additional changed-state captures, not replacements for the matched dark comparisons:

- [After cancellation, desktop](<./after-cancelled-1040.png>): the report reads **취소됨**, the action is gone, and the received answer and original conversation remain intact.
- [Light theme at 320 px](<./after-light-320.png>): the summary wraps without clipping the action. Dark, Light, and System-light were all checked at 320 px; the action is at least 24 × 24 px with inherited readable theme colors.

## Behavior and scope

The message-summary action cancels **all reports currently queued in that source conversation**. Its accessible name and tooltip explicitly say so. Already-running follow-ups are unchanged and use the existing **작업 중단** control. This is not a pause setting: new reports created after cancellation are unaffected. Repeating the command is a no-op only while no new queued report has appeared.

`POST /api/conversations/:id/bot-reports/cancel` takes strict `{}`, requires the existing same-origin JSON mutation headers, returns `200 AppState`, and returns 404 for a missing conversation. It persists `cancelled` plus `finishedAt` without allocating a run or hidden report input. The command and scheduler share one queue; a report still queued when cancellation is processed cannot subsequently be dispatched. If dispatch already started it, this endpoint leaves it running.

Follow-up reports remain Rust-backend-only. After writing an unstarted cancellation, use the updated Rust server: older binaries that require a run ID for every cancelled report reject that saved shape.

## Completed verification

All checks below passed locally:

```sh
npm run build
npm run build:server
npx tsx --test tests/frontend-state.test.ts
node scripts/cargo.mjs test --locked -p grok-bot-model -p grok-bot-store -p grok-bot-core
node scripts/cargo.mjs clippy --locked -p grok-bot-model -p grok-bot-store -p grok-bot-core --all-targets -- -D warnings
cargo +stable fmt --all --check
GROK_BOT_BACKEND=rust npx playwright test tests/bot-reports.spec.ts
git diff --check
```

- Rust store tests cover multiple queued reports, conversation scope, preservation of running/finished reports and unfinished deliveries, idempotency, missing conversation, two reopens, and rejection of forged cancelled reports with missing timestamps or hidden-input references.
- Live HTTP tests cover mutation/body guards, the returned state, preservation of an independently active source run, idempotency, and backend restart.
- Real-engine runtime tests cancel before scheduler dispatch through the shared command queue, verify no extra model call/message/run, reject direct report startup, and verify no replay after reopening the store and runtime.
- All **3** Playwright scenarios pass: the existing successful automatic follow-up, plus desktop and mobile queued cancellation. New scenarios exercise server rejection/retry, disabled duplicate actions while a request is in flight, keyboard activation, actual disk persistence, slot release with scheduler polls, page reload, and actual Rust-server restart without a follow-up model call.
- Frontend state tests pass; the interface detector reports no findings. Source and desktop/mobile screenshot self-review plus an independent read-only code/visual review found no unresolved material issue.

The frontend build retains its existing large-chunk advisory; the build succeeds. No external paid model or hosted application deployment is claimed. GitHub CI is recorded separately on the PR.
