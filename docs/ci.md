# CI scope and runner diagnostics

## Decisions

- **PRs and main pushes stay selective.** Changes to a Rust crate run its tests and
  clippy plus those of every transitive reverse-dependent workspace crate. For
  example, changing `grok-bot-store` selects store, core and server, not engine.
  `cargo metadata --format-version 1 --no-deps --locked` supplies manifest paths
  and dependency paths without resolving/downloading the external dependency graph.
  Renamed, optional, dev, build and target-specific workspace dependencies are
  conservatively included. Unknown/deleted crate directories select the workspace.
- **Run the full suite daily on main**, at **03:17 UTC**, to cover gaps in the
  intentionally selective Node/browser mapping. Scheduled runs do not diff a base
  commit: they run web typecheck/build, workspace rustfmt/clippy/tests, every Node
  test and every Playwright spec. GitHub schedules use the default branch and may
  start late. Main push concurrency cannot cancel a scheduled check.
- **Manual runs are full runs too.** Use the Actions UI's CI → Run workflow, or:

  ```sh
  gh workflow run ci.yml --repo elicie/bot --ref main
  ```

- Pull requests never run the full application suite, including PRs that edit the
  CI workflow, selector or diagnostics. A missing or invalid diff base falls back
  to the last commit. Root Cargo manifests/lockfile and Rust build-environment
  changes select the Rust workspace; a rustfmt-only change needs just formatting.
- Detect runs only deterministic selector regressions. Timing-sensitive diagnostic
  process tests have a separate `CI diagnostic regressions` job, selected only for
  CI workflow/helper changes and daily/manual runs, so an ordinary application PR
  does not depend on process-lifecycle timing checks.
- Conventional crate-root READMEs and Markdown under a crate's `docs/` tree do not
  select Rust jobs by themselves. Source/test fixtures and other assets remain
  conservative inputs, including Markdown outside those documentation locations.
- Existing application-PR Node/backend and changed-spec mapping is preserved;
  reverse-dependency expansion concerns Rust crates. The daily full run is the
  safety net for cross-cutting JS/browser behavior, not a claim that selective
  checks cover every downstream behavior.

## When a command stops producing output

Long build/test commands run through [the resource wrapper](../scripts/ci-run.mjs).
It emits a snapshot before, after, and every 60 seconds during the command:

- UTC timestamp, process IDs, command names (not arguments/environment), RSS,
  CPU usage and process state;
- available disk space and inodes for root, workspace and temporary storage;
- target directory size, RAM/swap and Linux memory/I/O pressure;
- available cgroup memory usage, limits and OOM counters;
- the command's exit code or terminating signal, and best-effort recent kernel
  warnings after failure.

Diagnostics run asynchronously with bounded output and two-second probe timeouts;
slow probes do not block cancellation or replace the command's exit status.
Cancellation is forwarded to an owned command process group. The wrapper keeps
that group referenced and cleans it up on normal completion too, escalating after
five seconds if descendants survive a successful direct-child exit. A synchronous
exit guard also attempts cleanup after explicit wrapper exit or uncaught failure;
it never signals the runner's own group. Forced wrapper SIGKILL/host loss cannot
run cleanup, and a descendant that deliberately creates a new session can escape
the group. Engine compilation is separate from Node tests, identifying which phase
is actually stalled. Existing per-test and job/step time limits remain in place.

The snapshots are in the Actions job log, including while a command is silent;
Playwright failure traces/screenshots retain the existing seven-day artifact
policy. **A lost or unresponsive host can still stop log delivery and artifact
upload.** These diagnostics narrow the next investigation; they cannot guarantee
that every runner loss has a recoverable definitive cause. Missing resource files
or privileged kernel-log access are non-fatal and explicitly best-effort.

Local smoke checks (no npm install required):

```sh
node --test scripts/ci-*.test.mjs
printf 'crates/store/src/lib.rs\n' | node scripts/ci-changes.mjs
node scripts/ci-changes.mjs --full
CI_RESOURCE_INTERVAL_MS=1000 node scripts/ci-run.mjs node -e 'setTimeout(() => {}, 2200)'
```

## Historical evidence for issue #13

All times below are 2026-11-02 UTC. These observations distinguish an actual
failure from a suspected cause; see [issue #13](https://github.com/elicie/bot/issues/13).

### Silent Node-stage runs: root cause not established

- [Run 41381018518, job 125071642871](https://github.com/elicie/bot/actions/runs/41381018518/job/125071642871)
  records Node tests in progress before concurrency cancelled the run.
- [Run 41383713890, job 125081217432](https://github.com/elicie/bot/actions/runs/41383713890/job/125081217432)
  records Node tests in progress before cancellation by `@elicie`. This revision
  already had a per-test timeout and step timeout; another timeout alone does not
  explain the silence. Step metadata after cancellation may be stale.
- Neither job's log archive was retrievable with `gh run view --log`. The direct
  job-log download for job 125071642871 also returned HTTP 404 / BlobNotFound.
  There are no contemporaneous disk, memory or process readings
  to establish disk exhaustion, OOM, a test/child-process hang or host failure.
- The later [PR #26 run 41396847165](https://github.com/elicie/bot/actions/runs/41396847165)
  again recorded Node tests in progress, then concurrency cancellation. Both log
  retrieval methods still returned missing archives after completion, so the
  underlying cause remains unknown. A local run of all 105 Node tests passed in
  14.34 seconds; that does not establish hosted-runner health.
- The latest successful selective jobs instead show 91–92 GiB free and an
  11–12 GiB target directory. This does **not** establish earlier disk conditions.
- [A separate failed Rust job](https://github.com/elicie/bot/actions/runs/41381640432/job/125073517683)
  failed a Chrome race assertion expecting HTTP 409; it did not reach Node tests.
  That assertion failure is not evidence of disk exhaustion.

### Rust cache: saves and reuse verified separately

[PR #32](https://github.com/elicie/bot/pull/32) moved same-repository build jobs to
the self-hosted `grok-bot` runner, using its pinned Rust toolchain and persistent
`CARGO_TARGET_DIR`. They do not upload an Actions Rust/npm cache. Fork PRs still
use hosted runners and `Swatinem/rust-cache`; the default key includes the job ID,
separating `lint` from `test`. No cache-key collision was established, so those
defaults are kept. Job summaries distinguish persistent targets from Actions
**exact-match** results; restore/save logs show fallback keys and actual uploads.
An npm cache hit is not a Rust cache hit.

Confirmed historical uploads:

| Scope | Rust test cache upload | Rust lint cache upload |
| --- | --- | --- |
| [Selective PR #9](https://github.com/elicie/bot/actions/runs/41390384145) | 1,531,905,855 bytes | 816,224,461 bytes |
| [Main](https://github.com/elicie/bot/actions/runs/41391689111) | 1,531,810,463 bytes at 05:15:25 | 816,058,453 bytes at 05:09:15 |

All inspected Rust restores before this PR were misses. The old combined `rust`
job, new `test`/`lint` keys and PR/main cache scopes differ, explaining why those
saves alone are not evidence of a broken cache or a warm run.

Follow-up verification in [PR #26's first full run](https://github.com/elicie/bot/actions/runs/41396014649):
both `lint` and `test` logged their exact Rust cache keys, successful restoration,
and `cache-hit=true`, confirming that main's saved caches are reusable by a new
PR. That run stopped on an existing integration-test disk-scan race (observing an
atomic state-file replacement before completion), not on a resource failure.
The fixture synchronization fix is isolated in [PR #36](https://github.com/elicie/bot/pull/36),
not this CI change. It flushes pending writes before asserting the unchanged
secret-file allowlist; it does not ignore temporary files or weaken the assertion.

### Timing target

The selective [PR #9 run](https://github.com/elicie/bot/actions/runs/41390384145)
finished in **9m01s** and [PR #7 run](https://github.com/elicie/bot/actions/runs/41390382345)
in **13m29s**, both with cold Rust caches. Node tests were skipped in those runs,
so they are not proof of warm/full Node CI performance. An
[older full run](https://github.com/elicie/bot/actions/runs/41383717952) took
**18m27s**, including 105 passing Node tests in 32 seconds.

The under-15-minute target applies to ordinary selective PRs, not an empty-cache
workspace rebuild or the daily/manual full suite. Keep measuring the critical-path
job and whether the Rust cache actually restored; do not infer a guarantee from a
single run. New reverse-dependent coverage can increase the work for low-level
crate changes, intentionally trading some selectivity for correctness.
