# Source-based engine integration

## Understood as

Build this application from Grok Build's source runtime and own its execution engine. Users run the web app without installing a separate Grok Build CLI. Preserve existing bots, conversations, routines, approved local file access, and query-based browser navigation.

## Source evidence

The official `xai-org/grok-build` repository at revision `2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8` contains the Rust agent runtime and carries the Apache-2.0 license. `crates/codegen/xai-grok-shell/src/agent/app.rs` exports `run_stdio_agent`, and `agent/config.rs` exports the programmatic `Config` constructor. `xai-grok-pager-bin` is the separate CLI and terminal UI composition root.

## Design candidates

- Link `xai-grok-shell` in a small application-owned Rust executable and retain ACP as the internal transport. This reuses the upstream agent loop, model sampler, sessions, and tools while keeping the existing web state, scheduler, and file approval boundary.
- Bind upstream Rust session actors directly into Node. This adds a native interface and lifecycle layer before it improves user behavior. A Rust host process is sufficient for the current single-active-run model.

## Decision

Use an app-owned Rust engine directly linked to the pinned upstream runtime. The entry point reads the app-generated run configuration and serves its internal ACP connection. Upstream helpers still read their effective configuration layers, including system policy under `/etc/grok`; this integration preserves those upstream reads and isolates user-level configuration with private HOME/GROK_HOME/XDG directories. The default runtime path is a project build artifact. It never searches PATH for `grok` or invokes the CLI composition root. Preserve the process supervisor, private working directory, explicit two-tool agent profile, one-use file approval broker, cancellation, inactivity deadlines, credential redaction, and durable application state. The Rust entry point resolves skipped runtime configuration fields explicitly, disables memory/subagents/web search, and uses API-key authentication selected by the private configuration.

The application accepts text-only chat. Before delegating a prompt containing `@`, image-path placeholders, inline data URIs, or a leading slash command after Unicode whitespace, it losslessly encodes the entire text as a JSON string with escaped attachment markers. The display transcript keeps the original message. This prevents upstream's pre-tool file-expansion and command parser from interpreting pasted text as native actions. Replacement history passes through the same boundary. File access continues through the confined broker.

## Build ownership

`engine/Cargo.toml` is an independent Cargo root. Its `async-openai` patch repeats upstream's pinned fork because dependency workspace patches do not propagate to this root. `engine/Cargo.lock` fixes the application dependency graph. The application uses the upstream library's empty default feature set; the CLI/TUI package is outside the executable dependency graph.

`npm run build:engine` uses Rust stable, a native compiler toolchain, and ripgrep. It stages the verified protoc 29.3 archive on Linux x86_64, sets both upstream ripgrep bundle overrides, runs a locked native build, and atomically installs the executable into `.engine/`. It also copies the source notices into `.engine/licenses/`. The current local build uses the unoptimized development profile with debug information disabled. This is a source-build workflow for the host computer; cross-platform archives and bit-for-bit reproducibility remain release work. The verified executable links the host's libc, libm, libgcc, and zlib.

The original Apache-2.0 license, third-party notices, and tool-source notices are retained unchanged in `engine/licenses/`. No upstream source file is modified. Before distributing executable packages, supplement notices for the final locked Rust/npm graph and provide exact covered source availability for MPL dependencies. Upstream's notice inventory alone does not cover every resolved dependency version.

## Verification

The actual executable builds and reports its application version and pinned source identity. The complete automated suite covers missing/mismatched engine identities, bounded history migration, session continuation, tool confinement, approval/denial, cancellation, routines, and lifecycle cleanup. Chrome exercises desktop/mobile chat, files, scheduling, and query-based history against a disposable OpenAI-compatible provider and workspace.

The real-engine integration tests and full six-scenario Chrome suite also passed with `PATH=/usr/bin:/bin`, where `grok` is unavailable. Node was invoked by its absolute path. Tests check that only `search_tool` and `use_tool` reach the provider, forbidden native/shell calls fail, out-of-workspace files stay unchanged, and approved bytes reach disk. Detailed results and remaining limits are in [verification.md](verification.md).
