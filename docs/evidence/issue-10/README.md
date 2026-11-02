# Issue #10: bounded, plain-text conversation previews

## Before / After

Both images are actual Chrome application captures with the same seven synthetic persisted conversations, viewport **1280 × 900**, sidebar width **400 px**, **ko-KR** locale, **dark** theme and explicit blob avatars. The selected main bot is empty. Ordinary rows exercise math, Markdown, code, visualization, a generated Markdown image, a bare generated-image link and an HTML-only message. No personal conversations, credentials, or external model calls are involved.

- **Before source:** `95ffc464e8d55c1a315fddb31101fadb9c9c5b7b`, in a detached worktree with no production edits. Only the current test scenario was copied in to capture it. Its plain-text assertion fails on the raw math preview as expected.
- **After implementation:** `e15b866b26948dc210376656e42e7e81f5b1ce61`. The subsequent evidence commit changes only this documentation and captures.
- Both captures use the Node app backend and the same version-checked pinned engine dependency (`0.1.0`, source `2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8`). The scenario reads seeded messages rather than model-generated responses.

| Before | After |
| --- | --- |
| ![Before: raw Markdown, TeX and media paths](<before.png>) | ![After: plain text and short media labels](<after.png>) |

## Behavior and resource limits

- `\(23 = 20 + 3\)으로 나누어 계산...` becomes `23 = 20 + 3으로 나누어 계산...`.
- Markdown structure keeps visible text and link labels. Code blocks become `코드`; existing Mermaid/HTML/SVG/visualization fence types become `차트`. Generated-image destinations, including inline/reference/bare links, and other Markdown images become `이미지`.
- Paired math delimiters (`\(` / `\)`, `\[` / `\]`, `$` / `$$`) are removed without interpreting the formula body as Markdown. Inline code loses its backticks but retains literal contents. Neither a code-internal opener nor closer can consume outside text; a formula cannot consume a fenced/indented code block.
- The latest nonblank, non-report message is selected, with existing active/error states taking precedence. Raw HTML remains skipped, but a nonempty HTML-only message gets `메시지`, not the false claim that no messages exist. Actual empty conversations retain the original empty-state copy.
- **Only the first 2,048 UTF-16 code units enter parsing/scanning.** Normalized math uses shorter collision-free markers, so a second parse cannot expand past that cap. Output collapses whitespace and is at most **280 code units**. Markup/definitions or summaries beyond the prefix are deliberately not inspected; e.g. a long code block's later explanation may not enter its preview. Full stored messages and full-chat rendering are unchanged.
- Cache entries use **message ID plus the bounded prefix**, not the whole reply or object identity. At most **2,048 messages** retain a copied small prefix and result. Changed prefixes invalidate the entry; fresh SSE objects and changes only outside the prefix reuse it. The 120-row history case no longer thrashes a 100-item/source-size cache.
- A deterministic container-depth guard runs before Markdown. Parser/converter exceptions produce generic `메시지` rather than preventing the sidebar/history from opening. Delimiters are tokenized in forward scans rather than repeatedly searching an unclosed suffix.
- The preview no longer statically imports `remark-math`; the production build emits it as a separate lazy chunk again. KaTeX/Mermaid rendering remains unchanged and lazy. The existing large-chunk advisory remains, but the former ineffective dynamic-import warning is gone.

## Performance observations

Performance **Before** here is the previous full-parsing PR source `f023215c0312202a4b86e872adbe9f6f451f377f` (same source tree as the earlier `147d924` head, with reviewer credit added), not the original raw-text visual baseline above. **After** is the bounded implementation identified above.

Node v24.18.0, Linux, isolated child processes with a 12-second deadline and 384 MiB heap. Each case has five short warmups followed by five distinct source/ID cache misses; medians are illustrative observations, **not CI speed thresholds**. The ASCII Markdown fixture repeats `## Analysis`, a bold paragraph/link and two list items to the stated size; malformed fixtures repeat `\(x ` or `\[x `. Raw samples and browser measurements are in [performance.json](<performance.json>).

| ASCII body length | Previous helper median | Bounded helper median |
| --- | ---: | ---: |
| 15,000: mixed Markdown | 41.736 ms | 6.035 ms |
| 77,000: mixed Markdown | 207.993 ms | 9.308 ms |
| 192,000: mixed Markdown | 991.408 ms | 7.056 ms |
| 60,000: unclosed inline TeX | 197.048 ms | 3.738 ms |
| 60,000: unclosed display TeX | 191.126 ms | 3.064 ms |

A separate 120 × 15 KB stable-ID traversal measured **2,923 / 2,218 / 2,307 ms** across three passes before, versus **260 / 0.078 / 0.045 ms** in the bounded working-tree prototype. The final deterministic regression checks **zero parser calls** on all 120 reloaded unchanged IDs, including messages larger than the old cache cutoff.

The actual Chrome History test seeds 120 conversations, including a >200 KB reply, 4,096 nested quote markers and 60 KB of unclosed delimiters. It opens History three times, refreshes real SSE snapshots while all rows are open, types in the composer and verifies complete stored bodies. Opening-to-second-frame measurements were **364 / 34 / 34 ms**. The first cold parse/render still takes multiple frames; the claim is bounded work and eliminated repeated parse cost, not instantaneous first opening. [History capture](<history.png>).

## Reproduce and verification

```sh
npx tsx --test tests/preview-text.test.ts tests/preview-performance.test.ts
npx tsx --test --test-concurrency=1 tests/*.test.ts
npm run build
npx playwright test tests/sidebar-previews.spec.ts tests/markdown-math.spec.ts tests/conversation-history.spec.ts
```

- [Conversion/state cases](<../../../tests/preview-text.test.ts>): **52 passed**, including code-closer boundaries, nonempty HTML copy, math/currency/quote boundaries, link/image summaries and preservation of stored messages.
- [Resource contracts](<../../../tests/preview-performance.test.ts>): **9 passed**. A spy on the actual parser proves all input sizes bounded, no parsing for 120 unchanged cloned messages, prefix invalidation, large-message caching, eventual eviction, preparse deep-container rejection and recovery from a forced parser exception. Unclosed-delimiter tests run in an isolated process with a generous timeout rather than a fragile millisecond assertion.
- Full Node suite: **166 passed, zero skipped**, with serial test-file execution. Earlier concurrent runs on the initial implementation hit unrelated routine-capacity/engine-history timing assertions; no runtime/test fixes outside this ticket were made to hide them.
- Strict TypeScript / production build: passed.
- [Sidebar/history Chrome scenarios](<../../../tests/sidebar-previews.spec.ts>): **2 passed**, covering exact copy, reload, original message preservation, full-chat bold rendering, mobile drawer, 120-history rows, real SSE refresh and composer input.
- Existing conversation-history and desktop/mobile math/Mermaid Chrome regressions: **3 passed**.

This change does not typeset TeX commands, add a new Markdown dialect, or render raw HTML. Korean summary labels are the requested copy; no localization system or layout redesign is introduced.
