# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Product purpose

A local-first web application built directly on Grok Build's source agent runtime. The user configures an OpenAI-compatible model endpoint, creates named bots, chats with them, approves bot-to-bot messages, schedules local routines, edits server-local files, and starts an isolated managed Chrome for public-site browsing.

## Users and operating context

The confirmed initial user runs this repository on their local computer and opens its browser interface. The local backend owns agent processes and file access. A browser on another computer edits files on the backend computer. Remote hosting and multi-user access are outside this implementation.

## Capabilities and constraints

- Build an app-owned Rust engine directly linked to pinned Grok Build source for the agent loop, model sampling, tool dispatch, and session continuity. Use ACP as its private transport and keep file approvals in the app.
- Configure an OpenAI-compatible base URL, model ID, and optional API key.
- Select a local workspace and browse, read, and explicitly save text files there.
- Display agent responses, tool activity, permission requests, cancellation, and failures.
- Persist bots, bot-scoped private instruction skills, conversations, routines, execution history, settings, and session identifiers locally.
- Load enabled skill instructions through the authenticated app broker and review complete model-created skills before one-use save approval. External connector accounts, installation, publishing, and executable workflows remain separate implementation gaps.
- Approve durable bot-to-bot delivery once, execute the recipient's identity in a chat-only conversation, and open a read-only pair transcript.
- Start an app-owned sandboxed Chrome with a public-origin allowlist through a loopback connection. Model observations and actions require a fresh local approval.
- Create, edit, pause, and manually run routines with one-time, interval, and daily timezone schedules.
- Keep model-only chat usable before connecting a workspace.
- Bind to loopback by default, support explicit trusted-network binding, and validate Host and Origin.
- Use no Docker commands or automatic container provisioning.
- Store provider credentials on the backend. Never return stored API keys to the browser.

## Brand commitments

The user explicitly requests the same UI as Grok Bot. Follow the live 0.78.1 accessibility observations and measured geometry in `docs/grok-bot-ui-audit.md`. The older 0.18 settings screenshot is secondary visual evidence. Implement the interface independently using system fonts, original geometric avatars, and open-source icons. Original shipped renderer bundles, fonts, avatars, and brand assets are excluded.

## Evidence on hand

- Live Grok Bot 0.78.1 window geometry, accessibility controls, authorized window screenshots, and read-only navigation on the Mac.
- Local Grok Bot 0.18 reconstructed source and the Router settings screenshot as historical evidence.
- Official Grok Build ACP and custom-model documentation.
- The implemented local application and repeatable backend and Chromium tests.

## Product principles

- Make local file mutations visible and intentional.
- Preserve the user's configured provider and conversation context.
- Represent connection, execution, and approval states explicitly.
- Test the actual browser interaction and disk contents.

## Open decisions

The user granted Mac screen recording and the observed window states have been visually compared. Exact pixel equality remains unverified across fonts, independent avatars, local feature extensions, and unobserved states. Cloud computers, system-wide desktop control, encrypted credential vaults, cloud billing, recurring cloud automations, and hosted multi-user deployment remain separate capabilities. Managed Chrome requires a working host sandbox and fails closed when it cannot verify one.
