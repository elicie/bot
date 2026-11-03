# Issue #18 — matched link-preview evidence

Historical pre-P4 capture record. Its Node-backend reproduction steps apply only to the revisions named below; the current Rust-only comparison and commands are in [the post-#28 record](../issue-18-rust-only/README.md).

All images are actual Chrome application captures, not design mockups. Conversations, model replies and remote-site metadata are disposable fixtures. The thumbnail fixture is the repository's public Issue #6 application capture. No real user conversation, provider key, or private Feishu content is included.

## Revisions and scenario

- **Before:** `d6c90d6698a890667a5edbbc3f898b53e26b09bc`, built and launched from a detached baseline worktree. Its unchanged Node backend and real pinned engine drive the baseline conversation.
- **After capture revision:** `94065bf1cdde4ea49136a59ca17e85ec9bbb8e0d`, built frontend and Rust server. The production implementation is `8aef6ed47743f8a4091f6f4741fb72c924c5ce74`; the later commit only strengthens tests. The commit introducing this record and the images is evidence-only.
- Same Google Chrome **150.0.7871.181**, dark theme, original blob avatar, unhovered UI state, fixed local-site origin (`http://127.0.0.1:43128`), prompt and assistant answer.
- Prompt: `작업 진행 상황을 확인할 수 있는 자료를 알려줘`.
- Assistant answer: `참고할 자료를 찾았습니다.` followed by the same Markdown link labelled `봇의 진행 상황을 한눈에`.
- The server fixture provides an Open Graph title, Korean description and relative PNG thumbnail. The After case first verifies default-off/no site requests, then uses **설정 → 일반 → 링크 미리보기** to opt in and waits for metadata plus a decoded same-origin thumbnail.
- Desktop viewport: **1040 × 760**. Mobile viewport: **390 × 844**. General settings captures use the same modal and bottom-of-section scroll state; the added setting/privacy notice naturally increases that section's height.

## Before / After

Before shows only the original Markdown link and has no link-preview setting. After shows the same answer plus its title/description/local-thumbnail card; the After settings captures show the new switch **on** and its server-network disclosure. Default-off/no requests is verified before enabling, not represented by the on-state screenshot.

| Scenario | Before | After |
| --- | --- | --- |
| Desktop chat, dark | [Before](before-1040.png) | [After](after-1040.png) |
| Mobile chat, dark | [Before](before-390.png) | [After](after-390.png) |
| Desktop General settings, dark | [Before](before-settings-1040.png) | [After](after-settings-1040.png) |
| Mobile General settings, dark | [Before](before-settings-390.png) | [After](after-settings-390.png) |

Additional changed-state coverage: [desktop light](after-1040-light.png) and [mobile light](after-390-light.png). These supplement, rather than replace, the paired dark-theme evidence.

## Reproduction

Run the actual changed app through the scripted local model/site fixtures:

```sh
npm ci
npm run build
npm run build:server
GROK_BOT_BACKEND=rust LINK_PREVIEW_FIXTURE_PORT=43128 \
  GROK_BOT_LINK_PREVIEW_EVIDENCE_DIR=docs/evidence/issue-18 \
  npx playwright test tests/link-previews.spec.ts tests/link-preview-privacy.spec.ts
```

To reproduce Before, start at this PR's checkout and use a fresh sibling worktree. The named comparison test is copied from the PR into the baseline; its snapshot type already declares the optional new field locally, so no application source/type is added to Before:

```sh
# Run from this PR checkout; the sibling destination must not already exist.
git worktree add --detach ../link-preview-before d6c90d6698a890667a5edbbc3f898b53e26b09bc
cp tests/link-previews.spec.ts ../link-preview-before/tests/link-previews.spec.ts
cd ../link-preview-before
npm ci
npm run build:engine
npm run build
LINK_PREVIEW_BASELINE=1 LINK_PREVIEW_FIXTURE_PORT=43128 \
  GROK_BOT_LINK_PREVIEW_EVIDENCE_DIR=docs/evidence/issue-18 \
  npx playwright test tests/link-previews.spec.ts --grep 'visual comparison'
```

Both sides require the normal project build tools and system Chrome described in [the project README](../../../README.md). The tests launch their app, model and site fixtures and stop them automatically; port 43128 must be free. Before runs the unchanged Node backend; After selects the built Rust server. The output directory is relative to each checkout.

After tests start the debug server with `NODE_ENV=test` and `GROK_BOT_LINK_PREVIEW_TEST_ORIGINS` set to a JSON array containing only the exact fixture loopback origin. Release builds ignore this exception. Production public-address validation is separately exercised without external probes:

```sh
node scripts/cargo.mjs test --locked -p grok-bot-core --lib link_preview
node scripts/cargo.mjs test --locked -p grok-bot-core --test link_preview_api
node scripts/cargo.mjs test --locked -p grok-bot-store --test link_previews
```

Request observation checks that the browser only accesses the application origin for previews, not the fixture page or thumbnail directly.

## Coverage

The companion browser tests cover default-off/no requests; explicit enable; one title/description/local-thumbnail card; maximum three distinct cards per assistant answer (URL fragments do not create additional cards); title/description fallback; ignored images, quoted URLs and code; server cache hit/reload; rejection with 403 when a preview GET lacks `X-Grok-Bot: 1`; opt-out and durable reload; zero remote browser requests and page errors; 320 px long-unbroken-content wrapping; keyboard focus; and a failed opt-out save that stays paused in the current tab with an explicit error.

Observed results for the capture tree:

- Before frontend build/typecheck and both desktop/mobile comparison scenarios passed.
- After frontend build/typecheck, 120 Node tests, 215 Rust workspace tests, Rust formatting and workspace clippy (`-D warnings`) passed. The final raw-header fixture change was rechecked with all 19 preview unit tests and clippy; production code was unchanged.
- After browser acceptance plus markdown/media/report-cancellation regressions: 12 scenarios passed, including the four dedicated preview scenarios.
- Independent static security and desktop/mobile visual review found no outstanding actionable issue. The final PR revision review and GitHub CI status are recorded in the PR, not implied by these captures.

For API contracts, resource budgets, known metadata limitations and security-boundary details, see [the feature note](../../link-previews.md).
