# Local Grok Bot architecture

## Understood as

Build a browser application with the Grok Bot desktop layout, configurable OpenAI-compatible inference through Grok Build, and intentional editing of local files on the backend computer. Implement the renderer independently and verify real browser behavior and disk changes.

## Data ownership

`shared/types.ts` defines public bots, conversations, messages, routines, routine executions, tool activities, pending permissions, runtime state, settings, and text documents. The backend is the single writer of durable application state. Grok Build owns ACP execution sessions. The application stores session IDs and a display transcript. API keys stay in the backend data directory and subprocess environment.

## Runtime

One local HTTP server serves both the React frontend and the API. The default bind address is `127.0.0.1`; explicit `HOST=0.0.0.0` enables trusted-network access. Express and Vite share the explicit hostname allowlist, and the backend validates Host, port, and Origin. The application has no login. SSE carries public state snapshots with a persisted monotonic revision. Changes publish after their atomic state write succeeds. The frontend rejects snapshots older than its current revision. HTTP commands mutate settings, bots, conversations, approvals, and workspace documents. The active run states are `starting`, `running`, and `awaiting_permission`. Terminal states are `completed`, `cancelled`, `error`, and `interrupted`.

`engine/src/main.rs` directly links the `xai-grok-shell` library from pinned Grok Build source. It builds an application-specific configuration and calls the upstream `run_stdio_agent` entry point. `npm run build:engine` produces `.engine/grok-bot-engine`; `server/engine.ts` checks its application version and source identity before enabling execution. The runtime adapter starts this absolute application artifact per run. ACP (Agent Client Protocol) is the internal transport between Node and Rust. The app owns the entry point and process lifecycle; Grok Build supplies the agent loop, model sampler, tool dispatch, and saved execution sessions. The upstream CLI composition root is outside this build.

When configuration and engine source match, the runtime loads the conversation's saved ACP session. A changed fingerprint starts a new session and passes up to 64 KiB of recent visible user/assistant history as quoted conversation context. Existing messages and original execution files stay intact. Private engine homes isolate configuration, credentials, and state from the user's other Grok installations. The selected custom model uses `chat_completions`. A curated ACP agent profile exposes only MCP (Model Context Protocol) discovery and invocation. An app-owned HTTP MCP server performs workspace operations. Every MCP write waits for a one-use browser approval. Cancellation settles pending permissions and ends the child process. Automatic retries are disabled so provider failures reach the UI promptly. The backend enforces a 30-second inactivity deadline, pauses it while a user approval is pending, and resets it on actual ACP updates. A child supervisor watches the parent IPC connection and terminates the engine process group after backend death. Source provenance, licensing, and build gates are recorded in [docs/source-engine.md](docs/source-engine.md).

## Private skills

Each bot optionally owns persisted instruction skills with a generated ID, name, description, complete Markdown instructions, enabled state, and optimistic revision. User mutations pass through the existing command queue and idle guard. Model `create_skill` proposals require a complete one-use review; denial or cancellation writes nothing. Authenticated broker `list_skills` and `read_skill` infer ownership from the run, enforce enabled state, and deny private-skill access in recipient bot-delivery conversations. Skill content grants no new tools or permissions. Enabled-content fingerprints replace execution context after changes while retaining visible chat history.

The app uses instruction-only records and a strict SKILL.md text importer. Native filesystem discovery, native Skill/read aliases, plugin extension forwarding, shell tools, and executable Rhai workflows remain disabled. The source audit and remaining connector/publishing gaps are recorded in [docs/skills-gap-audit.md](docs/skills-gap-audit.md).

## Routines

A routine selects one bot, a prompt, and a one-time, interval, or daily IANA-timezone schedule. The app-owned scheduler shares the serialized HTTP command queue. It persists each conversation, immutable execution metadata, and schedule advance atomically before starting the existing runtime. Only one execution is active globally. Each routine admits one queued or active execution, and missed recurring times coalesce.

`RoutineRun.executionRunId` binds history to the original execution; terminal `RoutineRun.result` survives follow-up messages in the same conversation. Startup recovery interrupts started work without replay and resumes durable queued claims. Pausing keeps accepted work, and deletion keeps execution history and conversations. See [docs/routines.md](docs/routines.md) for the scheduling and recovery contract.

## Bot messages

`BotExchange` stores a two-bot transcript and `BotDelivery` stores its durable, idempotent execution receipt. The authenticated broker exposes bot discovery and an outbound message proposal. Approval records the full recipient/text before creating the recipient's dedicated conversation and queue entry atomically. A single source run can deliver one distinct message. Each further hop needs fresh approval and chains stop at four deliveries.

The recipient runs under its own identity with file and computer tools denied. Its one terminal final answer is projected once into the exchange. Dedicated recipient conversations reject user-prompt hijacking. The routine dispatcher admits both routine claims and bot deliveries in FIFO order under the same global execution slot. Queued deliveries survive startup; started deliveries become interrupted without resend. Pair transcripts use query routing and preserve the originating chat draft.

## Managed computer

`shared/computer.ts` defines a typed 960 × 600 browser surface. `ManagedComputer` owns a fresh private Chrome, an ephemeral profile, a mandatory verified sandbox, an authenticated loopback proxy, and a public-origin allowlist. Request UUIDs are one-use; epoch and frame UUIDs prevent stale actions. Public-address validation covers all DNS answers, and the proxy pins the selected validated address. HTTP methods, credentials, sensitive controls, downloads/uploads, workers, WebSockets, WebRTC, and native OS actions are restricted. Browser sandbox verification fails closed.

`ApplicationComputer` associates the single browser with a bot, caches local PNG frames, and adds cancellation generations so Stop invalidates pending work across both command queues. HTTP starts/actions share model-run admission. Stop bypasses that queue to interrupt slow navigation. Computer HTTP routes and model permission decisions require a loopback socket; forwarded-address headers grant no trust. LAN snapshots and SSE replace local-only approval/tool details with a content-free stub. Cached PNGs bind to the exact epoch/frame, and coordinate clicks stay disabled until that image loads. Cancellation stops only the browser epoch used by the run. A reverse proxy requires separate authentication and remains outside the deployment contract. Model computer tools operate only on a user-started browser and request one-use local approval for every observation/action. The provider receives redacted URL/title/control text and coordinates; the browser preview uses PNGs locally.

## Read state, deletion, attachments, and rich text

Read state is server-side: `Bot.unread` in `state.json`, shared by every browser and kept across reloads and restarts. A run that finishes as `completed` or `error` marks its bot unread in the same commit; cancellation does not. The browser showing that bot (chat view, visible tab) marks it read again when the answer arrives or when the bot is opened. A manual "읽지 않음으로 표시" stays until the bot is opened again. `Bot.notifyOnUpdates` is per bot on the server; each browser shows a `Notification` only when it holds permission in a secure context (HTTPS or localhost; LAN HTTP falls back silently with a notice). The message sound is a per-browser preference (`localStorage`), synthesized with Web Audio. Read-state and notification changes are allowed during runs.

Deleting a bot requires an idle application (no active run), no queued routine run of the bot or its routines, no queued or running bot message in its delivery graph, and at least one other bot. One commit removes the bot with its skills, its conversations, its routines and their routine history, every delivery and exchange it sent or received plus the deliveries chained from those, the read-only recipient conversations those deliveries created for other bots, and links from surviving messages to removed exchanges. The candidate state must pass the same reference validation as `state.json` loading before it is committed. Afterwards the server removes the deleted conversations' attachment blobs and private engine session directories (`<data>/attachments/<id>`, `<data>/grok/<id>`). Engine-internal session records under the shared `GROK_HOME` are not pruned.

Attachments are sent with `POST /api/conversations/:id/attachment-messages` (base64 JSON, body limit raised for this route only). Limits: 8 files, text 256 KB each and 512 KB together, images (PNG, JPEG, GIF, WebP by signature) 5 MB each, 10 MB per message. `state.json` keeps only metadata on the user message; bytes are owner-only blobs under `<data>/attachments/<conversation>/<attachment>`, served back as `text/plain` or the image type under a sandboxing CSP. Text files reach the engine inside the text prompt as a `<user_attachments>` section of JSON lines whose string values escape `@`, `[`, `:`, and `<`, and the combined text still passes `literal_engine_prompt`, so attachment contents cannot trigger upstream `@path` expansion, `data:` image extraction, or slash commands. Images become ACP image blocks; the pinned Grok Build accepts them although it does not advertise `promptCapabilities.image`, normalizes them, and sends them as Chat Completions `image_url` data URLs (pinned by `crates/core/tests/attachments.rs`). Replacement sessions replay only attachment names.

Assistant Markdown renders KaTeX math (`$…$`, `$$…$$`, `trust: false`) and Mermaid fences (`securityLevel: 'strict'`, shown as an `<img>` of the generated SVG). Both libraries load on first use; formulas that fail show their source, and diagrams that fail show their code. Raw HTML stays skipped.

## File access

Chat needs a configured provider and model. File access additionally needs an explicitly selected workspace. HTTP and MCP reads and writes use the same confined file service and reject symbolic links, hard links, traversal, binary documents, and oversized files. Linux opens parent directories through pinned descriptors and uses no-follow flags. Explicit Save uses a content hash to detect external changes. The agent runs in an app-private working directory and accesses the user workspace through the MCP broker. Native file tools, shell execution, subagents, and external tools are disabled. The profile declares its exact discovery/invocation tools and disables ambient instruction-file discovery. Attachment-shaped text and leading slash commands are delegated as a literal JSON string so upstream's pre-tool parser cannot expand local paths or dispatch native commands. The visible transcript preserves the original text. This is application-level confinement. It does not claim an OS sandbox against other programs running as the same user.

## HTTP contract

All JSON mutations send `Content-Type: application/json` and `X-Grok-Bot: 1`. The browser uses the same origin for API requests.

| Method | Path | Request | Response |
| --- | --- | --- | --- |
| GET | `/api/state` | | `AppState` |
| GET | `/api/events` | | SSE `state` events with `AppState` |
| PUT | `/api/settings` | `SettingsInput` | `AppState` |
| POST | `/api/provider/test` | `{ baseUrl?, apiKey? }` | `ProviderCheck` |
| POST | `/api/bots` | `{ name, color?, instructions?, shape?, pinned? }` | `AppState` |
| PATCH | `/api/bots/:id` | `{ name?, color?, instructions?, shape?, pinned?, hidden? }` | `AppState` |
| DELETE | `/api/bots/:id` | `{}` | `AppState` |
| POST | `/api/bots/:id/unread` | `{ unread }` | `AppState` |
| POST | `/api/bots/:id/notify` | `{ enabled }` | `AppState` |
| POST | `/api/bots/:botId/skills` | `{ name, description, instructions }` | `AppState`, status 201 |
| POST | `/api/bots/:botId/skills/import` | `{ markdown }` | `AppState`, status 201 |
| PATCH | `/api/bots/:botId/skills/:id` | `{ revision, name?, description?, instructions?, enabled? }` | `AppState` |
| DELETE | `/api/bots/:botId/skills/:id` | `{ revision }` | `AppState` |
| POST | `/api/routines` | `RoutineInput` | `AppState`, status 201 |
| PUT | `/api/routines/:id` | `RoutineInput` | `AppState` |
| PATCH | `/api/routines/:id` | `{ enabled }` | `AppState` |
| DELETE | `/api/routines/:id` | `{}` | `AppState` |
| POST | `/api/routines/:id/run` | `{}` | `{ run, state }`, status 202 |
| POST | `/api/conversations` | `{ botId }` | `{ conversation, state }` |
| POST | `/api/conversations/:id/messages` | `{ text }` | `AppState`, status 202 |
| POST | `/api/conversations/:id/attachment-messages` | `{ text, attachments: [{ name, mimeType, data }] }` (body up to about 15 MiB) | `AppState`, status 202 |
| GET | `/api/conversations/:id/attachments/:attachmentId` | | stored bytes (`text/plain` or image) |
| POST | `/api/conversations/:id/cancel` | `{}` | `AppState` |
| POST | `/api/conversations/:id/permissions/:permissionId` | `{ optionId }` | `AppState` |
| GET | `/api/files?path=` | workspace-relative directory | `FileListing` |
| GET | `/api/file?path=` | workspace-relative file | `FileDocument` |
| PUT | `/api/file` | `{ path, content, hash }` | `FileDocument` |
| GET | `/api/computer?owner=` | bot UUID, loopback only | `ComputerStatus` |
| POST | `/api/computer` | `{ ownerId, requestId, userInitiated: true, origins }`, loopback only | `ComputerStatus` |
| DELETE | `/api/computer` | `{ ownerId }`, loopback only | `ComputerStatus` |
| GET | `/api/computer/preview?owner=&epoch=&frame=` | loopback only | no-store PNG |
| POST | `/api/computer/actions` | `{ ownerId, requestId, epoch, frameId, action }`, loopback only | `ComputerObservation` |

Errors use `{ error: string }` and an appropriate non-2xx status. Mutations that conflict with an active run return 409. Settings changes invalidate runtime session attachments. State snapshots and SSE exclude API keys.

## Implementation ownership

- Backend owner writes `server/` and backend tests in `tests/`.
- Frontend owner writes `web/`.
- Coordinator owns shared contracts, root config, documentation, and end-to-end tests.
- Reviewers inspect all three boundaries after integration.

## Verification predicates

A browser user configures a local mock OpenAI-compatible endpoint, connects a disposable workspace, sends a prompt through the source-linked application engine, observes tool status, approves or denies a write, and checks exact disk contents. Reopening the page preserves bots and conversation history. Cancellation ends the task. Traversal and symlink paths fail. Provider failures appear in the UI. Desktop and mobile controls stay usable. Approved bot delivery executes a recipient and persists its pair transcript. Managed Chrome supports actual browsing and typed control while private-address and stale-frame actions fail; Stop interrupts navigation. The user authorized original-window capture and the observed states were compared. Complete-product pixel parity remains unverified across independent assets, local extensions, and unobserved states.
