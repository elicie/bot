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

test('Rust crate changes keep crate checks targeted and run full e2e plus release verification', () => {
  assert.deepEqual(select('crates/server/src/main.rs'), { ...nothing, rust: 'true', packages: '-p grok-bot-server', specs: 'all', release: 'true' });
  assert.deepEqual(select('crates/core/src/http.rs', 'crates/store/src/lib.rs'), { ...nothing, rust: 'true', packages: '-p grok-bot-core -p grok-bot-store', specs: 'all', release: 'true' });
  assert.deepEqual(select('engine/src/lib.rs'), { ...nothing, rust: 'true', packages: '-p grok-bot-engine', specs: 'all', release: 'true' });
});

test('Cargo manifest and lockfile changes check the whole workspace', () => {
  for (const file of ['Cargo.toml', 'Cargo.lock']) {
    assert.deepEqual(select(file), { ...nothing, rust: 'true', packages: '--workspace', specs: 'all', release: 'true' }, file);
  }
});

test('rustfmt-only changes do not build crates or the standalone release', () => {
  assert.deepEqual(select('rustfmt.toml'), { ...nothing, rust: 'true' });
});

test('frontend and frontend build inputs run units, full Rust e2e, and standalone release', () => {
  for (const file of ['web/App.tsx', 'shared/types.ts', 'index.html', 'vite.config.ts', 'tsconfig.json', 'package.json', 'package-lock.json']) {
    assert.deepEqual(select(file), { ...nothing, web: 'true', unit: 'all', specs: 'all', release: 'true' }, file);
  }
});

test('shared browser fixtures and Playwright config run every spec', () => {
  for (const file of ['tests/app-fixture.ts', 'tests/mock-provider.ts', 'playwright.config.ts']) {
    assert.deepEqual(select(file), { ...nothing, specs: 'all' }, file);
  }
});

test('build tools and CI changes verify full browser and standalone release paths', () => {
  for (const file of ['scripts/cargo.mjs', 'scripts/rust-env.mjs', 'scripts/ci-changes.mjs', 'scripts/ci-changes.test.mjs', 'scripts/verify-release.mjs', '.github/workflows/ci.yml', '.cargo/config.toml', 'rust-toolchain.toml']) {
    assert.deepEqual(select(file), { ...nothing, unit: 'all', specs: 'all', release: 'true' }, file);
  }
});

test('the cleanup runs all e2e without selecting removed backend tests', () => {
  assert.deepEqual(select('tests/backend-api.test.ts', 'tests/app-fixture.ts', 'package.json', '.github/workflows/ci.yml'), {
    ...nothing, web: 'true', unit: 'all', specs: 'all', release: 'true',
  });
});
