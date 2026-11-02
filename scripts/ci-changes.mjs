// Decide which CI checks a change needs from its changed file list (one path per line on stdin).
// Prints GitHub Actions outputs (`key=value` lines):
//   web=true|false      web UI typecheck and build
//   rust=true|false     rustfmt check (and clippy/tests for `packages`)
//   packages=...        `-p <crate>` flags of the changed crates, `--workspace` for Cargo.toml/Cargo.lock changes, or empty
//   node=...            Node backend tests to run: `all`, a space-separated file list, or empty
//   specs=...           Playwright spec files that changed (only those run), space-separated
// Usage: git diff --name-only <base>...HEAD | node scripts/ci-changes.mjs
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const files = readFileSync(0, 'utf8').split('\n').map(line => line.trim()).filter(Boolean);
const under = (prefix) => files.some(file => file.startsWith(prefix));

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

// The Node backend has no per-module test mapping: a backend change runs its whole (fast) suite,
// otherwise only the Node test files that changed.
const nodeTests = files.filter(file => /^tests\/[^/]+\.test\.ts$/.test(file));
const node = under('server/') ? 'all' : nodeTests.join(' ');

const specs = files.filter(file => /^tests\/[^/]+\.spec\.ts$/.test(file)).join(' ');

for (const [key, value] of Object.entries({ web, rust, packages, node, specs })) console.log(`${key}=${value}`);
