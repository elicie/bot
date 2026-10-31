# Local Grok Bot architecture

## Understood as

Build a browser application with the Grok Bot desktop layout, configurable OpenAI-compatible inference through Grok Build, and intentional editing of local files on the backend computer. Implement the renderer independently and verify real browser behavior and disk changes.

## Data ownership

`shared/types.ts` defines public bots, conversations, messages, tool activities, pending permissions, runtime state, settings, and text documents. The backend is the single writer of durable application state. Grok Build owns ACP execution sessions. The application stores session IDs and a display transcript. API keys stay in the backend data directory and subprocess environment.

## Runtime

One local HTTP server serves both the React frontend and the API. The server binds to `127.0.0.1` and validates Host and Origin. SSE carries public state snapshots with a persisted monotonic revision. Changes publish after their atomic state write succeeds. The frontend rejects snapshots older than its current revision. HTTP commands mutate settings, bots, conversations, approvals, and workspace documents. The active run states are `starting`, `running`, and `awaiting_permission`. Terminal states are `completed`, `cancelled`, `error`, and `interrupted`.

The runtime adapter starts an isolated `grok agent --no-leader stdio` child per run and loads the conversation's previous ACP session when configuration matches. Private Grok homes isolate configuration, credentials, and agent state from the user's CLI settings. The selected custom model uses `chat_completions`. A curated ACP agent profile exposes only MCP discovery and invocation. An app-owned HTTP MCP server performs workspace operations. Every MCP write waits for a one-use browser approval. Cancellation settles pending permissions and ends the child process. Grok Build 1.0.62 is required because its tool restriction and file-backed argument behavior were verified. Automatic retries are disabled so provider failures reach the UI promptly. The backend enforces a 30-second inactivity deadline, pauses it while a user approval is pending, and resets it on actual ACP updates. A child supervisor watches the parent IPC connection and terminates the Grok process group after backend death.

## File access

The server operates on an explicitly selected workspace. HTTP and MCP reads and writes use the same confined file service and reject symbolic links, hard links, traversal, binary documents, and oversized files. Linux opens parent directories through pinned descriptors and uses no-follow flags. Explicit Save uses a content hash to detect external changes. The agent runs in an app-private working directory and accesses the user workspace through the MCP broker. Native file tools, shell execution, subagents, and external tools are disabled. This is application-level confinement. It does not claim an OS sandbox against other programs running as the same user.

## HTTP contract

All JSON mutations send `Content-Type: application/json` and `X-Grok-Bot: 1`. The browser uses the same origin for API requests.

| Method | Path | Request | Response |
| --- | --- | --- | --- |
| GET | `/api/state` | | `AppState` |
| GET | `/api/events` | | SSE `state` events with `AppState` |
| PUT | `/api/settings` | `SettingsInput` | `AppState` |
| POST | `/api/provider/test` | `{ baseUrl?, apiKey? }` | `ProviderCheck` |
| POST | `/api/bots` | `{ name, color?, instructions? }` | `AppState` |
| PATCH | `/api/bots/:id` | `{ name?, color?, instructions? }` | `AppState` |
| POST | `/api/conversations` | `{ botId }` | `{ conversation, state }` |
| POST | `/api/conversations/:id/messages` | `{ text }` | `AppState`, status 202 |
| POST | `/api/conversations/:id/cancel` | `{}` | `AppState` |
| POST | `/api/conversations/:id/permissions/:permissionId` | `{ optionId }` | `AppState` |
| GET | `/api/files?path=` | workspace-relative directory | `FileListing` |
| GET | `/api/file?path=` | workspace-relative file | `FileDocument` |
| PUT | `/api/file` | `{ path, content, hash }` | `FileDocument` |

Errors use `{ error: string }` and an appropriate non-2xx status. Mutations that conflict with an active run return 409. Settings changes invalidate runtime session attachments. State snapshots and SSE exclude API keys.

## Implementation ownership

- Backend owner writes `server/` and backend tests in `tests/`.
- Frontend owner writes `web/`.
- Coordinator owns shared contracts, root config, documentation, and end-to-end tests.
- Reviewers inspect all three boundaries after integration.

## Verification predicates

A browser user configures a local mock OpenAI-compatible endpoint, connects a disposable workspace, sends a prompt through the actual Grok Build binary, observes tool status, approves or denies a write, and checks exact disk contents. Reopening the page preserves bots and conversation history. Cancellation ends the task. Traversal and symlink paths fail. Provider failures appear in the UI. Desktop and mobile controls stay usable. Exact complete-product pixel parity remains unverified without runnable original screens.
