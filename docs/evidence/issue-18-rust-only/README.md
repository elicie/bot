# Issue #18 — Rust-only integration after #28

All screenshots are actual application captures using disposable local model/site fixtures. Both Before and After use the Rust server; no Node backend, private conversation, credential, or design mockup is used.

## Revisions and scenario

- **Before:** `e7de2f2d4f10539e0e25ee39752dd47e205c06ba`, the main revision that merged #28. Built independently in the detached baseline worktree; [details](BEFORE.md).
- **After:** `0f1b5b3ea443b9f0c956c52a8bb4635d2a69913e`, this feature branch after merging that main and removing legacy backend compatibility. Subsequent commits add only evidence documentation/images.
- Same Chrome 150.0.7871.181, dark theme, blob avatar, unhovered pointer, fresh conversation, completed answer, and fixed article origin `http://127.0.0.1:43128`.
- Same prompt: `작업 진행 상황을 확인할 수 있는 자료를 알려줘`. Same answer: `참고할 자료를 찾았습니다.` followed by the link `봇의 진행 상황을 한눈에`.
- Desktop **1040 × 760**, mobile **390 × 844**. General settings is scrolled to the bottom. Before has no preview card/switch; After first verifies default-off/no site requests, then explicitly enables the switch and waits for metadata and a decoded same-origin thumbnail. The settings captures show the resulting on-state and privacy notice.

| Scenario | Before | After |
| --- | --- | --- |
| Desktop chat, dark | [Before](before-1040.png) | [After](after-1040.png) |
| Mobile chat, dark | [Before](before-390.png) | [After](after-390.png) |
| Desktop General settings, dark | [Before](before-settings-1040.png) | [After](after-settings-1040.png) |
| Mobile General settings, dark | [Before](before-settings-390.png) | [After](after-settings-390.png) |

Additional changed-state coverage: [desktop light](after-1040-light.png), [mobile light](after-390-light.png).

## Rust/web-only scope

- Main's removal of the legacy backend is preserved. This PR has no diff under the removed backend directory and no backend compatibility test.
- Preview fetching, persistence, generated-image serving and security tests remain Rust. URL extraction/state tests remain frontend units.
- Browser acceptance uses the repository's default Rust fixture and no longer depends on the removed backend-selection environment variable. The four preview/privacy cases run in the normal e2e command rather than silently skipping.
- The current server's embedded-web/CLI lifecycle, MCP app and canvas rendering remain present. Old compatibility-only wording was replaced by a generic old-server-version message; older capture records are labelled historical.

## Local verification

- Frontend production build/typecheck: **passed**.
- All **121 frontend unit tests**: **passed**.
- Rust preview unit/security tests **19/19**, HTTP settings/restart integration **1/1**, store preference/schema tests **3/3**: **passed**.
- Formatting, affected core/store/model all-target clippy with `-D warnings`, debug server build: **passed**.
- Default-Rust browser acceptance: **5/5 passed**, including four preview/privacy cases and canvas/MCP app state coexistence. No backend-selector variable was set.
- Matched capture runs: Before **2/2 passed**; After **2/2 passed**. Both sides check page errors, browser requests staying on the app origin, and horizontal overflow. The thumbnail fixture is the public Issue #6 application capture.

```sh
npm run build
npm test
node scripts/cargo.mjs test --offline --locked -p grok-bot-core --lib link_preview
node scripts/cargo.mjs test --offline --locked -p grok-bot-core --test link_preview_api
node scripts/cargo.mjs test --offline --locked -p grok-bot-store --test link_previews
node scripts/cargo.mjs clippy --offline --locked -p grok-bot-core -p grok-bot-store -p grok-bot-model --all-targets -- -D warnings
npm run build:server
LINK_PREVIEW_FIXTURE_PORT=43128 \
  GROK_BOT_LINK_PREVIEW_EVIDENCE_DIR=docs/evidence/issue-18-rust-only \
  npx playwright test tests/link-previews.spec.ts --grep 'visual comparison'
```

To reproduce Before, create a detached worktree at the named baseline, copy the comparison spec into its tests directory, build that frontend and Rust debug server, then run the same visual test with `LINK_PREVIEW_BASELINE=1`. No baseline application source is changed. [The Before record](BEFORE.md) includes the isolated-server command used for these captures.

[Feature/API/security contract](../../link-previews.md). Tests use isolated local fixtures and the existing exact debug/test-origin exception; no live external SSRF probe is performed. Final CI and exact-base/head independent review status are recorded in the PR rather than inferred from screenshots.
