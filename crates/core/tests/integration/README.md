# Core integration tests

Cargo discovers this directory's `main.rs` as the single `integration` test target. Keep test
modules and their fixtures here rather than adding more `tests/*.rs` targets: each target links
another copy of the engine.

```sh
# All core library and integration tests:
node scripts/cargo.mjs test --locked -p grok-bot-core
# List the 47 integration cases (30 runtime scenarios plus per-Bot concurrency coverage):
node scripts/cargo.mjs test --locked -p grok-bot-core --test integration -- --list
# Run one scenario, including its output:
node scripts/cargo.mjs test --locked -p grok-bot-core --test integration \
  runtime::scenarios::file_approvals_and_sessions -- --exact --nocapture
# Limit parallel cases on a smaller machine, or reproduce one-at-a-time execution:
node scripts/cargo.mjs test --locked -p grok-bot-core --test integration -- --test-threads=2
```

Every engine-backed `#[test]` calls `common::run`. The parent libtest process schedules cases in
parallel; the helper re-executes **the same compiled executable** with an exact single-case filter.
It does not build or link another executable. Each child receives its own temporary home, engine
root, data, workspace, provider and two-worker Tokio runtime. The parent owns the temporary root
until the child exits, including failure paths, and forwards the child's output and failure status.

Process isolation is intentional. The pinned upstream engine caches `GROK_HOME`, initialization
settings and the first filesystem-watcher runtime globally. A per-test data directory or mutex
cannot reset those caches, and changing process environment variables inside concurrent tests is
unsafe. `common::run` sets the child's environment **before startup**, without modifying the
parent's environment. The vault test also installs its log subscriber only in its child.

The former runtime driver contained 28 fixture scenarios plus checklist and startup checks. Those
same bodies and assertions now have 30 separate libtest entry points; the other 16 original test
entry points remain. No application behavior or assertion coverage is intentionally changed.
