# Verification

Verified on Linux with Node.js 24.18.0, Grok Build 1.0.62, ACP SDK 2.2.0, and Google Chrome through Playwright.

## Automated checks

- `npm run build` passed. It includes strict TypeScript checks and a Vite production build.
- `npm test` passed 23 tests with zero skips.
- `npm run test:e2e` passed two browser scenarios.
- Production HTTP probes returned 200 for `/`, `/api/state`, and an application fallback route. A foreign Origin received 403.
- The Impeccable source detector returned no findings. Independent source and browser-artifact reviews identified issues that were fixed and retested.

## Browser behavior

The browser tests start the actual application and installed Grok Build executable. A deterministic local OpenAI-compatible server supplies model responses and tool calls. Tests use disposable local files and read the resulting bytes directly.

The tested flows cover provider configuration and model discovery, server-only saved credentials, workspace selection, nested and empty folders, binary-file errors, direct editor saves, external-editor conflicts, AI write approval and denial, clean-editor refresh after AI work, new bots, history restoration, bot search, mobile drawer focus and Escape, mobile file editing layout, cancellation, and disconnected-provider errors. The first flow also checks browser page errors and horizontal overflow.

## Runtime and file boundaries

Tests cover path traversal, symlink parents, symlink files, hard-linked files, size and UTF-8 limits, optimistic content hashes, one-use approvals, stale permission decisions, interrupted startup recovery, session continuity, replay deduplication, inactive-provider termination, approval waits, and server-parent death. Real Grok tests verify that only MCP discovery and invocation appear in the agent tool catalog, forbidden shell and native readers fail, and file-backed argument paths cannot bypass the broker.

Additional regressions verify streamed credential-prefix masking in every emitted snapshot, opaque public tool identifiers, complete review of limited write proposals, late-approval rejection, persisted state revisions, committed-state publication, and delayed HTTP snapshot rejection.

## Review process

The pstack poteto workflow separated backend and frontend ownership behind a shared typed contract. Two independent design sketches and separate UI/runtime reviews shaped the result. All reviewers used the session's inherited model. Cross-model review was unavailable under the session's model-selection constraints. The comment review found zero comments and zero suppression directives in authored code.

## Limits

- The UI follows the observed Grok Bot 0.18 screenshot, measurements, and interaction structure. Complete-product pixel equality is unverified. Original renderer bundles, fonts, and image assets are excluded.
- Provider compatibility was exercised against a local OpenAI Chat Completions fixture. The user's actual provider and credentials were not supplied or tested.
- The runtime currently requires Grok Build 1.0.62. Other versions fail closed until the tool boundary is reverified.
- File execution occurs on the backend computer. The server binds to loopback and targets one local user.
- Direct text editing supports files up to 1 MiB. AI write proposals are limited to 160 KiB for complete review.
- This implementation provides application-level file confinement. Other programs running as the same OS user remain outside that boundary.
- Windows and macOS behavior has not been verified. Native desktop control, cloud computers, hosted multi-user access, groups, and cloud routines are outside this build.

Temporary screenshots, traces, providers, workspaces, and test servers were used for verification and are cleaned after the run. Run the test commands to regenerate evidence.
