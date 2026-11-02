// Decide which CI checks a change needs from its changed file list (one path per line on stdin).
// Prints GitHub Actions outputs (`key=value` lines):
//   web=true|false      web UI typecheck and build
//   rust=true|false     rustfmt check (and clippy/tests for `packages`)
//   packages=...        `-p <crate>` flags of the changed crates, `--workspace` for Cargo.toml/Cargo.lock changes, or empty
//   unit=...            frontend unit test files that changed (only those run), space-separated
//   specs=...           Playwright spec files that changed (only those run), space-separated
//   release=true|false  standalone Rust release build and smoke check, only for release-path changes
// Usage: git diff --name-only <base>...HEAD | node scripts/ci-changes.mjs
import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const files = readFileSync(0, 'utf8').split('\n').map(line => line.trim()).filter(Boolean);
const under = (prefix) => files.some(file => file.startsWith(prefix));
const existingTests = (pattern) => files.filter(file => pattern.test(file) && existsSync(path.join(root, file)));

const packageName = (dir) => {
  const manifest = readFileSync(path.join(root, dir, 'Cargo.toml'), 'utf8');
  return manifest.match(/^\[package\][^[]*?^name\s*=\s*"([^"]+)"/ms)?.[1];
};

const web = under('web/') || under('shared/') || files.some(file => /^(index\.html|vite\.config\.[cm]?[jt]s|tsconfig[^/]*\.json|package(-lock)?\.json)$/.test(file));

const rootManifest = files.some(file => ['Cargo.toml', 'Cargo.lock'].includes(file));
const crateDirs = new Set();
for (const file of files) {
  const crate = file.match(/^(crates\/[^/]+)\//)?.[1];
  if (crate) crateDirs.add(crate);
  else if (file.startsWith('engine/')) crateDirs.add('engine');
}
const packages = rootManifest
  ? '--workspace'
  : [...crateDirs].map(dir => { try { return packageName(dir); } catch { return undefined; } }).filter(Boolean).map(name => `-p ${name}`).join(' ');
// rustfmt.toml alone needs only the format check, not the crate lints and tests.
const rust = packages !== '' || files.includes('rustfmt.toml');

// Pull requests check only what changed: no broadening to the whole unit or browser suite.
// Deleted test/spec paths are deliberately ignored.
const unit = existingTests(/^tests\/[^/]+\.test\.ts$/).join(' ');
const specs = existingTests(/^tests\/[^/]+\.spec\.ts$/).join(' ');

// The opt-level-3 release build is slow; run it only when the release path itself changes.
const release = under('crates/server/') || files.some(file => ['crates/core/src/web.rs', 'scripts/verify-release.mjs'].includes(file));

for (const [key, value] of Object.entries({ web, rust, packages, unit, specs, release })) console.log(`${key}=${value}`);
