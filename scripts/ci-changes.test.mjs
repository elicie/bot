import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { readCargoMetadata, selectChanges } from './ci-changes.mjs';

const script = fileURLToPath(new URL('./ci-changes.mjs', import.meta.url));
const cwd = path.resolve('synthetic-workspace');
const empty = { web: false, rust: false, packages: '', unit: '', specs: '', release: false, tools: false };
const noMetadata = { loadMetadata: () => assert.fail('metadata must remain lazy'), fileExists: () => true };
const dependency = (dir, fields = {}) => ({ name: path.basename(dir), path: path.join(cwd, dir), ...fields });
const member = (name, dir, dependencies = []) => ({
  id: `workspace:${name}`, name, manifest_path: path.join(cwd, dir, 'Cargo.toml'), dependencies,
});
const metadataFor = packages => ({ packages, workspace_members: packages.map(pkg => pkg.id), resolve: null });
const workspace = () => metadataFor([
  member('engine', 'engine'),
  member('model', 'crates/model'),
  member('files', 'crates/files', [dependency('crates/model')]),
  member('store', 'crates/store', [dependency('crates/model'), dependency('crates/files')]),
  member('core', 'crates/core', [dependency('crates/store'), dependency('engine')]),
  member('server', 'crates/server', [dependency('crates/core'), dependency('engine')]),
]);
const select = (files, metadata = workspace()) => selectChanges(files, {
  cwd, loadMetadata: () => metadata, fileExists: () => true,
});
const cli = (args = [], input = '') => spawnSync(process.execPath, [script, ...args], {
  encoding: 'utf8', input, timeout: 10_000, env: { ...process.env, PATH: '' },
});
const outputs = result => {
  assert.equal(result.error, undefined);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stderr, '');
  return Object.fromEntries(result.stdout.trimEnd().split('\n').map(line => line.split('=')));
};
const emptyOutputs = { web: 'false', rust: 'false', packages: '', unit: '', specs: '', release: 'false', tools: 'false' };

// Preserve main's affected-crate coverage, including the engine as an in-process Rust library.
test('selects the transitive reverse closure, not upstream or unrelated crates', () => {
  const cases = [
    ['crates/model/src/lib.rs', '-p core -p files -p model -p server -p store'],
    ['crates/files/src/lib.rs', '-p core -p files -p server -p store'],
    ['crates/store/src/lib.rs', '-p core -p server -p store'],
    ['crates/core/src/lib.rs', '-p core -p server'],
    ['crates/server/src/main.rs', '-p server'],
    ['engine/src/lib.rs', '-p core -p engine -p server'],
  ];
  for (const [file, packages] of cases) {
    assert.deepEqual(select([file]), { ...empty, rust: true, packages, release: file.startsWith('crates/server/') }, file);
  }
});

test('deduplicates files and reverse paths and loads metadata once per selection', () => {
  let calls = 0;
  const result = selectChanges([
    'crates/store/src/lib.rs', 'crates/store/src/lib.rs', 'crates/store/tests/store.rs',
    'crates/core/src/lib.rs', 'engine/src/main.rs',
  ], { cwd, loadMetadata: () => { calls++; return workspace(); } });
  assert.equal(calls, 1);
  assert.equal(result.packages, '-p core -p engine -p server -p store');
});

test('matches manifest paths rather than package names or fixed directory depth', () => {
  const metadata = metadataFor([
    member('storage-library', 'crates/group/storage'),
    member('storage-client', 'crates/client', [dependency('crates/group/storage')]),
    member('nonstandard-layout', 'components/library'),
  ]);
  assert.equal(select(['crates/group/storage/src/lib.rs'], metadata).packages, '-p storage-client -p storage-library');
  assert.equal(select(['components/library/src/lib.rs'], metadata).packages, '-p nonstandard-layout');
});

test('matches the innermost manifest and respects directory boundaries', () => {
  const metadata = metadataFor([
    member('outer', 'crates/store'),
    member('inner', 'crates/store/nested'),
  ]);
  assert.equal(select(['crates/store/nested/src/lib.rs'], metadata).packages, '-p inner');
  assert.equal(select(['crates/storehouse/src/lib.rs'], metadata).packages, '--workspace');
});

test('conservatively follows renamed, optional, dev, build and target-specific path edges', () => {
  const edges = [
    ['renamed', { name: 'different-package-name', rename: 'storage_alias' }],
    ['optional', { optional: true }],
    ['dev', { kind: 'dev' }],
    ['build', { kind: 'build' }],
    ['target', { target: 'cfg(windows)' }],
  ];
  const metadata = metadataFor([
    member('store', 'crates/store'),
    member('engine', 'engine'),
    ...edges.map(([name, fields]) => member(name, `crates/${name}`, [dependency('crates/store', fields)])),
  ]);
  assert.equal(select(['crates/store/src/lib.rs'], metadata).packages, '-p build -p dev -p optional -p renamed -p store -p target');
});

test('handles cycles and duplicate dependency edges without duplicate flags', () => {
  const metadata = metadataFor([
    member('a', 'crates/a', [dependency('crates/b'), dependency('crates/b', { kind: 'dev' })]),
    member('b', 'crates/b', [dependency('crates/a', { kind: 'dev' })]),
  ]);
  assert.equal(select(['crates/a/src/lib.rs'], metadata).packages, '-p a -p b');
});

test('filters packages by workspace membership and ignores non-workspace dependency paths', () => {
  const metadata = workspace();
  metadata.packages.push(member('outsider', 'crates/outsider', [dependency('crates/store')]));
  metadata.packages.push(member('unrelated', 'crates/unrelated', [
    { name: 'store' }, dependency('vendor/store', { name: 'store' }),
  ]));
  metadata.workspace_members.push('workspace:unrelated');
  assert.equal(select(['crates/store/src/lib.rs'], metadata).packages, '-p core -p server -p store');
  assert.equal(select(['crates/outsider/src/lib.rs'], metadata).packages, '--workspace');
});

test('unknown or deleted crate paths select the workspace instead of dropping checks', () => {
  for (const file of ['crates/deleted/src/lib.rs', 'crates/deleted/Cargo.toml', 'unknown/build.rs']) {
    assert.deepEqual(select([file]), { ...empty, rust: true, packages: '--workspace' }, file);
  }
  assert.equal(select(['crates/store/src/lib.rs', 'crates/deleted/src/lib.rs']).packages, '--workspace');
  assert.equal(select(['engine/src/lib.rs'], metadataFor([])).packages, '--workspace');
});

test('metadata failures are not silently swallowed', () => {
  assert.throws(() => selectChanges(['crates/store/src/lib.rs'], {
    loadMetadata: () => { throw new Error('broken workspace metadata'); },
  }), /broken workspace metadata/);
});

test('root manifests and Rust build helpers select the workspace without app or release expansion', () => {
  for (const file of [
    'Cargo.toml', 'Cargo.lock', '.cargo/config.toml', 'rust-toolchain', 'rust-toolchain.toml',
    'scripts/cargo.mjs', 'scripts/rust-env.mjs',
  ]) {
    assert.deepEqual(selectChanges([file], noMetadata), { ...empty, rust: true, packages: '--workspace' }, file);
  }
});

test('member manifest changes retain focused reverse-dependent selection', () => {
  assert.equal(select(['crates/store/Cargo.toml']).packages, '-p core -p server -p store');
});

test('formatting-only changes run just fmt, with no metadata', () => {
  for (const file of ['rustfmt.toml', '.rustfmt.toml']) {
    assert.deepEqual(selectChanges([file], noMetadata), { ...empty, rust: true }, file);
  }
});

test('empty and documentation-only changes need no jobs or metadata', () => {
  for (const files of [[], [''], ['README.md', 'docs/ci.md'], ['scripts/unrelated.mjs', '.github/workflows/other.yml']]) {
    assert.deepEqual(selectChanges(files, noMetadata), empty);
  }
});

test('crate-root README and core/engine Markdown docs need no jobs or metadata', () => {
  const files = [
    'crates/store/README', 'crates/model/README.md', 'crates/core/readme.markdown', 'engine/README.md',
    'crates/core/docs/architecture.md', 'crates/core/docs/design/runtime.markdown',
    'engine/docs/usage.md', 'engine/docs/design/README.MD',
  ];
  for (const file of files) assert.deepEqual(selectChanges([file], noMetadata), empty, file);
  assert.deepEqual(selectChanges(files, noMetadata), empty);
});

test('crate fixtures and non-documentation assets still select reverse dependents', () => {
  const cases = [
    ['crates/store/tests/fixtures/daily-golden.json', '-p core -p server -p store'],
    ['crates/store/tests/fixtures/README.md', '-p core -p server -p store'],
    ['crates/core/src/prompt.md', '-p core -p server'],
    ['crates/core/tests/fixtures/docs/input.md', '-p core -p server'],
    ['crates/core/assets/schema.json', '-p core -p server'],
    ['crates/core/docs/example.json', '-p core -p server'],
    ['crates/core/docs/example.rs', '-p core -p server'],
    ['engine/src/prompt.markdown', '-p core -p engine -p server'],
    ['engine/tests/fixtures/README.md', '-p core -p engine -p server'],
    ['engine/config.toml', '-p core -p engine -p server'],
  ];
  for (const [file, packages] of cases) {
    assert.deepEqual(select([file]), { ...empty, rust: true, packages }, file);
  }
});

test('mixed crate docs and Rust changes keep focused reverse-dependent selection', () => {
  const docs = ['crates/core/README.md', 'crates/core/docs/runtime.md', 'engine/docs/usage.md'];
  for (const [file, packages] of [
    ['crates/store/src/lib.rs', '-p core -p server -p store'],
    ['crates/core/src/lib.rs', '-p core -p server'],
    ['engine/src/lib.rs', '-p core -p engine -p server'],
  ]) {
    assert.deepEqual(select([...docs, file]), { ...empty, rust: true, packages }, file);
  }
});

test('CI workflow, selector and runner changes select diagnostics without expanding app suites', () => {
  for (const file of [
    '.github/workflows/ci.yml', 'scripts/ci-changes.mjs', 'scripts/ci-changes.test.mjs',
    'scripts/ci-run.mjs', 'scripts/ci-run.test.mjs', 'scripts/ci-future.mjs',
  ]) {
    assert.deepEqual(selectChanges([file], noMetadata), {
      ...empty, tools: true, unit: file.endsWith('.test.mjs') ? file : '',
    }, file);
  }
});

test('tools selection stays independent from mixed focused app and Rust checks', () => {
  const files = ['web/App.tsx', 'crates/store/src/lib.rs', 'tests/one.test.ts', 'tests/ui.spec.ts'];
  assert.deepEqual(select(files), {
    ...empty, web: true, rust: true, packages: '-p core -p server -p store',
    unit: 'tests/one.test.ts', specs: 'tests/ui.spec.ts',
  });
  for (const file of ['scripts/ci-run.mjs', '.github/workflows/ci.yml']) {
    assert.deepEqual(select([...files, file]), { ...select(files), tools: true }, file);
  }
});

test('unrelated scripts, workflows and lookalike paths do not select tools', () => {
  for (const file of [
    'scripts/unrelated.mjs', 'scripts/ci.mjs', 'scripts/nested/ci-run.mjs',
    '.github/workflows/other.yml', '.github/workflows/ci.yaml', '.github/workflows/ci.yml.bak',
    'docs/scripts/ci-run.mjs', 'docs/.github/workflows/ci.yml',
  ]) {
    assert.deepEqual(selectChanges([file], noMetadata), empty, file);
  }
});

test('web selection preserves existing paths and config files without broadening other jobs', () => {
  for (const file of [
    'web/App.tsx', 'shared/types.ts', 'index.html', 'vite.config.ts', 'vite.config.mjs',
    'tsconfig.json', 'tsconfig.node.json', 'package.json', 'package-lock.json',
  ]) {
    assert.deepEqual(selectChanges([file], noMetadata), { ...empty, web: true }, file);
  }
});

test('frontend unit tests stay targeted without a Rust or standalone engine build', () => {
  for (const file of ['tests/frontend-state.test.ts', 'tests/frontend-composer.test.ts', 'tests/canvas-state.test.ts', 'tests/visualization-document.test.ts']) {
    assert.deepEqual(selectChanges([file], noMetadata), { ...empty, unit: file }, file);
  }
});

test('fixtures, legacy Node paths and browser configuration never broaden suites', () => {
  for (const file of [
    'tests/app-fixture.ts', 'tests/mock-provider.ts', 'tests/mcp-app-fixture.ts', 'playwright.config.ts',
    'server/app.ts', 'server/canvas.ts', 'scripts/build-engine.mjs', 'scripts/clip-lines.mjs',
  ]) {
    assert.deepEqual(selectChanges([file], noMetadata), empty, file);
  }
  assert.deepEqual(selectChanges(['server/app.ts', 'tests/frontend-state.test.ts'], noMetadata), {
    ...empty, unit: 'tests/frontend-state.test.ts',
  });
});

test('selects only changed top-level unit tests, script tests and Playwright specs', () => {
  assert.deepEqual(selectChanges([
    'tests/one.test.ts', 'tests/two.test.ts', 'tests/one.test.ts', 'scripts/service.test.mjs',
    'tests/ui.spec.ts', 'tests/ui.spec.ts', 'tests/another.spec.ts',
    'tests/nested/ignored.test.ts', 'tests/nested/ignored.spec.ts', 'tests/ignored.test.js',
    'scripts/nested/ignored.test.mjs', 'scripts/ignored.test.ts', 'scripts/service.mjs',
  ], noMetadata), {
    ...empty, unit: 'tests/one.test.ts tests/two.test.ts scripts/service.test.mjs', specs: 'tests/ui.spec.ts tests/another.spec.ts',
  });
});

test('ignores deleted unit, script and spec paths and resolves existence against the selected workspace', () => {
  const probes = [];
  const fileExists = absolute => {
    probes.push(absolute);
    return [path.join(cwd, 'tests/kept.test.ts'), path.join(cwd, 'scripts/kept.test.mjs'), path.join(cwd, 'tests/kept.spec.ts')].includes(absolute);
  };
  assert.deepEqual(selectChanges([
    ' tests/kept.test.ts ', 'tests/deleted.test.ts', 'scripts/kept.test.mjs', 'scripts/deleted.test.mjs',
    'tests/kept.spec.ts', 'tests/deleted.spec.ts', 'tests/kept.test.ts', 'tests/app-fixture.ts',
  ], { ...noMetadata, cwd, fileExists }), {
    ...empty, unit: 'tests/kept.test.ts scripts/kept.test.mjs', specs: 'tests/kept.spec.ts',
  });
  assert.deepEqual(probes, [
    'tests/kept.test.ts', 'tests/deleted.test.ts', 'scripts/kept.test.mjs', 'scripts/deleted.test.mjs',
    'tests/kept.spec.ts', 'tests/deleted.spec.ts',
  ].map(file => path.join(cwd, file)));
});

test('release selection is limited to the explicit standalone release paths', () => {
  assert.deepEqual(select(['crates/server/build.rs']), { ...empty, rust: true, packages: '-p server', release: true });
  assert.deepEqual(select(['crates/core/src/web.rs']), { ...empty, rust: true, packages: '-p core -p server', release: true });
  assert.deepEqual(selectChanges(['scripts/verify-release.mjs'], noMetadata), { ...empty, release: true });
  // Even documentation under crates/server/** is within the explicitly requested release gate.
  assert.deepEqual(selectChanges(['crates/server/README.md'], noMetadata), { ...empty, release: true });
  for (const file of [
    'web/App.tsx', 'shared/types.ts', 'tests/app-fixture.ts', 'tests/frontend-state.test.ts',
    'scripts/service.test.mjs', 'package.json', 'Cargo.toml', 'scripts/cargo.mjs',
    '.github/workflows/ci.yml', 'scripts/ci-changes.mjs',
  ]) {
    assert.equal(selectChanges([file], noMetadata).release, false, file);
  }
  for (const file of ['crates/core/src/http.rs', 'crates/store/src/lib.rs', 'engine/src/lib.rs']) {
    assert.equal(select([file]).release, false, file);
  }
});

test('CLI parses stdin and preserves focused web, unit and spec outputs without Cargo', () => {
  assert.deepEqual(outputs(cli([], 'shared/types.ts\r\n tests/frontend-state.test.ts \n\ntests/computer.spec.ts\n')), {
    ...emptyOutputs, web: 'true', unit: 'tests/frontend-state.test.ts', specs: 'tests/computer.spec.ts',
  });
});

test('CLI selects only existing script unit files and never deleted tests or specs', () => {
  assert.deepEqual(outputs(cli([], 'scripts/service.test.mjs\ntests/backend-api.test.ts\nscripts/no-longer-present.test.mjs\ntests/no-longer-present.spec.ts\n')), {
    ...emptyOutputs, unit: 'scripts/service.test.mjs',
  });
});

test('CLI infrastructure changes stay limited to changed script tests and CI diagnostics', () => {
  assert.deepEqual(outputs(cli([], 'scripts/ci-run.test.mjs\n')), {
    ...emptyOutputs, unit: 'scripts/ci-run.test.mjs', tools: 'true',
  });
  assert.deepEqual(outputs(cli([], '.github/workflows/ci.yml\n')), { ...emptyOutputs, tools: 'true' });
});

test('CLI release-verifier changes select only the standalone release job', () => {
  assert.deepEqual(outputs(cli([], 'scripts/verify-release.mjs\n')), { ...emptyOutputs, release: 'true' });
});

test('CLI empty and documentation-only changes stay focused without Cargo', () => {
  for (const input of ['', 'README.md\n', 'crates/core/README.md\ncrates/core/docs/runtime.md\nengine/docs/usage.md\n']) {
    assert.deepEqual(outputs(cli([], input)), emptyOutputs);
  }
});

test('CLI reports metadata failure on stderr and fails without partial outputs', () => {
  const result = cli([], 'crates/store/src/lib.rs\n');
  assert.equal(result.error, undefined);
  assert.equal(result.status, 1);
  assert.equal(result.stdout, '');
  assert.match(result.stderr, /cargo.*ENOENT/);
});

test('workflow detection preserves selector failure through tee', { skip: process.platform !== 'linux' }, () => {
  const workflow = readFileSync(new URL('../.github/workflows/ci.yml', import.meta.url), 'utf8');
  // GitHub's explicit bash shell runs with -e -o pipefail; implicit bash has only -e.
  assert.match(workflow, /- id: detect\n\s+shell: bash(?:\s+#.*)?\n/);
  const quote = value => `'${value.replaceAll("'", "'\\''")}'`;
  const pipeline = `printf 'crates/store/src/lib.rs\\n' | PATH= ${quote(process.execPath)} ${quote(script)} | /usr/bin/tee /dev/null`;
  const result = spawnSync('/bin/bash', ['--noprofile', '--norc', '-e', '-o', 'pipefail', '-c', pipeline], {
    encoding: 'utf8', timeout: 10_000,
  });
  assert.equal(result.error, undefined);
  assert.equal(result.status, 1);
  assert.equal(result.stdout, '');
  assert.match(result.stderr, /cargo.*ENOENT/);
});

test('workflow keeps Rust-only changed tests, log clipping and standalone release diagnostics', () => {
  const workflow = readFileSync(new URL('../.github/workflows/ci.yml', import.meta.url), 'utf8');
  assert.match(workflow, /unit: \$\{\{ steps\.detect\.outputs\.unit \}\}/);
  assert.match(workflow, /UNIT_TESTS: \$\{\{ needs\.changes\.outputs\.unit \}\}/);
  assert.match(workflow, /node scripts\/ci-run\.mjs npx tsx --test [^\n]+\$UNIT_TESTS 2>&1 \| node scripts\/clip-lines\.mjs/);
  assert.match(workflow, /node scripts\/ci-run\.mjs npx playwright test \$SPECS/);
  assert.match(workflow, /if: needs\.changes\.outputs\.release == 'true'/);
  assert.match(workflow, /node scripts\/ci-run\.mjs npm run build:release/);
  assert.match(workflow, /node scripts\/ci-run\.mjs node scripts\/verify-release\.mjs "\$CARGO_TARGET_DIR\/release\/grok-bot"/);
  assert.doesNotMatch(workflow, /build:engine|NODE_TESTS|outputs\.node|GROK_BOT_BACKEND|schedule:|workflow_dispatch:|= all|tests\/\*\.test\.ts/);
});

test('CLI rejects flags, including the removed --full mode', () => {
  for (const flag of ['--all', '--full']) {
    const result = cli([flag]);
    assert.equal(result.error, undefined);
    assert.equal(result.status, 1);
    assert.equal(result.stdout, '');
    assert.match(result.stderr, /Usage:/);
  }
});

test('importing the selector does not read stdin, invoke child processes or print outputs', () => {
  const source = `
    import fs from 'node:fs';
    import childProcess from 'node:child_process';
    import { syncBuiltinESMExports } from 'node:module';
    const read = fs.readFileSync;
    fs.readFileSync = (file, ...args) => {
      if (file === 0) throw new Error('stdin read during import');
      return read(file, ...args);
    };
    childProcess.execFileSync = () => { throw new Error('child process invoked during import'); };
    syncBuiltinESMExports();
    await import(${JSON.stringify(new URL('./ci-changes.mjs', import.meta.url).href)});
  `;
  const result = spawnSync(process.execPath, ['--input-type=module', '-e', source], {
    encoding: 'utf8', input: 'engine/src/lib.rs\n', timeout: 10_000,
  });
  assert.equal(result.error, undefined);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout, '');
  assert.equal(result.stderr, '');
});

test('actual locked no-deps metadata selects store, core and server but not engine', t => {
  let metadata;
  try { metadata = readCargoMetadata(); }
  catch (error) {
    if (error.code !== 'ENOENT') throw error;
    t.skip('Cargo is not installed; synthetic metadata tests still run');
    return;
  }
  assert.equal(metadata.resolve, null);
  assert.equal(selectChanges(['crates/store/src/lib.rs'], { loadMetadata: () => metadata }).packages,
    '-p grok-bot-core -p grok-bot-server -p grok-bot-store');
});
