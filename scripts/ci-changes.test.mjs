import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const selector = fileURLToPath(new URL('./ci-changes.mjs', import.meta.url));
const select = (...files) => {
  const result = spawnSync(process.execPath, [selector], { input: files.join('\n'), encoding: 'utf8' });
  assert.equal(result.error, undefined);
  assert.equal(result.status, 0, result.stderr);
  return Object.fromEntries(result.stdout.trim().split('\n').map(line => line.split('=')));
};
const nothing = { web: 'false', rust: 'false', packages: '', unit: '', specs: '', release: 'false' };

test('docs-only and empty changes do not schedule builds or tests', () => {
  assert.deepEqual(select(), nothing);
  assert.deepEqual(select('README.md', 'docs/architecture.md'), nothing);
});

test('isolated frontend unit tests stay targeted without a Rust or engine build', () => {
  assert.deepEqual(select('tests/frontend-state.test.ts'), { ...nothing, unit: 'tests/frontend-state.test.ts' });
  assert.deepEqual(select('tests/visualization-document.test.ts'), { ...nothing, unit: 'tests/visualization-document.test.ts' });
});

test('isolated browser specs stay targeted on the default Rust fixture', () => {
  assert.deepEqual(select('tests/computer.spec.ts', 'tests/main-bot.spec.ts'), { ...nothing, specs: 'tests/computer.spec.ts tests/main-bot.spec.ts' });
});

test('deleted backend unit tests and browser specs are not scheduled', () => {
  assert.deepEqual(select('tests/backend-api.test.ts', 'tests/no-longer-present.spec.ts'), nothing);
  assert.deepEqual(select('tests/backend-api.test.ts', 'tests/frontend-state.test.ts'), { ...nothing, unit: 'tests/frontend-state.test.ts' });
});

test('Rust crate changes check only those crates; release only for the release path', () => {
  assert.deepEqual(select('crates/server/src/main.rs'), { ...nothing, rust: 'true', packages: '-p grok-bot-server', release: 'true' });
  assert.deepEqual(select('crates/core/src/web.rs'), { ...nothing, rust: 'true', packages: '-p grok-bot-core', release: 'true' });
  assert.deepEqual(select('crates/core/src/http.rs', 'crates/store/src/lib.rs'), { ...nothing, rust: 'true', packages: '-p grok-bot-core -p grok-bot-store' });
  assert.deepEqual(select('engine/src/lib.rs'), { ...nothing, rust: 'true', packages: '-p grok-bot-engine' });
});

test('Cargo manifest and lockfile changes check the whole workspace without e2e or release', () => {
  for (const file of ['Cargo.toml', 'Cargo.lock']) {
    assert.deepEqual(select(file), { ...nothing, rust: 'true', packages: '--workspace' }, file);
  }
});

test('rustfmt-only changes do not build crates or the standalone release', () => {
  assert.deepEqual(select('rustfmt.toml'), { ...nothing, rust: 'true' });
});

test('frontend and frontend build inputs run only the web build', () => {
  for (const file of ['web/App.tsx', 'shared/types.ts', 'index.html', 'vite.config.ts', 'tsconfig.json', 'package.json', 'package-lock.json']) {
    assert.deepEqual(select(file), { ...nothing, web: 'true' }, file);
  }
});

test('fixtures, build tools and CI changes never broaden a PR to the full suite', () => {
  for (const file of ['tests/app-fixture.ts', 'tests/mock-provider.ts', 'playwright.config.ts', 'scripts/cargo.mjs', 'scripts/rust-env.mjs', 'scripts/ci-changes.mjs', 'scripts/ci-changes.test.mjs', '.github/workflows/ci.yml', '.cargo/config.toml', 'rust-toolchain.toml']) {
    assert.deepEqual(select(file), nothing, file);
  }
  assert.deepEqual(select('scripts/verify-release.mjs'), { ...nothing, release: 'true' });
});
