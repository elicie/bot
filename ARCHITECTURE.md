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

Each routine can hold one webhook key. Only a SHA-256 hash is stored (the public snapshot omits even that). `POST /hooks/routines/:id` lives outside `/api` because external senders cannot send `Origin` or `X-Grok-Bot`; it authenticates `Authorization: Bearer <key>` with a constant-time hash comparison, throttles repeated failures per routine, and claims a `webhook`-trigger run through the same admission as a manual run. An optional body (at most 16 KiB of UTF-8) is appended to the prompt inside a `<webhook_payload>` block marked as untrusted data. Host and Origin validation still apply, so LAN senders need `HOST=0.0.0.0` and an allowed host. Webhook runs leave the schedule untouched, and pausing a routine stops only scheduled runs.

## Permission rules

Rules map an action category (`file_write`, `bot_message`, `bot_create`, `skill_save`, `memory_save`) for all bots or one bot to "항상 묻기" (`ask`, the default) or "묻지 않고 허용" (`allow`). Ask rules win conflicts. Every broker mutation calls `Runtime::approve_category`, which records an allowed action's tool activity with `approvedByRule` (shown as "내 규칙으로 자동 승인됨") instead of showing a card. Rules never lift the safety floor: local-only approvals always ask, bot-delivery recipient conversations and webhook-triggered runs always ask, and hash, size, hop, and recipient checks run before approval. The server has no login, so only a loopback client may add or loosen allow rules; LAN clients may view rules and tighten them.

## Bot messages

`BotExchange` stores a two-bot transcript and `BotDelivery` stores its durable, idempotent execution receipt. The authenticated broker exposes bot discovery and an outbound message proposal. Approval records the full recipient/text before creating the recipient's dedicated conversation and queue entry atomically. A single source run can deliver one distinct message. Each further hop needs fresh approval and chains stop at four deliveries.

The recipient runs under its own identity with file and computer tools denied. Its one terminal final answer is projected once into the exchange. Dedicated recipient conversations reject user-prompt hijacking. The routine dispatcher admits both routine claims and bot deliveries in FIFO order under the same global execution slot. Queued deliveries survive startup; started deliveries become interrupted without resend. Pair transcripts use query routing and preserve the originating chat draft.

## Managed computer

`shared/computer.ts` defines a typed 960 × 600 browser surface. `ManagedComputer` owns a fresh private Chrome, an ephemeral profile, a mandatory verified sandbox, an authenticated loopback proxy, and a public-origin allowlist. Request UUIDs are one-use; epoch and frame UUIDs prevent stale actions. Public-address validation covers all DNS answers, and the proxy pins the selected validated address. HTTP methods, credentials, sensitive controls, downloads/uploads, workers, WebSockets, WebRTC, and native OS actions are restricted. Browser sandbox verification fails closed.

`ApplicationComputer` associates the single browser with a bot, caches local PNG frames, and adds cancellation generations so Stop invalidates pending work across both command queues. HTTP starts/actions share model-run admission. Stop bypasses that queue to interrupt slow navigation. Computer HTTP routes and model permission decisions require a loopback socket; forwarded-address headers grant no trust. LAN snapshots and SSE replace local-only approval/tool details with a content-free stub. Cached PNGs bind to the exact epoch/frame, and coordinate clicks stay disabled until that image loads. Cancellation stops only the browser epoch used by the run. A reverse proxy requires separate authentication and remains outside the deployment contract. Model computer tools operate only on a user-started browser and request one-use local approval for every observation/action. The provider receives redacted URL/title/control text and coordinates; the browser preview uses PNGs locally.

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
| POST | `/api/bots/:botId/skills` | `{ name, description, instructions }` | `AppState`, status 201 |
| POST | `/api/bots/:botId/skills/import` | `{ markdown }` | `AppState`, status 201 |
| PATCH | `/api/bots/:botId/skills/:id` | `{ revision, name?, description?, instructions?, enabled? }` | `AppState` |
| DELETE | `/api/bots/:botId/skills/:id` | `{ revision }` | `AppState` |
| POST | `/api/routines` | `RoutineInput` | `AppState`, status 201 |
| PUT | `/api/routines/:id` | `RoutineInput` | `AppState` |
| PATCH | `/api/routines/:id` | `{ enabled }` | `AppState` |
| DELETE | `/api/routines/:id` | `{}` | `AppState` |
| POST | `/api/routines/:id/run` | `{}` | `{ run, state }`, status 202 |
| POST | `/api/routines/:id/webhook` | `{}` | `{ key, keyHint, createdAt, state }`, status 201 (creates or rotates; the key is returned once) |
| DELETE | `/api/routines/:id/webhook` | `{}` | `AppState` |
| POST | `/hooks/routines/:id` | `Authorization: Bearer <key>`, optional text/JSON body ≤ 16 KiB; no `X-Grok-Bot` | `{ run }`, status 202; 401, 409, 413, 429 |
| GET | `/api/permission-rules` | | `{ rules, canAutoAllow }` |
| POST | `/api/permission-rules` | `{ category, action, botId? }` | `AppState`, status 201; allow rules loopback only |
| PATCH | `/api/permission-rules/:id` | `{ action }` | `AppState`; switching to allow loopback only |
| DELETE | `/api/permission-rules/:id` | `{}` | `AppState`; deleting an ask rule loopback only |
| POST | `/api/conversations` | `{ botId }` | `{ conversation, state }` |
| POST | `/api/conversations/:id/messages` | `{ text }` | `AppState`, status 202 |
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
