# Issue #33: cancel only the displayed follow-up report

All screenshots are actual Chrome captures of the local Rust application and real pinned engine with a scripted local provider. Only disposable synthetic fixture conversations are shown; no credentials or private conversation content are included.

## Matched Before / After

- **Before:** `d6c90d6698a890667a5edbbc3f898b53e26b09bc` (main after PR #24 and PR #30). Both the unchanged frontend and exact baseline Rust server were built before capture; the baseline has a separate Cargo target.
- **After implementation:** `d485cbf831614fd43d90f661cea55163a8726326`. A following evidence-only commit records this directory.
- Same scenario, locale `ko-KR`, timezone `UTC`, Dark theme, and viewports **1040 × 760** / **390 × 844**. Each isolated fixture has generated bot/conversation IDs.
- User asks Grok to request Reviewer's plan verdict and approves the message. Reviewer's answer arrives while an older routine occupies the global execution slot, leaving the follow-up report queued.
- For the no-op comparison, the browser holds the cancellation request, the routine finishes, and the real scheduler starts the selected report before the request proceeds. Both captures therefore show the same running report after the cancellation command; Before silently succeeds, while After explains that nothing was cancelled.

| State / viewport | Before | After |
| --- | --- | --- |
| Pending action, desktop | [Before](<./before-queued-1040.png>) | [After](<./after-queued-1040.png>) |
| Pending action, mobile | [Before](<./before-queued-390.png>) | [After](<./after-queued-390.png>) |
| Scheduler first / no-op, desktop | [Before](<./before-noop-1040.png>) | [After](<./after-noop-1040.png>) |
| Scheduler first / no-op, mobile | [Before](<./before-noop-390.png>) | [After](<./after-noop-390.png>) |

The pending action now visibly says **이 보고 취소** and sends only that summary's displayed delivery ID. No-op feedback is visible and announced after the user's action. Running and historical report chips have no status role and opt out of the transcript's live announcements; queued chips remain live.

## API and persistence

`POST /api/conversations/:id/bot-reports/cancel` requires strict `{deliveryIds: string[]}` with 1–10,000 UUIDs. It returns `{state: AppState, cancelledDeliveryIds: string[]}`. There is no `{}` cancel-all fallback. Unknown, no-longer-queued, and other-conversation IDs cannot cancel anything else. An empty actual result is explicit, not a claim of success. The HTTP command shares the scheduler's queue.

A selected A is cancelled only while still queued; a later unselected B remains eligible and executes normally. Dispatch-first leaves A running, reports no cancellation, and directs the user to the existing Stop control. Repeated cancellation writes nothing when the selected reports are no longer queued.

Before the first unstarted cancellation is written, the ordered store writer preserves the previous compatible bytes as `state.json.pre-unstarted-report-cancellation.bak` (private, create-once, no overwrite). A required-backup failure prevents the incompatible write through the normal store failure path. This is separate from the pre-reports backup. Older report-aware binaries requiring a run ID cannot read unstarted cancellations. Already-new-shape states from the earlier implementation cannot acquire a compatible backup retroactively, and a misleading new-shape backup is not fabricated. Restoring a backup is a manual whole-state rollback that loses later changes.

## Verification

Completed frontend checks: production build/typecheck, existing frontend-state tests, interface detector, and all **4** Rust-backed Playwright scenarios. The browser suite covers the original automatic follow-up, desktop/mobile selected cancellation with disk and server-restart persistence, exact outgoing selected IDs, rejected-command retry, in-flight duplicate-action disabling, keyboard activation, conditional live-region attributes, and scheduler-first no-op feedback with the report continuing to completion.

Rust store/core regression suites passed, including real HTTP/shared-queue tests for cancellation-first and dispatch-first, A selected followed by B arriving and only B executing, strict body validation, exact private create-once backup bytes, restart/no overwrite, fail-closed obstructed or invalid backups, and no fabricated rollback backup for already-new-shape states. Format and whitespace checks passed; final lint and independent-review results are recorded on the PR. No paid model, hosted deployment, or GitHub approval by the local reviewer is claimed.
