# PR38 — Rust-only Before

Baseline: `origin/main` at **`e7de2f2d4f10539e0e25ee39752dd47e205c06ba`**, after PR28 removed the Node backend. These are actual screenshots of that revision's frontend and Rust debug server, not the feature branch running with previews disabled. Tracked baseline source was unchanged; only the [comparison scenario](<../../../tests/link-previews.spec.ts>) was copied in temporarily.

## Matched scenario

- Fresh local state; dark theme; blob avatar; pointer moved to `(0, 0)` before capture.
- Prompt: `작업 진행 상황을 확인할 수 있는 자료를 알려줘`.
- Completed mock-provider answer: `참고할 자료를 찾았습니다.` followed by the link `봇의 진행 상황을 한눈에` to `http://127.0.0.1:43128/guide`.
- Viewports: **1040×760** and **390×844**. General settings are scrolled to the bottom.

| Viewport | Completed answer | General settings |
| --- | --- | --- |
| 1040×760 | [Before](<before-1040.png>) | [Before settings](<before-settings-1040.png>) |
| 390×844 | [Before](<before-390.png>) | [Before settings](<before-settings-390.png>) |

The answer contains a plain text link, without a preview card. General settings have no link-preview switch. All four images were opened and inspected.

## Reproduction and result

From the detached baseline worktree, after copying the comparison scenario:

```sh
npm run build
CARGO_TARGET_DIR="$(realpath ../repo/target)" npm run build:server
cp --reflink=auto ../repo/target/debug/grok-bot ../baseline-rust-server
GROK_BOT_SERVER_BIN="$(realpath ../baseline-rust-server)" \
  LINK_PREVIEW_BASELINE=1 LINK_PREVIEW_FIXTURE_PORT=43128 \
  GROK_BOT_LINK_PREVIEW_EVIDENCE_DIR=../repo/docs/evidence/issue-18-rust-only \
  npx playwright test tests/link-previews.spec.ts --grep 'visual comparison'
```

Frontend typecheck/build and Rust debug build passed. Playwright: **2 passed (4.3s)**, with no page errors, off-origin browser requests, or horizontal overflow. Baseline mode bypasses new-feature assertions; the fixed article received no request before capture. No release build or standalone Node backend was used.

Scenario SHA-256: `7fc240019b19d2420c83e7839b3dd56956d709d374e0f76cd1935c37610db4bc`.

Scope: **Before only**, using a local mock model. No After or live-model behavior is claimed here.
