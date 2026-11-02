// Decide which CI checks a change needs from its changed file list (one path per line on stdin).
// Prints GitHub Actions outputs (`key=value` lines):
//   web=true|false      web UI typecheck and build
//   rust=true|false     rustfmt check (and clippy/tests for `packages`)
//   packages=...        `-p <crate>` flags of the changed crates, `--workspace` for Cargo.toml/Cargo.lock changes, or empty
//   unit=...            frontend unit tests to run: `all`, a space-separated file list, or empty
//   specs=...           Playwright specs to run: `all` for app/build/fixture/CI changes, changed specs otherwise
//   release=true|false  standalone Rust release build and smoke check
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

const frontendBuild = files.some(file => /^(index\.html|vite\.config\.[cm]?[jt]s|tsconfig[^/]*\.json|package(-lock)?\.json)$/.test(file));
const buildOrCi = frontendBuild || under('.cargo/') || files.some(file => /^(scripts\/(cargo|rust-env|ci-changes(?:\.test)?|verify-release)\.mjs|\.github\/workflows\/ci\.yml|rust-toolchain(\.toml)?)$/.test(file));
const web = under('web/') || under('shared/') || frontendBuild;

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

// Frontend code and build changes run the complete fast unit suite; isolated test edits stay targeted.
// Deleted test/spec paths are deliberately ignored.
const unit = web || buildOrCi ? 'all' : existingTests(/^tests\/[^/]+\.test\.ts$/).join(' ');

// Shared fixtures, app code, and build/CI changes can affect every browser spec. A spec-only change
// still runs just that spec. This also gives the Node-backend removal a full Rust e2e run.
const fixtures = files.some(file => ['tests/app-fixture.ts', 'tests/mock-provider.ts', 'playwright.config.ts'].includes(file));
const release = web || buildOrCi || rootManifest || crateDirs.size > 0;
const specs = release || fixtures ? 'all' : existingTests(/^tests\/[^/]+\.spec\.ts$/).join(' ');

for (const [key, value] of Object.entries({ web, rust, packages, unit, specs, release })) console.log(`${key}=${value}`);
