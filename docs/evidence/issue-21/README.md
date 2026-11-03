# P4: standalone release build guidance

Actual application captures, not mockups. These are revision-bound P4 evidence from before the rebase onto `origin/main` at `3c11131`, not refreshed captures of the current review head. Both sides use a real Rust `grok-bot` server, a fresh temporary data directory with the default Grok bot, an available engine, Chrome, the dark theme, the **Settings → Updates** panel, and a **1040 × 760** viewport. No credentials, saved personal data, or real provider requests are present.

- **Before:** baseline `95ffc46` (`main` at ticket checkout). Baseline Rust server and frontend were built directly from that revision before any UI edits. The panel still advertised `build:engine` and a Node-driven restart.
- **After:** original implementation revision `030794a6c87811e11b2d8dfb223a07e5e209f3a4` (`feat: complete Rust-only standalone release packaging`) on branch `rust-release-cleanup-3adb303fe5c0`, before rebase. The panel advertises the bundled release build and executable, not the removed engine command. The comparison remains evidence of P4's build-help copy change; the upstream MCP form/Vault UI merged through PR #30 is not captured here.

| Scenario | Before | After |
| --- | --- | --- |
| Same desktop settings state | [Before desktop](<before-desktop.png>) | [After desktop](<after-desktop.png>) |
| Same update section | [Before update guidance](<before-updates.png>) | [After update guidance](<after-updates.png>) |

The [optimized-release recapture](<after-release-desktop.png>) repeated the desktop scenario with only the earlier 181 MiB release executable copied into a new temporary directory, no companion frontend/engine, and no executable on the server's PATH. Chrome rendered the embedded UI with zero page or HTTP errors; its screenshot is byte-identical to the debug After image above. That size and runtime check belong to the earlier artifact, not the rebased release build.

Additional After captures verify [390 px](<after-390.png>) and [320 px](<after-320.png>) layouts with no page errors or document-level horizontal overflow. The panel retains its existing scroll container so the instructions remain reachable on small screens. The full acceptance suite also exercises the existing mobile settings behavior.

The P4 UI change shown here is limited to build/recovery guidance; styling and layout are unchanged. During review, the current rebased frontend was opened in real Chrome at the same dark 1040 × 760 Settings → Updates state: the update-guidance region was byte-identical to the recorded After image. Full-window hashes vary with the fresh default bot's generated avatar, not the guidance. Current-head verification is tracked separately in [the verification record](<../../verification.md#rebased-review-verification>).
