// Run with: node --test scripts/service.test.mjs
// Only tar is real. npm, cargo, loginctl, systemctl and HTTP are fail-closed fakes.
// Keep these tests serial: stage() reads CARGO_TARGET_DIR from the process environment.
import assert from 'node:assert/strict';
import { copyFile, lstat, mkdir, mkdtemp, readFile, readdir, readlink, realpath, rm, symlink, utimes, writeFile } from 'node:fs/promises';
import { tmpdir, userInfo } from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { Deployment, parseConfig, request, run } from './service.mjs';

const unit = 'grok-bot.service';
const archivePattern = /^backup-\d{8}T\d{6}Z-[a-zA-Z0-9]{6}\.tar\.gz$/;
const stateResponse = () => ({ bots: [], conversations: [], runtime: { available: true } });

function deferred() {
  let resolve;
  const promise = new Promise(done => { resolve = done; });
  return { promise, resolve };
}

async function entries(directory) {
  try { return (await readdir(directory)).sort(); }
  catch (error) { if (error.code === 'ENOENT') return []; throw error; }
}

async function absent(file) {
  await assert.rejects(lstat(file), { code: 'ENOENT' });
}

async function fixture(t, { installed = true, seedConfig = true, seedData = true, seedUnit = installed } = {}) {
  const parent = await realpath(tmpdir());
  const temporary = await mkdtemp(path.join(parent, 'grok-bot-service-test-'));
  const home = path.join(temporary, 'home');
  const cargoTarget = path.join(temporary, 'cargo-target');
  const previousEnvironment = { HOME: process.env.HOME, CARGO_TARGET_DIR: process.env.CARGO_TARGET_DIR };
  t.after(async () => {
    for (const [key, value] of Object.entries(previousEnvironment)) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
    // The only recursive test cleanup is the exact mkdtemp directory above.
    assert.equal(path.dirname(path.resolve(temporary)), parent);
    assert.ok(path.basename(temporary).startsWith('grok-bot-service-test-'));
    await rm(temporary, { recursive: true, force: true });
  });
  process.env.HOME = home;
  process.env.CARGO_TARGET_DIR = cargoTarget;
  await mkdir(home, { recursive: true, mode: 0o700 });

  const f = {
    temporary, home, cargoTarget,
    data: path.join(home, '.local/share/grok-bot-q'),
    backups: path.join(home, '.local/state/grok-bot/backups'),
    previous: null,
    events: [], commands: [], httpCalls: [], waits: [],
    serviceState: installed ? 'active' : 'inactive',
    unitEnabled: installed,
    maintenance: false,
    manualListener: false,
    linger: 'yes',
    originalState: Buffer.from('{"fixture":"before update","bots":[],"conversations":[]}\n'),
    attachment: Buffer.from([0, 1, 2, 127, 128, 255]),
  };
  f.insideTemporary = file => {
    const resolved = path.resolve(file);
    assert.ok(resolved.startsWith(`${temporary}${path.sep}`), `not a test path: ${resolved}`);
    return resolved;
  };
  f.configText = (overrides = {}) => Object.entries({
    HOST: '127.0.0.1', PORT: '7777', GROK_BOT_DATA_DIR: f.data,
    GROK_BOT_BACKUP_DIR: f.backups, GROK_BOT_BACKUP_KEEP: '7', GROK_BOT_ALLOWED_HOSTS: '',
    ...overrides,
  }).map(([key, value]) => `${key}=${value}\n`).join('');

  f.execute = async (command, args, options = {}) => {
    f.commands.push({ command, args: [...args], options: { ...options } });
    if (command === 'npm') {
      assert.deepEqual(args.slice(0, 4), ['run', 'build', '--', '--outDir']);
      assert.equal(args.length, 5);
      const dist = f.insideTemporary(args[4]);
      assert.equal(path.dirname(path.dirname(dist)), f.deployment.releases);
      f.events.push('build:web');
      await f.beforeWebBuild?.();
      if (f.failBuild === 'web') throw new Error('fixture web build failed');
      await mkdir(dist, { recursive: true });
      await writeFile(path.join(dist, 'index.html'), '<html><body>fixture new release</body></html>');
      return '';
    }
    if (command === process.execPath) {
      assert.deepEqual(args, ['scripts/cargo.mjs', 'build', '--locked', '--release', '--target-dir', cargoTarget, '-p', 'grok-bot-server']);
      f.events.push('build:server');
      if (f.failBuild === 'server') throw new Error('fixture server build failed');
      const binary = f.insideTemporary(path.join(cargoTarget, 'release/grok-bot'));
      await mkdir(path.dirname(binary), { recursive: true });
      await writeFile(binary, 'fixture new binary; never executed\n');
      return '';
    }
    if (command === 'loginctl') {
      assert.deepEqual(args, ['show-user', userInfo().username, '--property=Linger', '--value']);
      assert.equal(options.capture, true);
      f.events.push('linger');
      return f.linger;
    }
    if (command === 'systemctl') {
      assert.equal(args[0], '--user');
      assert.equal(options.capture, true);
      const operation = args[1];
      if (operation !== 'daemon-reload') assert.equal(args[2], unit);
      switch (operation) {
        case 'is-active':
          f.events.push('is-active');
          return f.serviceState;
        case 'show':
          assert.deepEqual(args.slice(3), ['--property=ActiveState', '--value']);
          f.events.push(`show:${f.serviceState}`);
          return f.serviceState;
        case 'stop':
          f.events.push('stop');
          if (f.stopFailureState && !f.stopFailureConsumed) {
            f.stopFailureConsumed = true;
            f.serviceState = f.stopFailureState;
            throw new Error('fixture systemctl stop failed');
          }
          f.serviceState = 'inactive';
          return '';
        case 'start': {
          const selected = await realpath(f.deployment.current);
          const which = selected === f.previous ? 'previous' : 'new';
          f.events.push(`start:${which}`);
          f.serviceState = 'active';
          f.maintenance = false;
          if (which === 'new') await f.onNewStart?.();
          return '';
        }
        case 'enable':
        case 'disable':
          f.unitEnabled = operation === 'enable';
          f.events.push(operation);
          return '';
        case 'reset-failed':
        case 'daemon-reload':
          f.events.push(operation);
          return '';
        default: assert.fail(`unimplemented fake systemctl operation: ${operation}`);
      }
    }
    if (command === 'tar') {
      f.insideTemporary(args[1]);
      if (args[0] === '-czf') {
        assert.deepEqual(args.slice(2), ['-C', f.deployment.data, '.']);
        f.insideTemporary(f.deployment.data);
        assert.equal(f.serviceState, 'inactive', 'backup must happen only after a confirmed stop');
        if (f.previous) assert.equal(await realpath(f.deployment.current), f.previous, 'backup must precede activation');
        f.events.push('backup:create');
        if (f.failTar === 'create') throw new Error('fixture archive creation failed');
      } else {
        assert.deepEqual(args, ['-tzf', args[1]]);
        assert.equal(options.capture, true);
        f.events.push('backup:verify');
        if (f.failTar === 'verify') throw new Error('fixture archive verification failed');
      }
      return run(command, args, options);
    }
    assert.fail(`refusing unexpected real command: ${command} ${args.join(' ')}`);
  };

  f.http = async (port, route, method = 'GET') => {
    assert.equal(port, '7777');
    f.httpCalls.push({ port, route, method });
    if (route === '/api/maintenance' && method === 'POST') {
      f.events.push('gate');
      assert.equal(f.serviceState, 'active');
      if (f.gateFailure) {
        f.maintenance = Boolean(f.latchOnGateFailure);
        throw f.gateFailure;
      }
      f.maintenance = true;
      return f.gateResponse ?? { maintenance: true, dataDir: f.data };
    }
    if (route === '/api/maintenance' && method === 'DELETE') {
      f.events.push('resume');
      assert.equal(f.serviceState, 'active', 'do not try to resume a stopped server over HTTP');
      f.maintenance = false;
      return { maintenance: false };
    }
    assert.equal(method, 'GET');
    if (route === '/api/state') {
      if (f.serviceState !== 'active') {
        f.events.push('probe');
        if (f.manualListener) return stateResponse();
        throw new TypeError('fixture connection refused', { cause: { code: 'ECONNREFUSED' } });
      }
      f.events.push('health:state');
      const selected = await realpath(f.deployment.current);
      if (f.failNewHealth && selected !== f.previous) return { ...stateResponse(), runtime: { available: false } };
      return stateResponse();
    }
    if (route === '/') {
      f.events.push('health:html');
      assert.equal(f.serviceState, 'active');
      return '<!doctype html><html><body>fixture frontend</body></html>';
    }
    assert.fail(`refusing unexpected HTTP route: ${method} ${route}`);
  };
  f.wait = async ms => { assert.equal(ms, 1000); f.waits.push(ms); };
  f.deployment = new Deployment({ home, execute: f.execute, http: f.http, wait: f.wait });
  for (const key of ['home', 'installDir', 'releases', 'current', 'configFile', 'unitFile', 'lock']) {
    f.insideTemporary(f.deployment[key]);
  }
  await mkdir(f.deployment.installDir, { recursive: true, mode: 0o700 });
  if (seedConfig) {
    await mkdir(path.dirname(f.deployment.configFile), { recursive: true });
    await writeFile(f.deployment.configFile, f.configText(), { mode: 0o600 });
  }
  if (seedData) {
    await mkdir(path.join(f.data, 'attachments'), { recursive: true });
    await writeFile(path.join(f.data, 'state.json'), f.originalState);
    await writeFile(path.join(f.data, 'attachments/fixture.bin'), f.attachment);
  }
  if (installed) {
    f.previous = path.join(f.deployment.releases, 'previous-fixture');
    await mkdir(path.join(f.previous, 'dist'), { recursive: true });
    await writeFile(path.join(f.previous, 'grok-bot'), 'fixture previous binary; never executed\n');
    await writeFile(path.join(f.previous, 'dist/index.html'), '<html>fixture previous frontend</html>');
    await symlink(f.previous, f.deployment.current);
  }
  if (seedUnit) {
    await mkdir(path.dirname(f.deployment.unitFile), { recursive: true });
    await writeFile(f.deployment.unitFile, '# fixture previous unit; never loaded\n');
  }
  // Observe, but do not replace, the real symlink activation and retention operations.
  const activate = f.deployment.activate.bind(f.deployment);
  f.deployment.activate = async release => {
    await activate(release);
    f.events.push(release === f.previous ? 'activate:previous' : 'activate:new');
  };
  const prune = f.deployment.prune.bind(f.deployment);
  f.deployment.prune = async newest => { f.events.push('prune'); return prune(newest); };
  return f;
}

async function previousSnapshot(f) {
  const files = [
    f.deployment.configFile, f.deployment.unitFile,
    path.join(f.data, 'state.json'), path.join(f.data, 'attachments/fixture.bin'),
    path.join(f.previous, 'grok-bot'), path.join(f.previous, 'dist/index.html'),
  ];
  const selected = await lstat(f.deployment.current);
  return {
    current: { target: await readlink(f.deployment.current), ino: selected.ino, mtimeMs: selected.mtimeMs },
    files: await Promise.all(files.map(async file => {
      const stat = await lstat(file);
      return { file, bytes: await readFile(file), mtimeMs: stat.mtimeMs, mode: stat.mode };
    })),
  };
}

async function restoreIntoFreshDirectory(f, archive, name = 'restored') {
  f.insideTemporary(archive);
  const restored = f.insideTemporary(path.join(f.temporary, name));
  await mkdir(restored); // Never extract over the source, an installed release, or an existing directory.
  await run('tar', ['-xzf', archive, '-C', restored], { capture: true });
  return restored;
}

function assertNoRestartOrBackup(f) {
  assert.ok(!f.events.some(event => /^(stop|start:|backup:|activate:|prune)/.test(event)), f.events.join(', '));
}

test('config accepts documented defaults and literal overrides', () => {
  const home = '/fixture-home';
  assert.deepEqual(parseConfig('\n# comment\n', home), {
    HOST: '127.0.0.1', PORT: '7777',
    GROK_BOT_DATA_DIR: '/fixture-home/.local/share/grok-bot-q',
    GROK_BOT_BACKUP_DIR: '/fixture-home/.local/state/grok-bot/backups',
    GROK_BOT_BACKUP_KEEP: '7',
  });
  const config = parseConfig(' HOST=0.0.0.0\nPORT=65535\nGROK_BOT_DATA_DIR=/fixture/data/../data\nGROK_BOT_BACKUP_DIR=/fixture/backups\nGROK_BOT_BACKUP_KEEP=1000\nGROK_BOT_ALLOWED_HOSTS=localhost,bot.example,\n', home);
  assert.equal(config.HOST, '0.0.0.0');
  assert.equal(config.PORT, '65535');
  assert.equal(config.GROK_BOT_DATA_DIR, '/fixture/data');
  assert.equal(config.GROK_BOT_BACKUP_KEEP, '1000');
  assert.equal(config.GROK_BOT_ALLOWED_HOSTS, 'localhost,bot.example,');
  assert.equal(parseConfig('PORT=007777\n', home).PORT, '7777', 'HTTP Origin must use the runtime port, not its leading-zero spelling');
});

test('config rejects unsupported, ambiguous, unsafe and out-of-range settings', async t => {
  const invalid = [
    ['unknown key', 'UNSUPPORTED=value'],
    ['duplicate key', 'PORT=7777\nPORT=8888'],
    ['not an assignment', 'PORT 7777'],
    ['shell export', 'export PORT=7777'],
    ['lowercase key', 'port=7777'],
    ...['"value"', "'value'", '`value`', '$HOME/data', 'back\\slash', 'nul\0byte', 'embedded\rcarriage'].map(value => [`nonliteral ${JSON.stringify(value)}`, `GROK_BOT_DATA_DIR=/fixture/${value}`]),
    ['non-loopback/non-wildcard host', 'HOST=192.0.2.1'],
    ['IPv6 host', 'HOST=::1'],
    ...['0', '65536', '-1', '1.5', '', 'NaN', '1e3'].map(value => [`port ${JSON.stringify(value)}`, `PORT=${value}`]),
    ...['0', '1001', '-1', '1.5', ''].map(value => [`keep ${JSON.stringify(value)}`, `GROK_BOT_BACKUP_KEEP=${value}`]),
    ['relative data', 'GROK_BOT_DATA_DIR=relative/data'],
    ['root data', 'GROK_BOT_DATA_DIR=/'],
    ['relative backups', 'GROK_BOT_BACKUP_DIR=~/backups'],
    ['root backups', 'GROK_BOT_BACKUP_DIR=/tmp/..'],
    ['backups equal data', 'GROK_BOT_DATA_DIR=/fixture/data\nGROK_BOT_BACKUP_DIR=/fixture/data'],
    ['backups within data', 'GROK_BOT_DATA_DIR=/fixture/data\nGROK_BOT_BACKUP_DIR=/fixture/data/backups'],
    ...['https://bot.example', 'bot.example:7777', '*.example', 'two names', 'name/path', 'name@host', 'name?query', 'name#fragment'].map(value => [`allowed host ${value}`, `GROK_BOT_ALLOWED_HOSTS=${value}`]),
  ];
  for (const [name, text] of invalid) {
    await t.test(name, () => assert.throws(() => parseConfig(text, '/fixture-home')));
  }
});

test('loadConfig rejects symlink aliases and all containment directions', async t => {
  const cases = ['symlink backups into data', 'backups contain data', 'backups within installation', 'backups contain installation', 'data within installation', 'data contain installation'];
  for (const name of cases) {
    await t.test(name, async t => {
      const f = await fixture(t);
      let data = f.data;
      let backups = f.backups;
      if (name === 'symlink backups into data') {
        const nested = path.join(data, 'nested-backups');
        await mkdir(nested);
        backups = path.join(f.home, 'backup-alias');
        await symlink(nested, backups);
      } else if (name === 'backups contain data') {
        backups = path.dirname(data);
      } else if (name === 'backups within installation') {
        backups = path.join(f.deployment.installDir, 'archives');
      } else if (name === 'backups contain installation') {
        backups = path.join(f.home, '.local/lib');
      } else if (name === 'data within installation') {
        data = path.join(f.deployment.installDir, 'data');
        await mkdir(data);
      } else {
        data = path.join(f.home, '.local');
        backups = path.join(f.temporary, 'independent-backups');
      }
      await writeFile(f.deployment.configFile, f.configText({ GROK_BOT_DATA_DIR: data, GROK_BOT_BACKUP_DIR: backups }));
      await assert.rejects(f.deployment.loadConfig(), /must not contain one another|outside the data/);
      assert.deepEqual(f.commands, []);
      assert.deepEqual(f.httpCalls, []);
    });
  }
});

test('update builds before atomic maintenance, then stops, backs up, activates, starts and checks health', async t => {
  const f = await fixture(t);
  const result = await f.deployment.update();
  assert.deepEqual(f.events, [
    'is-active', 'build:web', 'build:server', 'gate', 'stop', 'show:inactive',
    'backup:create', 'backup:verify', 'activate:new', 'reset-failed', 'start:new',
    'is-active', 'health:state', 'health:html', 'prune',
  ]);
  assert.deepEqual(f.httpCalls.map(({ route, method }) => [method, route]), [
    ['POST', '/api/maintenance'], ['GET', '/api/state'], ['GET', '/'],
  ]); // No GET-idle / stop race: a single POST owns the atomic gate.
  assert.equal(await realpath(f.deployment.current), result.release);
  assert.deepEqual(JSON.parse(await readFile(path.join(result.release, 'deployment.json'), 'utf8')), { backup: result.archive, previousRelease: f.previous });
  assert.equal(result.previous, f.previous);
  assert.equal(f.serviceState, 'active');
  assert.equal(f.maintenance, false);
  assert.match(path.basename(result.archive), archivePattern);
  assert.equal((await lstat(result.archive)).mode & 0o777, 0o600);
  assert.equal((await lstat(f.backups)).mode & 0o777, 0o700);
  assert.equal((await lstat(path.join(result.release, 'grok-bot'))).mode & 0o777, 0o700);
  assert.deepEqual(await entries(f.backups), [path.basename(result.archive)]);
  const restored = await restoreIntoFreshDirectory(f, result.archive);
  assert.deepEqual(await readFile(path.join(restored, 'state.json')), f.originalState);
  assert.deepEqual(await readFile(path.join(restored, 'attachments/fixture.bin')), f.attachment);
  assert.deepEqual(await readFile(path.join(f.data, 'state.json')), f.originalState);
  assert.equal(await readFile(path.join(f.previous, 'grok-bot'), 'utf8'), 'fixture previous binary; never executed\n');
  await absent(f.deployment.lock);
});

test('active 409 leaves previous service, data and release files untouched and resumes', async t => {
  const f = await fixture(t);
  const before = await previousSnapshot(f);
  f.replyActive = true;
  f.gateFailure = new Error('/api/maintenance returned HTTP 409: a reply is active; finish it or stop it in the app and retry.');
  await assert.rejects(f.deployment.update(), /HTTP 409.*reply is active/);
  assert.deepEqual(f.events, ['is-active', 'build:web', 'build:server', 'gate', 'resume']);
  assertNoRestartOrBackup(f);
  assert.deepEqual(await previousSnapshot(f), before);
  assert.equal(f.serviceState, 'active');
  assert.equal(f.unitEnabled, true);
  assert.equal(f.replyActive, true);
  assert.equal(f.maintenance, false);
  assert.deepEqual(await entries(f.backups), []);
  await absent(f.deployment.lock);
});

test('lost maintenance response releases the old server maintenance latch', async t => {
  const f = await fixture(t);
  f.gateFailure = new Error('fixture response lost after latching');
  f.latchOnGateFailure = true;
  await assert.rejects(f.deployment.update(), /response lost/);
  assert.equal(f.maintenance, false);
  assert.equal(f.serviceState, 'active');
  assert.equal(await realpath(f.deployment.current), f.previous);
  assertNoRestartOrBackup(f);
  assert.equal(f.events.at(-1), 'resume');
});

test('invalid maintenance contract or runtime data-directory drift refuses stop and resumes', async t => {
  for (const name of ['unsupported gate', 'missing data directory', 'different data directory']) {
    await t.test(name, async t => {
      const f = await fixture(t);
      const before = await previousSnapshot(f);
      const otherData = path.join(f.temporary, 'other-data');
      await mkdir(otherData);
      f.gateResponse = name === 'unsupported gate' ? { maintenance: false }
        : name === 'missing data directory' ? { maintenance: true }
          : { maintenance: true, dataDir: otherData };
      await assert.rejects(f.deployment.update(), /maintenance gate|data directory differs/);
      assert.equal(f.events.at(-1), 'resume');
      assert.equal(f.maintenance, false);
      assert.equal(f.serviceState, 'active');
      assertNoRestartOrBackup(f);
      assert.deepEqual(await previousSnapshot(f), before);
      await absent(f.deployment.lock);
    });
  }
});

test('failed web or server build never gates, stops or backs up the old service', async t => {
  for (const build of ['web', 'server']) {
    await t.test(build, async t => {
      const f = await fixture(t);
      const before = await previousSnapshot(f);
      f.failBuild = build;
      await assert.rejects(f.deployment.update(), new RegExp(`fixture ${build} build failed`));
      assertNoRestartOrBackup(f);
      assert.deepEqual(f.httpCalls, []);
      assert.equal(f.serviceState, 'active');
      assert.equal(f.unitEnabled, true);
      assert.equal(f.maintenance, false);
      assert.deepEqual(await entries(f.backups), []);
      assert.deepEqual(await previousSnapshot(f), before);
      await absent(f.deployment.lock);
    });
  }
});

test('failed archive creation or verification cleans partial backup and restarts the previous service', async t => {
  for (const failure of ['create', 'verify']) {
    await t.test(failure, async t => {
      const f = await fixture(t);
      const before = await previousSnapshot(f);
      f.failTar = failure;
      await assert.rejects(f.deployment.update(), /fixture archive .* failed.*previous release was recovered/i);
      assert.equal(f.serviceState, 'active');
      assert.equal(f.unitEnabled, true);
      assert.equal(f.maintenance, false);
      assert.equal(await realpath(f.deployment.current), f.previous);
      assert.equal(f.events.filter(event => event === 'stop').length, 1);
      assert.deepEqual(f.events.filter(event => event.startsWith('start:')), ['start:previous']);
      assert.ok(!f.events.some(event => event.startsWith('activate:') || event === 'prune'));
      assert.ok(f.events.indexOf('backup:create') < f.events.indexOf('start:previous'));
      assert.ok(f.events.indexOf('start:previous') < f.events.indexOf('health:state'));
      assert.deepEqual(await entries(f.backups), []);
      assert.deepEqual(await previousSnapshot(f), before);
      await absent(f.deployment.lock);
    });
  }
});

test('backup refuses missing state.json, directories and symlinks without archiving another path', async t => {
  for (const kind of ['missing', 'directory', 'symlink']) {
    await t.test(kind, async t => {
      const f = await fixture(t, { seedData: false });
      await mkdir(f.data, { recursive: true });
      const state = path.join(f.data, 'state.json');
      if (kind === 'directory') await mkdir(state);
      if (kind === 'symlink') {
        const external = path.join(f.temporary, 'external-state.json');
        await writeFile(external, 'fixture external state\n');
        await symlink(external, state);
      }
      await f.deployment.loadConfig();
      await assert.rejects(f.deployment.backup(), kind === 'missing' ? { code: 'ENOENT' } : /regular file/);
      assert.deepEqual(f.commands, []);
      assert.deepEqual(await entries(f.backups), []);
    });
  }
});

test('a failed stop recovers according to actual ActiveState without backup or activation', async t => {
  for (const state of ['active', 'inactive']) {
    await t.test(state, async t => {
      const f = await fixture(t);
      f.stopFailureState = state;
      await assert.rejects(f.deployment.update(), /fixture systemctl stop failed.*previous release was recovered/i);
      assert.equal(f.serviceState, 'active');
      assert.equal(f.maintenance, false);
      assert.equal(await realpath(f.deployment.current), f.previous);
      assert.ok(!f.events.some(event => /^(backup:|activate:|prune)/.test(event)));
      if (state === 'active') {
        assert.ok(f.events.includes('resume'));
        assert.ok(!f.events.includes('start:previous'));
      } else {
        assert.ok(!f.events.includes('resume'));
        assert.ok(f.events.includes('reset-failed'));
        assert.ok(f.events.includes('start:previous'));
      }
      assert.ok(f.events.includes('health:html'));
      await absent(f.deployment.lock);
    });
  }
});

test('failed post-activation health stops new service, selects previous and requires explicit data restore', async t => {
  const f = await fixture(t);
  const migratedState = Buffer.from('{"fixture":"possibly migrated by new release"}\n');
  f.failNewHealth = true;
  f.onNewStart = () => writeFile(path.join(f.data, 'state.json'), migratedState);
  let failure;
  await assert.rejects(f.deployment.update(), error => {
    failure = error;
    return /Service did not become healthy/.test(error.message);
  });
  const archives = (await entries(f.backups)).filter(name => archivePattern.test(name));
  assert.equal(archives.length, 1);
  const archive = path.join(f.backups, archives[0]);
  assert.ok(failure.message.includes(`previous release ${f.previous} was reselected but NOT started`));
  assert.ok(failure.message.includes(`Restore from ${archive} before re-enabling`));
  assert.equal(f.serviceState, 'inactive');
  assert.equal(f.unitEnabled, false, 'a reboot must not start the old binary against possibly migrated data');
  assert.deepEqual(f.events.slice(-3), ['disable', 'stop', 'activate:previous']);
  assert.equal(await realpath(f.deployment.current), f.previous);
  assert.deepEqual(f.events.filter(event => event.startsWith('start:')), ['start:new']);
  assert.deepEqual(f.events.filter(event => event.startsWith('activate:')), ['activate:new', 'activate:previous']);
  assert.equal(f.events.filter(event => event === 'stop').length, 2);
  assert.equal(f.waits.length, 30);
  assert.ok(!f.events.includes('prune'));
  assert.deepEqual(await readFile(path.join(f.data, 'state.json')), migratedState, 'never automatically roll back possibly migrated data');
  assert.deepEqual(f.commands.filter(call => call.command === 'tar').map(call => call.args[0]), ['-czf', '-tzf']);
  const restored = await restoreIntoFreshDirectory(f, archive);
  assert.deepEqual(await readFile(path.join(restored, 'state.json')), f.originalState);
  assert.deepEqual(await readFile(path.join(restored, 'attachments/fixture.bin')), f.attachment);
  assert.deepEqual(await readFile(path.join(f.data, 'state.json')), migratedState, 'manual restore demonstration uses a separate empty directory');
  await absent(f.deployment.lock);
});

test('retention keeps requested count and newest backup, ignoring symlinks, directories and unmatched names', async t => {
  const f = await fixture(t);
  f.serviceState = 'inactive';
  await f.deployment.loadConfig();
  const newest = await f.deployment.backup();
  await utimes(newest, 1, 1); // The just-created archive must survive even if another file has a newer mtime.
  const older = [];
  for (let index = 0; index < 4; index++) {
    const file = path.join(f.backups, `backup-2026010${index + 1}T000000Z-OLD00${index}.tar.gz`);
    await copyFile(newest, file);
    await utimes(file, 100 + index, 100 + index);
    older.push(file);
  }
  const outside = path.join(f.temporary, 'outside-retention.tar.gz');
  await copyFile(newest, outside);
  const linked = path.join(f.backups, 'backup-20260109T000000Z-LINK00.tar.gz');
  const dangling = path.join(f.backups, 'backup-20260109T000001Z-LINK01.tar.gz');
  const directory = path.join(f.backups, 'backup-20260109T000002Z-DIR000.tar.gz');
  await symlink(outside, linked);
  await symlink(path.join(f.temporary, 'missing'), dangling);
  await mkdir(directory);
  const unmatched = ['README.txt', '.pending-unrelated', 'backup-invalid-ABC123.tar.gz', 'backup-20260101T000000Z-SHORT.tar.gz'];
  for (const name of unmatched) await writeFile(path.join(f.backups, name), 'fixture not managed by retention\n');

  f.deployment.config.GROK_BOT_BACKUP_KEEP = '2';
  await f.deployment.prune(newest);
  const matchedRegular = [];
  for (const name of await entries(f.backups)) {
    if (archivePattern.test(name) && (await lstat(path.join(f.backups, name))).isFile()) matchedRegular.push(name);
  }
  assert.deepEqual(matchedRegular.sort(), [path.basename(newest), path.basename(older.at(-1))].sort());
  for (const file of older.slice(0, -1)) await absent(file);
  assert.equal((await lstat(linked)).isSymbolicLink(), true);
  assert.equal(await readlink(linked), outside);
  assert.equal((await lstat(dangling)).isSymbolicLink(), true);
  assert.equal((await lstat(directory)).isDirectory(), true);
  assert.deepEqual(await readFile(outside), await readFile(newest));
  for (const name of unmatched) assert.equal(await readFile(path.join(f.backups, name), 'utf8'), 'fixture not managed by retention\n');

  f.deployment.config.GROK_BOT_BACKUP_KEEP = '1';
  await f.deployment.prune(newest);
  await absent(older.at(-1));
  assert.equal((await lstat(newest)).isFile(), true);
});

test('concurrent updater fails the lock without touching the first updater, then lock is released', async t => {
  const f = await fixture(t);
  const enteredBuild = deferred();
  const finishBuild = deferred();
  f.beforeWebBuild = async () => { enteredBuild.resolve(); await finishBuild.promise; };
  const first = f.deployment.update();
  await enteredBuild.promise;
  try {
    assert.equal(await readFile(path.join(f.deployment.lock, 'pid'), 'utf8'), `${process.pid}\n`);
    const callsBefore = f.commands.length;
    const second = new Deployment({ home: f.home, execute: f.execute, http: f.http, wait: f.wait });
    await assert.rejects(second.update(), /Another update holds .*update\.lock/);
    assert.equal(f.commands.length, callsBefore);
    assert.equal((await lstat(f.deployment.lock)).isDirectory(), true);
    assert.equal(await realpath(f.deployment.current), f.previous);
  } finally {
    finishBuild.resolve();
    await first;
  }
  await absent(f.deployment.lock);
  assert.equal(await f.deployment.withLock(async () => 'lock reusable'), 'lock reusable');
  await assert.rejects(f.deployment.withLock(async () => { throw new Error('fixture locked operation failed'); }), /locked operation failed/);
  await absent(f.deployment.lock);
});

test('new install requires confirmed linger before creating config, building or touching services', async t => {
  for (const linger of ['no', '']) {
    await t.test(JSON.stringify(linger), async t => {
      const f = await fixture(t, { installed: false, seedConfig: false, seedData: false });
      f.linger = linger;
      await assert.rejects(f.deployment.install(), /enable-linger/);
      assert.deepEqual(f.events, ['linger']);
      assert.deepEqual(f.httpCalls, []);
      await absent(f.deployment.configFile);
      await absent(f.data);
      await absent(f.deployment.unitFile);
      await absent(f.deployment.current);
      await absent(f.deployment.lock);
    });
  }
});

test('new install refuses to take over an existing manual listener or interrupt its replies', async t => {
  const f = await fixture(t, { installed: false, seedUnit: true });
  f.manualListener = true;
  const configBefore = await readFile(f.deployment.configFile);
  const unitBefore = await readFile(f.deployment.unitFile);
  await assert.rejects(f.deployment.install(), /Port is occupied.*stop the manual server/);
  assert.deepEqual(f.events, ['linger', 'probe']);
  assert.deepEqual(f.httpCalls.map(({ route, method }) => [method, route]), [['GET', '/api/state']]);
  assert.equal(f.manualListener, true);
  assertNoRestartOrBackup(f);
  assert.deepEqual(await readFile(f.deployment.configFile), configBefore);
  assert.deepEqual(await readFile(f.deployment.unitFile), unitBefore);
  assert.deepEqual(await readFile(path.join(f.data, 'state.json')), f.originalState);
  await absent(f.deployment.current);
  await absent(f.deployment.lock);
});

test('new install creates private default config and starts only after linger and an unoccupied-port check', async t => {
  const f = await fixture(t, { installed: false, seedConfig: false, seedData: false });
  const result = await f.deployment.install();
  assert.deepEqual(f.events, [
    'linger', 'probe', 'build:web', 'build:server', 'activate:new', 'daemon-reload',
    'enable', 'start:new', 'is-active', 'health:state', 'health:html',
  ]);
  assert.equal(f.serviceState, 'active');
  assert.equal(await realpath(f.deployment.current), result.release);
  assert.equal((await lstat(f.deployment.configFile)).mode & 0o777, 0o600);
  assert.equal((await lstat(f.deployment.installDir)).mode & 0o777, 0o700);
  assert.equal((await lstat(f.backups)).mode & 0o777, 0o700);
  assert.equal((await lstat(f.data)).isDirectory(), true);
  const config = parseConfig(await readFile(f.deployment.configFile, 'utf8'), f.home);
  assert.equal(config.GROK_BOT_DATA_DIR, f.data);
  assert.equal(config.GROK_BOT_BACKUP_DIR, f.backups);
  assert.equal(config.GROK_BOT_BACKUP_KEEP, '7');
  assert.equal(config.HOST, '127.0.0.1');
  assert.equal(config.PORT, '7777');
  const unitText = await readFile(f.deployment.unitFile, 'utf8');
  assert.equal(unitText, await readFile(new URL('../deploy/systemd/grok-bot.service', import.meta.url), 'utf8'));
  assert.match(unitText, /^Environment=HOST=127\.0\.0\.1$/m);
  assert.match(unitText, /^Environment=PORT=7777$/m);
  assert.ok(unitText.indexOf('Environment=PORT=7777') < unitText.indexOf('EnvironmentFile='), 'configured values must override service defaults');
  assert.deepEqual(await entries(f.backups), []);
  assert.ok(!f.commands.some(call => call.command === 'tar'));
  await absent(f.deployment.lock);
});

test('first install archives existing data before the first service start', async t => {
  const f = await fixture(t, { installed: false });
  const migratedState = Buffer.from('{"fixture":"first install migrated data"}\n');
  f.onNewStart = () => writeFile(path.join(f.data, 'state.json'), migratedState);
  const result = await f.deployment.install();
  assert.match(path.basename(result.archive), archivePattern);
  assert.deepEqual(f.events, [
    'linger', 'probe', 'build:web', 'build:server', 'backup:create', 'backup:verify',
    'activate:new', 'daemon-reload', 'enable', 'start:new', 'is-active', 'health:state', 'health:html', 'prune',
  ]);
  const restored = await restoreIntoFreshDirectory(f, result.archive);
  assert.deepEqual(await readFile(path.join(restored, 'state.json')), f.originalState);
  assert.deepEqual(await readFile(path.join(restored, 'attachments/fixture.bin')), f.attachment);
  assert.deepEqual(await readFile(path.join(f.data, 'state.json')), migratedState);
  await absent(f.deployment.lock);
});

test('failed first-install backup never replaces an existing unit or starts the new service', async t => {
  const f = await fixture(t, { installed: false, seedUnit: true });
  const configBefore = await readFile(f.deployment.configFile);
  const unitBefore = await readFile(f.deployment.unitFile);
  f.failTar = 'verify';
  await assert.rejects(f.deployment.install(), /fixture archive verification failed/);
  assert.deepEqual(f.events, ['linger', 'probe', 'build:web', 'build:server', 'backup:create', 'backup:verify']);
  assert.equal(f.serviceState, 'inactive');
  assert.deepEqual(await readFile(f.deployment.configFile), configBefore);
  assert.deepEqual(await readFile(f.deployment.unitFile), unitBefore);
  assert.deepEqual(await readFile(path.join(f.data, 'state.json')), f.originalState);
  assert.deepEqual(await entries(f.backups), []);
  await absent(f.deployment.current);
  await absent(f.deployment.lock);
});

test('first install refuses to overwrite an unrelated existing unit', async t => {
  const f = await fixture(t, { installed: false, seedUnit: true });
  const unitBefore = await readFile(f.deployment.unitFile);
  await assert.rejects(f.deployment.install(), { code: 'EEXIST' });
  assert.deepEqual(await readFile(f.deployment.unitFile), unitBefore);
  assert.ok(!f.events.includes('enable'));
  assert.ok(!f.events.includes('start:new'));
  await absent(f.deployment.current);
  await absent(f.deployment.lock);
});

test('install refuses an already managed release before inspecting linger or services', async t => {
  const f = await fixture(t);
  const before = await previousSnapshot(f);
  await assert.rejects(f.deployment.install(), /already installed; use update/);
  assert.deepEqual(f.commands, []);
  assert.deepEqual(f.httpCalls, []);
  assert.deepEqual(await previousSnapshot(f), before);
  await absent(f.deployment.lock);
});

test('request stays on loopback, does not redirect, and explains an active 409 without network access', async t => {
  const originalFetch = globalThis.fetch;
  t.after(() => { globalThis.fetch = originalFetch; });
  const calls = [];
  let status = 200;
  globalThis.fetch = async (url, options) => {
    calls.push({ url, options });
    return { ok: status === 200, status, json: async () => stateResponse(), text: async () => '<html>fixture</html>' };
  };
  assert.deepEqual(await request('17777', '/api/state'), stateResponse());
  assert.equal(await request('17777', '/'), '<html>fixture</html>');
  await request('17777', '/api/maintenance', 'POST');
  await request('17777', '/api/maintenance', 'DELETE');
  for (const { url, options } of calls) {
    assert.ok(url.startsWith('http://127.0.0.1:17777/'));
    assert.equal(options.redirect, 'error');
    assert.equal(options.headers.Origin, 'http://127.0.0.1:17777');
    assert.equal(options.headers['X-Grok-Bot'], '1');
    assert.ok(options.signal instanceof AbortSignal);
    assert.equal(options.body, options.method === 'GET' ? undefined : '{}');
  }
  await request('80', '/api/maintenance', 'POST');
  assert.equal(calls.at(-1).url, 'http://127.0.0.1/api/maintenance');
  assert.equal(calls.at(-1).options.headers.Origin, 'http://127.0.0.1');
  status = 409;
  await assert.rejects(request('17777', '/api/maintenance', 'POST'), /HTTP 409.*reply is active.*retry/);
});
