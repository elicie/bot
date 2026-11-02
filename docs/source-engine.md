# Source-based engine integration

## Understood as

Build this application from Grok Build's source runtime and own its execution engine. Users run one Rust web-server executable without installing Node, a separate Grok Build CLI, or an engine executable. Preserve existing bots, conversations, routines, approved local file access, and query-based browser navigation.

## Source evidence

The official [xai-org/grok-build repository](https://github.com/xai-org/grok-build/tree/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8) at revision `2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8` contains the Rust agent runtime and carries the Apache-2.0 license. Its public `MvpAgent`, ACP connection, and configuration APIs support the in-process host described in [rust-migration.md](<rust-migration.md>). The upstream `xai-grok-pager-bin` CLI and terminal UI composition root are not built or invoked.

## Decision

[engine/src/host.rs](<../engine/src/host.rs>) links the pinned `xai-grok-shell` library into the server. The agent is `!Send`, so a dedicated OS thread owns a current-thread Tokio runtime and `LocalSet`. `MvpAgent` and the application's ACP client communicate over `tokio::io::duplex`; commands and events cross thread channels, not Node↔Rust stdio. The engine is a library, not a standalone binary. [crates/core](<../crates/core/>) owns HTTP state, scheduler, file approval broker, and runtime admission. One shared engine starts lazily and the application retains its single-active-run policy.

Upstream helpers still read their effective configuration layers, including system policy under `/etc/grok`. The application prepares a private HOME/GROK_HOME/XDG environment before starting threads and does not reuse the user's Grok login or settings. The host resolves skipped runtime configuration fields explicitly and disables ambient memory, subagents, web search, and native file/shell tools. MCP discovery/invocation stays behind the application's broker; the native todo tool reports progress without adding execution permissions. The provider relay keeps the real provider API key out of the engine configuration.

Cancellation settles permissions and sends ACP `session/cancel`. The backend's inactivity deadline pauses while approval is pending. There is no per-turn engine process, process supervisor, or independently killable engine. A changed configuration/source fingerprint creates a replacement session with bounded visible history instead of replaying earlier tools. `panic = "unwind"` preserves thread unwinding; it does not establish complete recovery from every possible upstream failure.

Before delegating text containing `@`, image-path placeholders, inline data URIs, or a leading slash command after Unicode whitespace, the application losslessly encodes it as a JSON string with escaped attachment markers. The display transcript keeps the original message. This prevents upstream's pre-tool file-expansion and command parser from interpreting pasted text as native actions. Replacement history passes through the same boundary. Explicit attachments use their separate application-controlled path described in [ARCHITECTURE.md](<../ARCHITECTURE.md#read-state-deletion-attachments-and-rich-text>). File access continues through the confined broker.

## Build ownership

[engine/Cargo.toml](<../engine/Cargo.toml>) is a member of the root [Cargo workspace](<../Cargo.toml>). The root [Cargo.lock](<../Cargo.lock>) pins the complete Rust dependency graph, including the engine; there is no independent engine lockfile or engine build/install step. The root workspace applies the pinned `async-openai` fork. The upstream library uses an empty default feature set, excluding the CLI/TUI composition root.

The [Cargo wrapper](<../scripts/cargo.mjs>) uses Rust stable, a native compiler toolchain, and ripgrep. It stages a checksum-verified protoc 29.3 archive on Linux x86_64 unless `PROTOC` is supplied and sets both upstream ripgrep bundle overrides. Its build-tool cache is not a runtime engine installation. Source builds also need `pkg-config`, zlib development files, and `unzip`. Node.js 22.12+ is required for frontend/build/test tooling only.

```sh
npm ci
npm run build          # frontend only
npm run build:server   # Rust debug server with engine library
npm start              # Rust debug server through the Cargo wrapper
```

`npm run dev` combines the frontend build and Rust debug startup; it does not provide HMR. For deployment:

```sh
npm run build:release
./target/release/grok-bot
./target/release/grok-bot --version
./target/release/grok-bot --licenses
```

`build:release` first runs the frontend build, then `node scripts/cargo.mjs build --locked --release -p grok-bot-server`. [crates/server/build.rs](<../crates/server/build.rs>) embeds the compiled [dist/](<../dist/>); release compilation requires `dist/index.html`. Direct release compilation therefore also requires `npm run build` first. The resulting executable runs from any working directory without Node/npm, an external frontend directory, a separate engine, or the source checkout.

An explicit `GROK_BOT_WEB_ROOT` selects external assets instead and must contain `index.html`. Debug builds allow API-only operation without frontend output; they prefer a bundle embedded at compile time and otherwise serve `dist/` from the current working directory. The release profile is `opt-level = 3`, `debug = 0`, `strip = "symbols"`, `panic = "unwind"`; do not use `panic = "abort"`, which bypasses engine-thread unwinding. A single executable is not a static-link or cross-platform guarantee. Compatible Linux native libraries, including libc and zlib, remain required; managed browsing additionally needs system Google Chrome and its verified sandbox.

## Notices and redistribution

The original source notice is retained in [engine/NOTICE](<../engine/NOTICE>). The Apache-2.0 license, third-party notices, and tool-source notices remain unchanged in [engine/licenses](<../engine/licenses/>). `grok-bot --licenses` prints these embedded originals, so they remain accessible when the executable is copied without the checkout. Source and executable redistributions must preserve the applicable licenses and notices.

The upstream notice inventory is not a complete inventory for the final locked Rust and bundled frontend dependency graph. Before distributing packages, review those exact resolved dependencies, supplement their required licenses/notices, and provide the covered source availability required by MPL dependencies. Embedding the original notices does not by itself demonstrate that all distribution obligations have been met. No upstream source file is modified by this integration.

## Verification

Current backend/engine tests use `npm run test:rust`; frontend unit tests use `npm test`. Browser acceptance uses `npm run build`, `npm run build:server`, then `npm run test:e2e`, with Rust as the only backend and no opt-in environment flag. Fixtures use disposable state, workspaces, and a local OpenAI-compatible provider; they do not require paid model calls.

P4 full-suite, release-build, standalone-runtime, and notice-output results and verification limits are recorded in [verification.md](<verification.md>). Earlier engine-subprocess and P0–P3 results are historical evidence, not proof that the P4 artifact or final dependency notices have been verified.
