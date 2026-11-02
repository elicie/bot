// Decide which CI checks a change needs from its changed file list (one path per line on stdin).
// Prints GitHub Actions outputs (`key=value` lines):
//   web=true|false      web UI typecheck and build
//   rust=true|false     rustfmt check (and clippy/tests for `packages`)
//   packages=...        `-p <crate>` flags including reverse dependents, `--workspace`, or empty
//   node=...            Node backend tests to run: `all`, a space-separated file list, or empty
//   specs=...           Playwright spec files that changed, or empty
//   tools=true|false    CI script tests (independent of the app suites)
// Usage: git diff --name-only <base>...HEAD | node scripts/ci-changes.mjs
// There is no full-suite mode: every run, including one that edits CI itself, checks only what changed.
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

export const readCargoMetadata = (cwd = root) => JSON.parse(execFileSync('cargo', [
  'metadata', '--format-version', '1', '--no-deps', '--locked',
], { cwd, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }));

// Only conventional crate-root README files (bare/Markdown) and docs/ Markdown are doc-only.
// Keep source/test fixtures (even Markdown), build inputs and other non-code assets selected.
const isCrateDocumentation = file => /^(?:crates\/[^/]+\/|engine\/)(?:README(?:\.(?:md|markdown))?|docs\/.*\.(?:md|markdown))$/i.test(file);
const isCrateFile = file => !isCrateDocumentation(file)
  && (/^(crates\/|engine\/)/.test(file) || file.endsWith('.rs') || file.endsWith('/Cargo.toml'));
const isRustBuildFile = file => /^(\.cargo\/|rust-toolchain(?:\.toml)?$)/.test(file)
  || ['Cargo.toml', 'Cargo.lock', 'scripts/cargo.mjs', 'scripts/rust-env.mjs', 'scripts/build-engine.mjs'].includes(file);

const affectedPackages = (files, cwd, loadMetadata) => {
  const crateFiles = files.filter(isCrateFile);
  if (crateFiles.length === 0) return '';

  // --no-deps avoids resolving remote dependencies; dependencies.path still describes every
  // workspace path edge, including renamed, optional, dev/build and target-specific edges.
  // Let metadata failures fail detection rather than silently skip checks on a broken workspace.
  const metadata = loadMetadata();
  const memberIds = new Set(metadata.workspace_members);
  const members = metadata.packages.filter(pkg => memberIds.has(pkg.id));
  const byDir = new Map(members.map(pkg => [path.dirname(path.resolve(cwd, pkg.manifest_path)), pkg]));
  // Prefer the innermost member if workspace package directories are nested.
  const directories = [...byDir.keys()].sort((a, b) => b.length - a.length);
  const selected = new Set();
  for (const file of crateFiles) {
    const absolute = path.resolve(cwd, file);
    const directory = directories.find(dir => absolute.startsWith(`${dir}${path.sep}`));
    // A removed/renamed member is absent from current metadata: don't drop its checks.
    if (!directory) return '--workspace';
    selected.add(byDir.get(directory).id);
  }

  const dependents = new Map(members.map(pkg => [pkg.id, new Set()]));
  for (const pkg of members) {
    for (const dependency of pkg.dependencies) {
      const target = dependency.path && byDir.get(path.resolve(cwd, dependency.path));
      if (target) dependents.get(target.id).add(pkg.id);
    }
  }
  // Set iteration visits newly added members too, giving the transitive reverse closure.
  for (const id of selected) {
    for (const dependent of dependents.get(id)) selected.add(dependent);
  }
  return members.filter(pkg => selected.has(pkg.id)).map(pkg => pkg.name).sort().map(name => `-p ${name}`).join(' ');
};

export function selectChanges(changedFiles, { cwd = root, loadMetadata = () => readCargoMetadata(cwd) } = {}) {
  const files = [...new Set(changedFiles.map(file => file.trim()).filter(Boolean))];
  const under = prefix => files.some(file => file.startsWith(prefix));
  const tools = under('scripts/ci-') || files.includes('.github/workflows/ci.yml');
  const web = under('web/') || under('shared/') || files.some(file => /^(index\.html|vite\.config\.[cm]?[jt]s|tsconfig[^/]*\.json|package(-lock)?\.json)$/.test(file));
  const packages = files.some(isRustBuildFile) ? '--workspace' : affectedPackages(files, cwd, loadMetadata);
  // Formatting configuration alone needs only the format check, not crate lints and tests.
  const rust = packages !== '' || files.some(file => ['rustfmt.toml', '.rustfmt.toml'].includes(file));

  // The Node backend has no per-module test mapping: a backend change runs its whole (fast)
  // suite, otherwise only the Node test files that changed.
  const nodeTests = files.filter(file => /^tests\/[^/]+\.test\.ts$/.test(file));
  const node = under('server/') ? 'all' : nodeTests.join(' ');
  const specs = files.filter(file => /^tests\/[^/]+\.spec\.ts$/.test(file)).join(' ');
  return { web, rust, packages, node, specs, tools };
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const args = process.argv.slice(2);
    if (args.length) throw new Error('Usage: git diff --name-only <base>...HEAD | node scripts/ci-changes.mjs');
    const files = readFileSync(0, 'utf8').split('\n');
    for (const [key, value] of Object.entries(selectChanges(files))) console.log(`${key}=${value}`);
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
