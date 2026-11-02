# P4: standalone release build guidance

Actual application captures, not mockups. Both sides use a real Rust `grok-bot` server, a fresh temporary data directory with the default Grok bot, an available engine, Chrome, the dark theme, the **Settings → Updates** panel, and a **1040 × 760** viewport. No credentials, saved personal data, or real provider requests are present.

- **Before:** baseline `95ffc46` (`main` at ticket checkout). Baseline Rust server and frontend were built directly from that revision before any UI edits. The panel still advertised `build:engine` and a Node-driven restart.
- **After:** the implementation commit titled **`feat: complete Rust-only standalone release packaging`** on branch `rust-release-cleanup-3adb303fe5c0` (resolve its hash in this PR's commit list). The panel advertises the bundled release build and executable, not the removed engine command. Later documentation-only or test-only commits do not alter this captured UI.

| Scenario | Before | After |
| --- | --- | --- |
| Same desktop settings state | [Before desktop](before-desktop.png) | [After desktop](after-desktop.png) |
| Same update section | [Before update guidance](before-updates.png) | [After update guidance](after-updates.png) |

Additional After captures verify [390 px](after-390.png) and [320 px](after-320.png) layouts with no page errors or document-level horizontal overflow. The panel retains its existing scroll container so the instructions remain reachable on small screens. The full acceptance suite also exercises the existing mobile settings behavior.

The change is limited to truthful build/recovery guidance; styling and layout are unchanged.
