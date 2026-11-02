import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { readCargoMetadata, selectChanges } from './ci-changes.mjs';

const script = fileURLToPath(new URL('./ci-changes.mjs', import.meta.url));
const cwd = path.resolve('synthetic-workspace');
const empty = { web: false, rust: false, packages: '', node: '', specs: '' };
const full = { web: true, rust: true, packages: '--workspace', node: 'all', specs: 'all' };
const noMetadata = { loadMetadata: () => assert.fail('metadata must remain lazy') };
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
const select = (files, metadata = workspace()) => selectChanges(files, { cwd, loadMetadata: () => metadata });
const cli = (args = [], input = '') => spawnSync(process.execPath, [script, ...args], {
  encoding: 'utf8', input, timeout: 10_000, env: { ...process.env, PATH: '' },
});
const outputs = result => {
  assert.equal(result.error, undefined);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stderr, '');
  return Object.fromEntries(result.stdout.trimEnd().split('\n').map(line => line.split('=')));
};

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
    assert.deepEqual(select([file]), { ...empty, rust: true, packages }, file);
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

test('root manifests and Rust build helpers select the workspace without metadata', () => {
  for (const file of [
    'Cargo.toml', 'Cargo.lock', '.cargo/config.toml', 'rust-toolchain', 'rust-toolchain.toml',
    'scripts/cargo.mjs', 'scripts/rust-env.mjs', 'scripts/build-engine.mjs',
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

test('--full selection includes every check and bypasses metadata', () => {
  for (const files of [[], ['README.md'], ['crates/deleted/src/lib.rs']]) {
    assert.deepEqual(selectChanges(files, { ...noMetadata, full: true }), full);
  }
});

test('CI workflow, selector and runner changes exercise full CI without metadata', () => {
  for (const file of [
    '.github/workflows/ci.yml', 'scripts/ci-changes.mjs', 'scripts/ci-changes.test.mjs',
    'scripts/ci-run.mjs', 'scripts/ci-run.test.mjs',
  ]) {
    assert.deepEqual(selectChanges([file], noMetadata), full, file);
  }
});

test('web selection preserves existing paths and config files without broadening other jobs', () => {
  for (const file of [
    'web/App.tsx', 'shared/protocol.ts', 'index.html', 'vite.config.ts', 'vite.config.mjs',
    'tsconfig.json', 'tsconfig.node.json', 'package.json', 'package-lock.json',
  ]) {
    assert.deepEqual(selectChanges([file], noMetadata), { ...empty, web: true }, file);
  }
});

test('Node backend changes still run all Node tests but no unrelated suites', () => {
  assert.deepEqual(selectChanges(['server/index.ts', 'tests/one.test.ts'], noMetadata), { ...empty, node: 'all' });
});

test('otherwise selects only changed top-level Node tests and Playwright specs', () => {
  assert.deepEqual(selectChanges([
    'tests/one.test.ts', 'tests/two.test.ts', 'tests/one.test.ts',
    'tests/ui.spec.ts', 'tests/ui.spec.ts', 'tests/another.spec.ts',
    'tests/nested/ignored.test.ts', 'tests/nested/ignored.spec.ts', 'tests/ignored.test.js',
  ], noMetadata), {
    ...empty, node: 'tests/one.test.ts tests/two.test.ts', specs: 'tests/ui.spec.ts tests/another.spec.ts',
  });
});

test('CLI --full preserves the exact five output keys and all sentinels without Cargo', () => {
  assert.deepEqual(outputs(cli(['--full'])), { web: 'true', rust: 'true', packages: '--workspace', node: 'all', specs: 'all' });
});

test('CLI parses stdin and preserves focused web, Node and spec outputs without Cargo', () => {
  assert.deepEqual(outputs(cli([], 'shared/protocol.ts\r\n tests/one.test.ts \n\ntests/ui.spec.ts\n')), {
    web: 'true', rust: 'false', packages: '', node: 'tests/one.test.ts', specs: 'tests/ui.spec.ts',
  });
});

test('CLI infrastructure changes use the full sentinels without Cargo', () => {
  assert.deepEqual(outputs(cli([], 'scripts/ci-run.test.mjs\n')), {
    web: 'true', rust: 'true', packages: '--workspace', node: 'all', specs: 'all',
  });
});

test('CLI reports metadata failure on stderr and fails without partial outputs', () => {
  const result = cli([], 'crates/store/src/lib.rs\n');
  assert.equal(result.error, undefined);
  assert.equal(result.status, 1);
  assert.equal(result.stdout, '');
  assert.match(result.stderr, /cargo.*ENOENT/);
});

test('CLI rejects unknown flags rather than silently producing partial coverage', () => {
  const result = cli(['--all']);
  assert.equal(result.error, undefined);
  assert.equal(result.status, 1);
  assert.equal(result.stdout, '');
  assert.match(result.stderr, /Usage:.*\[--full\]/);
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
