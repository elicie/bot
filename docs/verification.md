# Verification

Verified on Linux with Node.js 24.18.0, Grok Build 1.0.62, ACP SDK 2.2.0, and Google Chrome through Playwright.

## Automated checks

- `npm run build` passed. It includes strict TypeScript checks and a Vite production build.
- `npm test` passed 57 tests with zero skips, including the installed Grok Build integration tests.
- `npm run test:e2e` passed five browser scenarios.
- Production HTTP probes returned 200 for `/`, `/api/state`, and an application fallback route using loopback, `devbox`, and the configured Tailscale hostname. A foreign Origin received 403.
- The updated production server listens on `0.0.0.0:3000`. A live Chrome pass exercised desktop/mobile routine navigation, the daily form, confirmed cancellation, and return to chat without saving test data. Public values for the existing bot, two conversations, and settings matched their pre-restart fingerprints after accounting for schema key order.
- The Impeccable source detector returned no findings. Independent source and browser-artifact reviews identified issues that were fixed and retested.

## Browser behavior

The browser tests start the actual application and installed Grok Build executable. A deterministic local OpenAI-compatible server supplies model responses and tool calls. Tests use disposable local files and read the resulting bytes directly.

The tested flows cover provider configuration and model discovery, server-only saved credentials, workspace selection, nested and empty folders, binary-file errors, direct editor saves, external-editor conflicts, AI write approval and denial, clean-editor refresh after AI work, new bots, history restoration, bot search, mobile drawer focus and Escape, mobile file editing layout, cancellation, and disconnected-provider errors. The first flow also checks browser page errors and horizontal overflow.

The routine browser scenarios additionally cover bot creation and model-only chat on plain network HTTP, interval creation, daily timezone editing, pause/resume, manual and automatic scheduled execution, frozen results after follow-up chat, history navigation, reload, deletion with retained history, new-chat draft preservation across bot switches, queued cancellation without stopping the active chat, input-error retention, and desktop/mobile layout. The hostname fixture uses `http://grok-bot.test` mapped to loopback in Chrome, with `window.isSecureContext === false` and `crypto.randomUUID` unavailable. It exercises the backend and Vite hostname policies without a test-only Vite bypass.

Deterministic routine tests cover migration, invalid persisted references, due boundaries, duplicate manual/scheduled commands, recurring coalescing, DST gaps and repeated hours, recovery of queued and interrupted work, pause/delete guards, frozen results, failed persistence, shutdown during claims, capacity admission, and continued dispatch of accepted work. HTTP tests verify queued-prompt protection, queued cancellation, secret redaction, retained history, and startup/shutdown integration.

## Runtime and file boundaries

Tests cover path traversal, symlink parents, symlink files, hard-linked files, size and UTF-8 limits, optimistic content hashes, one-use approvals, stale permission decisions, interrupted startup recovery, session continuity, replay deduplication, inactive-provider termination, approval waits, and server-parent death. Real Grok tests verify that only MCP discovery and invocation appear in the agent tool catalog, forbidden shell and native readers fail, and file-backed argument paths cannot bypass the broker.

Model-only chat tests verify that HTTP and MCP file access stays unavailable until a workspace is connected. A byte-capacity HTTP regression verifies admission refusal before mutation and accepted prompt dispatch through the actual runtime startup method near the 64 MiB limit.

Additional regressions verify streamed credential-prefix masking in every emitted snapshot, opaque public tool identifiers, complete review of limited write proposals, late-approval rejection, persisted state revisions, committed-state publication, and delayed HTTP snapshot rejection.

## Review process

The pstack poteto workflow separated backend and frontend ownership behind a shared typed contract. Two independent design sketches and separate UI/runtime reviews shaped the result. Independent routine reviews found queued-prompt replacement, mutable history, capacity-dispatch, and hidden-draft issues; regression tests now cover the fixes. All reviewers used the session's inherited model. Cross-model review was unavailable under the session's model-selection constraints. The comment review retained one explanation of the scheduler dependency's DST behavior and found zero suppression directives.

## Limits

- The UI follows the observed Grok Bot 0.18 screenshot, measurements, and interaction structure. Complete-product pixel equality is unverified. Original renderer bundles, fonts, and image assets are excluded.
- Provider compatibility was exercised against a local OpenAI Chat Completions fixture. The user's actual provider and credentials were not supplied or tested.
- The runtime currently requires Grok Build 1.0.62. Other versions fail closed until the tool boundary is reverified.
- File execution occurs on the backend computer. The server defaults to loopback and supports explicit `0.0.0.0` binding for a trusted network. It targets one local user and has no login.
- Direct text editing supports files up to 1 MiB. AI write proposals are limited to 160 KiB for complete review.
- This implementation provides application-level file confinement. Other programs running as the same OS user remain outside that boundary.
- Local routine scheduling requires the server to remain running. Started work becomes interrupted after restart; durable queued work resumes. Limits retain 1,000 routines, 10,000 execution records, and 64 MiB of state without automatic history pruning.
- Windows and macOS behavior has not been verified. Native desktop control, cloud computers, hosted multi-user access, groups, and cloud-hosted scheduling are outside this build.

Temporary screenshots, traces, providers, workspaces, and test servers were used for verification and are cleaned after the run. Run the test commands to regenerate evidence.
