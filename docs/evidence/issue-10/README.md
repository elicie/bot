# Issue #10: plain-text conversation previews

## Before / After

Both images are actual Chrome application captures with the same synthetic persisted conversations, viewport **1280 × 900**, sidebar width **400 px**, **ko-KR** locale, **dark** theme and explicit blob avatars. The selected main bot has no messages; the six ordinary bot rows exercise math, Markdown, code, visualization, a generated Markdown image, and a bare generated-image link. No personal conversations, credentials, or external model calls are involved.

- **Before source:** `95ffc464e8d55c1a315fddb31101fadb9c9c5b7b`, in a detached baseline worktree with no production edits. Only the same new test scenario was copied in for the capture; its plain-text assertion failed on the raw math preview as expected.
- **After implementation:** `cea224038f08dd8c0d9427a883883b85b2d39a74`. The later evidence commit changes documentation/images only.
- Both captures use the Node application backend and the same version-checked pinned engine dependency (`0.1.0`, source `2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8`). The scenario reads seeded messages, not generated model responses.

| Before | After |
| --- | --- |
| ![Before: raw Markdown, TeX and media paths](<before.png>) | ![After: plain text and short media labels](<after.png>) |

## Behavior and verification

- `\(23 = 20 + 3\)으로 나누어 계산...` becomes `23 = 20 + 3으로 나누어 계산...`.
- Markdown structure is parsed, keeping visible text and link labels; code blocks become `코드`, and the existing Mermaid/HTML/SVG/visualization fence types become `차트`. Generated image destinations become `이미지`, including inline, reference and bare links.
- Math delimiters (`\\(` / `\\)`, `\\[` / `\\]`, `$` / `$$`) are removed without interpreting TeX commands as Markdown. Inline-code backticks are removed but their contents stay literal. Raw HTML tags/blocks are skipped rather than rendered; ordinary escaped symbols/currency are retained. Relative/absolute links with an `/api/generated-images/…` destination become `이미지`; other text links retain their labels. Other Markdown images also become `이미지`.
- The latest nonempty, non-report message is selected; active/error states still take precedence. Whitespace collapses to one space and trims at the ends. Existing CSS handles visual clipping; no new source-text truncation or localization system is introduced (the labels are the requested Korean copy).
- Existing active/error/empty preview states and hidden report-input filtering remain intact. Original stored messages and full-chat Markdown rendering are unchanged. Shared conversation preview consumers (history and group rows) use the same conversion.
- A bounded source-text cache avoids reparsing unchanged messages on typing/SSE rerenders.

## Reproduce

With the pinned engine installed as described in the project setup, run:

```sh
npx tsx --test tests/preview-text.test.ts
npx tsx --test --test-concurrency=1 tests/*.test.ts
npm run build
npx playwright test tests/sidebar-previews.spec.ts tests/markdown-math.spec.ts tests/conversation-history.spec.ts
```

The [sidebar scenario](<../../../tests/sidebar-previews.spec.ts>) creates and cleans its own disposable state and records `sidebar-previews.png` through Playwright. Its default viewport capture is the comparison above; the same test subsequently checks the 390 px mobile drawer. The [unit cases](<../../../tests/preview-text.test.ts>) specify exact conversion outputs.

Checks on the implementation revision:

- Preview conversion/state unit regressions: 47 passed, including adjacent/multiline/quoted math, comparison operators, literal math examples, CJK emphasis, nested/reference links and media summaries.
- Full Node suite: 152 passed, zero skipped, with serial test-file execution. Default concurrent runs were also exercised; one hit the unchanged 64 MiB routine-capacity fixture's 5-second wait and another hit the unchanged engine tool-history assertion. The capacity test passed in isolation; no unrelated runtime or test code was changed.
- Strict TypeScript and production build: passed.
- New sidebar Chrome e2e: passed; checks exact preview text, reload, unchanged persisted message text, preserved full-chat bold rendering and mobile drawer previews.
- Existing conversation-history and desktop/mobile math/Mermaid Chrome regressions: 3 passed.
- Independent source review: actionable math-normalization and repeated-render performance findings fixed and rechecked.

This change does not convert TeX commands into typographic math or add new Markdown dialects; it produces a readable one-line preview only. The production build retains its large-chunk advisory and notes that the existing math parser is now also used synchronously for previews; KaTeX/Mermaid rendering remains lazy and its browser regressions pass.
