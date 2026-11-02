# Issue #18 — current-main integration and deterministic cache verification

This comparison refreshes the [initial evidence](../issue-18/README.md) after integrating current main. All images are unedited captures of the actual application with disposable model/site fixtures, not mockups or real private conversations.

## Revisions and matched scenario

- **Before:** `c07a1cfec41e84ab86e566e7c72e3684dae1c31c`, unchanged application in a detached baseline worktree; [capture details](BEFORE.md).
- **After:** `fe16467dee13b0e3290d333724418bf6bb98a8f6`, the PR branch with latest main merged. A subsequent evidence-only commit adds these captures and this record.
- Same Chrome 150.0.7871.181, dark theme, blob avatar, completed answer, unhovered UI, and fixture article `http://127.0.0.1:43128/guide`.
- Same prompt: `작업 진행 상황을 확인할 수 있는 자료를 알려줘`. Same answer: `참고할 자료를 찾았습니다.` and the Markdown link `봇의 진행 상황을 한눈에`.
- Desktop **1040 × 760**, mobile **390 × 844**. Settings comparisons use General, scrolled to the bottom. Before has no preview setting/card; After first verifies default-off/no site requests, then explicitly enables the switch and shows the decoded local thumbnail/title/description card. The settings screenshot shows that on-state and disclosure.
- The new main canvas control remains visible in both versions; no adjacent feature is removed to make the comparison match.

| Scenario | Before | After |
| --- | --- | --- |
| Desktop chat, dark | [Before](before-1040.png) | [After](after-1040.png) |
| Mobile chat, dark | [Before](before-390.png) | [After](after-390.png) |
| Desktop General settings, dark | [Before](before-settings-1040.png) | [After](after-settings-1040.png) |
| Mobile General settings, dark | [Before](before-settings-390.png) | [After](after-settings-390.png) |

Additional changed-state coverage: [desktop light](after-1040-light.png), [mobile light](after-390-light.png).

## Deterministic cache test

The previous combined cache test used a 30 ms TTL across asynchronous HTTP requests. A slow request could legitimately expire an earlier entry during the unrelated LRU/capacity assertions, producing the reported cache size `1` instead of `2`.

The cache now receives its own private monotonic time source. Production still uses `Instant::now`; the test freezes that cache clock and advances it explicitly. Network deadlines continue to use real monotonic time and are not frozen. The test verifies a cache hit one nanosecond before expiry, a miss at the exact expiry boundary, LRU eviction/image cleanup, coalesced repeated URLs, and both hit and expiry for failure entries. It no longer sleeps or relies on HTTP completion within the cache TTL.

The formerly failing case passed once after compile and **20 consecutive repetitions** against the integrated source. All **19** preview unit/security tests also passed.

## Integration and relevant checks

- Latest main was merged, not rebased. Both the link-preview and canvas documentation/routes/UI remain present. Preview service construction now goes through main's shared `HttpContext::new`, preserving its probe lifecycle initialization.
- Frontend production build/typecheck passed; **99** affected Node tests passed (URL selection, preference compatibility, state/composer/canvas/summary regressions).
- Full Rust workspace tests passed after integration, including maintenance, canvas, engine/runtime, storage and preview cases. Affected model/store/core all-target clippy with `-D warnings`, formatting and server build passed.
- Actual Rust Chrome acceptance: **5/5 passed**, covering the four preview/privacy scenarios and canvas read/edit/export integration. The two baseline Chrome comparison scenarios also passed.
- No browser request for the external fixture page or thumbnail, page error, or horizontal overflow was observed in the preview scenarios. The 320 px long-text and failed opt-out persistence cases passed.

```sh
npm run build
node scripts/cargo.mjs test --offline --locked -p grok-bot-core --lib link_preview
node scripts/cargo.mjs test --offline --locked --workspace
node scripts/cargo.mjs clippy --offline --locked -p grok-bot-core -p grok-bot-store -p grok-bot-model --all-targets -- -D warnings
npm run build:server
GROK_BOT_BACKEND=rust LINK_PREVIEW_FIXTURE_PORT=43128 \
  GROK_BOT_LINK_PREVIEW_EVIDENCE_DIR=docs/evidence/issue-18-main-refresh \
  npx playwright test tests/link-previews.spec.ts tests/link-preview-privacy.spec.ts tests/canvas-read.spec.ts
```

See the [feature contract](../../link-previews.md) for the existing security/resource limitations and the [initial reproduction procedure](../issue-18/README.md#reproduction) for baseline setup. Network checks use isolated local fixtures; no live external SSRF probe is performed. Final GitHub CI and exact-base/head independent review status belong to the PR record, not the screenshots.
