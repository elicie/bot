# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Product purpose

A local-first web client for Grok Build. The user configures an OpenAI-compatible model endpoint, creates named bots, chats with them, schedules local routines, and edits files on the computer running the local server.

## Users and operating context

The confirmed initial user runs this repository on their local computer and opens its browser interface. The local backend owns agent processes and file access. A browser on another computer edits files on the backend computer. Remote hosting and multi-user access are outside this implementation.

## Capabilities and constraints

- Use Grok Build through ACP for the agent loop, tool execution, approvals, and session continuity.
- Configure an OpenAI-compatible base URL, model ID, and optional API key.
- Select a local workspace and browse, read, and explicitly save text files there.
- Display agent responses, tool activity, permission requests, cancellation, and failures.
- Persist bots, conversations, routines, execution history, settings, and session identifiers locally.
- Create, edit, pause, and manually run routines with one-time, interval, and daily timezone schedules.
- Keep model-only chat usable before connecting a workspace.
- Bind to loopback by default, support explicit trusted-network binding, and validate Host and Origin.
- Use no Docker commands or automatic container provisioning.
- Store provider credentials on the backend. Never return stored API keys to the browser.

## Brand commitments

The user explicitly requests the same UI as Grok Bot. Follow the observed desktop layout and dark visual language from `../grok-bot-0.18-reconstructed/docs/assets/router-settings.png` and the reference's conversation/settings layouts. Implement the interface independently. Original shipped renderer bundles, fonts, avatars, and brand assets have unresolved redistribution rights and are excluded.

## Evidence on hand

- Local Grok Bot 0.18 reconstructed source and the Router settings screenshot.
- Official Grok Build ACP and custom-model documentation.
- The implemented local application and repeatable backend and Chromium tests.

## Product principles

- Make local file mutations visible and intentional.
- Preserve the user's configured provider and conversation context.
- Represent connection, execution, and approval states explicitly.
- Test the actual browser interaction and disk contents.

## Open decisions

Exact pixel parity with the complete original application requires runnable original screens and a wider baseline set. Cloud computers, system-wide desktop control, recurring cloud automations, and hosted multi-user deployment are separate capabilities.
