# Issue #6 visual and real-model evidence

All images are actual Chrome application captures, not mockups. Only disposable synthetic conversations and fixture files are shown. No credentials or personal conversation content are included.

## Matched Before / After

- **Before source:** `05a275abc6e9dd0ad27b498dd9bebc040d053c4a`, built in a detached baseline worktree (frontend and Rust server).
- **After implementation:** `6cd996216cbcc444d08d0d85f54cf125a3c4107d` (subsequent commit adds only documentation/evidence).
- Same scenario: send `요청을 3단계로 나눠서 해 줘`; the real pinned engine receives a scripted provider `todo_write` with `merge:false` and three items: `요청과 자료 확인` completed, `핵심 내용 정리` in_progress, `결과 검토` pending. Hold the next inference so both screenshots show the running state.
- Same browser locale `ko-KR`, dark theme, explicit blob avatar, viewport 1040 × 760 desktop / 390 × 844 mobile.
- The baseline profile omits `todo_write`, so its real engine rejects the call. The changed app exposes it and renders the resulting checklist.

| Scenario | Before | After |
| --- | --- | --- |
| Desktop | [Before](before-desktop.png) | [After](after-desktop.png) |
| Mobile | [Before](before-mobile.png) | [After](after-mobile.png) |

[Light/System-light mobile](after-mobile-light.png) is additional changed-state coverage, not a replacement for the dark-theme paired comparison. The browser suite also checks 320 px and long unbroken content.

## Actual model, separate from the scripted comparison

The final implementation was also exercised through Chrome with actual `gpt-6.1-sol` and no added todo-specific bot/session guidance. The user prompt names the three-step task, not the planning tool. The model really listed the fixture directory, read its two text files and reviewed their differences.

- [In-progress screenshot](real-model-progress.png).
- [Completed screenshot after reload](real-model-completed.png).
- [Sanitized state observations](real-model.json): four successful native plan calls, four actual state changes while the run was still running, final list retained after reload, zero browser errors and unchanged canonical state.

The earlier six-run guidance exploration and its negative drafting-only results are summarized in [the behavior/verification note](../../todos.md#실제-모델-확인과-지침-결정). Those observations are qualitative, not a statistical reliability claim.

## Final checks

- Frontend production build / strict TypeScript: passed.
- Node tests: 105 passed; Node checklist Chrome tests: 3 passed (1040/390/320 px).
- Rust workspace: 179 passed with `umask 022`.
- Rust checklist/history/group/thread Chrome tests: 8 passed.
- Independent source and desktop/mobile visual review: findings fixed and re-reviewed; no remaining actionable findings.

The inherited `umask 077` exposed an **existing, unrelated** file-mode preservation test failure (expected 0640, got 0600). The same single test failed on the untouched Before revision. The full suite passed with the fixture-compatible `umask 022`; no file-permission production code or test was changed to hide it.
