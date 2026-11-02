# Issue #14: same-Bot worker evidence

Every image here is an actual Chrome capture of the application, not a mockup. Conversations, tasks and example weather values are synthetic/disposable; no user chats, credentials, or private workspace files are shown.

## Matched Before / After

- **Before source:** `95ffc464e8d55c1a315fddb31101fadb9c9c5b7b`, the clean merged PR #9 baseline, with its actual compiled Rust backend and built frontend.
- **After source:** `096d50bcdd44dddf4b6e7dfce59ce7d8d564c0dd` (the following commit adds only this evidence).
- Same scenario: send `제공한 예시 기온 자료를 서울·부산·제주로 나눠 확인하고, 세 도시의 결과를 모아 비교해 줘.` A scripted provider discovers `workspace spawn_worker` and attempts three distinct tasks. Hold the next parent inference so both captures show the parent still running immediately after those attempts.
- The old engine/app really rejects the nonexistent tool. The changed app asks for each new task's one-use approval, queues three hidden workers and renders their progress. No worker is allowed to start while the parent is held.
- Same browser locale `ko-KR`, timezone `Asia/Seoul`, dark theme, explicit blob avatar and viewport: 1040 × 760 desktop / 390 × 844 mobile. The after view scrolls to the added progress panel as chat content grows.

| Scenario | Before | After |
| --- | --- | --- |
| Desktop parent still running | [Before](before-desktop.png) | [After](after-desktop.png) |
| Mobile parent still running | [Before](before-mobile.png) | [After](after-mobile.png) |

[Before capture observations](before-observations.json) and [After capture observations](after-observations.json) record the common conditions and zero page errors. These are controlled provider comparisons, not evidence of an actual model's autonomous tool choice.

## Additional changed states

- [One finished / one running / one queued](after-active-desktop.png): no early report, parent Stop remains available while its own turn is already complete.
- [One grouped parent follow-up after reload](after-completed-desktop.png): all siblings terminal, report marker visible, hidden input absent.
- [61-line full-task disclosure at 320 px](after-long-task-320.png), [light theme on desktop](after-light-desktop.png), and [mobile active/result disclosures](after-active-mobile.png) extend coverage beyond the paired comparison. Browser tests also verify system-light, long unbroken results, keyboard disclosures, collapse state across real SSE updates, public GET/SSE filtering and history, reload, denial and parent cancellation. Long tasks have a bounded preview and explicit full-task disclosure; full task and result text support keyboard scrolling.

## Actual model, separate from the scripted comparison

The final worker implementation was exercised with actual `gpt-6.1-sol`. The prompt asks for three same-Bot workers, each summarizing one city's **provided example** temperature and rain probability; it does not ask for current/live weather or external research. The model actually calls the delegation tool three times, each is separately approved in the disposable browser, the workers answer, and the parent synthesizes all three in one automatic follow-up.

- [Actual model while a child is running](real-model-active.png).
- [Actual model's grouped answer after reload](real-model-completed.png).
- [Sanitized observations and output](real-model.json): three approvals, three completed tasks, one completed report, one public conversation, one visible user message, zero page errors, and the canonical user state unchanged during the smoke run.

No extra worker-specific instructions were added to the fixture Bot. This is one qualitative end-to-end check, not a reliability benchmark or a claim of external weather access.

## Verification record

- Frontend strict TypeScript and production build: passed.
- Rust formatting and strict clippy for model/store/core: passed.
- Model/store: 114 tests passed, plus the 12 worker tests rechecked after lint-only rewrites.
- Core: 67 tests passed, including all existing and new real-engine runtime scenarios.
- Node regression: 106 passed with `--test-concurrency=1`. The initial parallel run hit the existing routine byte-capacity test's 10-second wait; that isolated test and the complete serial suite then passed without changing it.
- New worker Chrome suite: 5 passed (1040/390/320 px, Stop, denial).
- Existing report/group/thread/checklist Chrome regression: 8 passed.
- Independent correctness and interface reviews: findings fixed and reviewed; source review has no open actionable findings. A Stop/report-completion race was fixed with a persisted-state reopen regression. A long-task compactness issue was fixed and verified with the 61-line browser scenario.
- The scripted comparisons and actual-model smoke each reported zero page errors. Mechanical interface detector reported no findings.

Source-level details and limitations are in [the worker lifecycle note](../../worker-delegation.md).
