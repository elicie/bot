# Before: current-main comparison

- Baseline revision: `c07a1cfec41e84ab86e566e7c72e3684dae1c31c` (detached, unchanged application source).
- Captured with the existing Node test backend and pinned engine; no application or test compatibility changes.
- The comparison spec was copied temporarily from this PR. It is the same completed-answer scenario as After: dark theme, blob avatar, fixed article origin `http://127.0.0.1:43128`, unhovered pointer, desktop 1040 × 760 / mobile 390 × 844. General settings is scrolled to the bottom.

```sh
# From the detached baseline worktree; dependencies and the pinned engine are prepared.
cp ../repo/tests/link-previews.spec.ts tests/link-previews.spec.ts
npm run build
LINK_PREVIEW_BASELINE=1 LINK_PREVIEW_FIXTURE_PORT=43128 \
  GROK_BOT_LINK_PREVIEW_EVIDENCE_DIR=../repo/docs/evidence/issue-18-main-refresh \
  npx playwright test tests/link-previews.spec.ts --grep 'visual comparison'
```

The frontend build/typecheck passed. Both browser scenarios passed (2/2, 9.2 seconds), including no page errors, no external browser requests, and no horizontal overflow. All four images were opened and visually checked; they are real application captures, not mockups.

| Scenario | Capture |
| --- | --- |
| Desktop answer, ordinary link without card | [Before desktop](before-1040.png) |
| Mobile answer, ordinary link without card | [Before mobile](before-390.png) |
| Desktop General settings, no preview switch | [Before desktop settings](before-settings-1040.png) |
| Mobile General settings, no preview switch | [Before mobile settings](before-settings-390.png) |

The baseline worktree's application diff remains empty. This capture does not replace or modify the original comparison in the [initial evidence record](../issue-18/README.md).
