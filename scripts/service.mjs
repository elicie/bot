// Linux user-service deployment. Run from a reviewed checkout, never from the live release.
import { spawn } from 'node:child_process';
import { chmod, copyFile, lstat, mkdir, mkdtemp, open, readFile, readdir, readlink, realpath, rename, rm, symlink, writeFile } from 'node:fs/promises';
import { homedir, userInfo } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const unit = 'grok-bot.service';
const keys = new Set(['HOST', 'PORT', 'GROK_BOT_ALLOWED_HOSTS', 'GROK_BOT_DATA_DIR', 'GROK_BOT_BACKUP_DIR', 'GROK_BOT_BACKUP_KEEP']);

export function parseConfig(text, home) {
  const values = { HOST: '127.0.0.1', PORT: '7777', GROK_BOT_DATA_DIR: path.join(home, '.local/share/grok-bot-q'),
    GROK_BOT_BACKUP_DIR: path.join(home, '.local/state/grok-bot/backups'), GROK_BOT_BACKUP_KEEP: '7' };
  const seen = new Set();
  for (const line of text.split('\n').map(line => line.trim()).filter(line => line && !line.startsWith('#'))) {
    const match = line.match(/^([A-Z_]+)=(.*)$/);
    if (!match || !keys.has(match[1]) || seen.has(match[1]) || /[\0\r"'`\\$]/.test(match[2])) {
      throw new Error('server.env requires unique supported KEY=literal lines, without quotes, escapes or shell expansion.');
    }
    seen.add(match[1]);
    values[match[1]] = match[2].trim();
  }
  if (!['127.0.0.1', '0.0.0.0'].includes(values.HOST)) throw new Error('HOST must be 127.0.0.1 or 0.0.0.0.');
  if (!/^\d+$/.test(values.PORT) || +values.PORT < 1 || +values.PORT > 65535) throw new Error('PORT must be 1..65535.');
  values.PORT = String(+values.PORT);
  if (!/^\d+$/.test(values.GROK_BOT_BACKUP_KEEP) || +values.GROK_BOT_BACKUP_KEEP < 1 || +values.GROK_BOT_BACKUP_KEEP > 1000) {
    throw new Error('GROK_BOT_BACKUP_KEEP must be 1..1000.');
  }
  for (const key of ['GROK_BOT_DATA_DIR', 'GROK_BOT_BACKUP_DIR']) {
    if (!path.isAbsolute(values[key]) || path.resolve(values[key]) === '/') throw new Error(`${key} must be an absolute non-root path.`);
    values[key] = path.resolve(values[key]);
  }
  for (const host of (values.GROK_BOT_ALLOWED_HOSTS ?? '').split(',').filter(Boolean)) {
    if (/[\s/@?#\\:*]/.test(host)) throw new Error('GROK_BOT_ALLOWED_HOSTS accepts comma-separated names without ports, URLs or wildcards.');
  }
  if (within(values.GROK_BOT_DATA_DIR, values.GROK_BOT_BACKUP_DIR)) throw new Error('Backups must be outside the data directory.');
  return values;
}

function within(parent, child) {
  const relative = path.relative(parent, child);
  return relative === '' || (!relative.startsWith(`..${path.sep}`) && relative !== '..' && !path.isAbsolute(relative));
}

async function syncPath(file) {
  const handle = await open(file, 'r');
  try { await handle.sync(); }
  finally { await handle.close(); }
}

export async function run(command, args, { capture = false } = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { cwd: root, stdio: capture ? ['ignore', 'pipe', 'pipe'] : ['ignore', 'inherit', 'inherit'] });
    let output = '';
    let error = '';
    child.stdout?.on('data', bytes => { output += bytes; });
    child.stderr?.on('data', bytes => { error += bytes; });
    child.once('error', reject);
    child.once('exit', (code, signal) => code === 0 ? resolve(output.trim()) : reject(new Error(`${command} failed (${signal ?? code}). ${error.trim()}`)));
  });
}

// Fetch never uses a proxy or redirects, and never logs the state response (it contains user data).
export async function request(port, route, method = 'GET') {
  const origin = new URL(`http://127.0.0.1:${port}`).origin;
  const response = await fetch(`${origin}${route}`, {
    method, redirect: 'error', signal: AbortSignal.timeout(5000),
    headers: { Origin: origin, 'Content-Type': 'application/json', 'X-Grok-Bot': '1' },
    ...(method === 'GET' ? {} : { body: '{}' }),
  });
  if (!response.ok) throw new Error(`${route} returned HTTP ${response.status}${response.status === 409 ? ': a reply is active; finish it or stop it in the app and retry' : ''}.`);
  return route.startsWith('/api/') ? response.json() : response.text();
}

export class Deployment {
  constructor({ home = homedir(), execute = run, http = request, wait = ms => new Promise(resolve => setTimeout(resolve, ms)) } = {}) {
    this.home = home;
    this.execute = execute;
    this.http = http;
    this.wait = wait;
    this.installDir = path.join(home, '.local/lib/grok-bot');
    this.releases = path.join(this.installDir, 'releases');
    this.current = path.join(this.installDir, 'current');
    this.configFile = path.join(home, '.config/grok-bot/server.env');
    this.unitFile = path.join(home, '.config/systemd/user', unit);
    this.lock = path.join(this.installDir, 'update.lock');
  }

  ctl(...args) { return this.execute('systemctl', ['--user', ...args], { capture: true }); }

  async disableAndStop() {
    const failures = [];
    // Disable first for reboot safety, but still try stopping if disabling itself fails.
    for (const command of ['disable', 'stop']) {
      try { await this.ctl(command, unit); }
      catch (error) { failures.push(`${command}: ${error.message}`); }
    }
    if (failures.length) throw new Error(failures.join('; '));
  }

  async loadConfig() {
    const config = parseConfig(await readFile(this.configFile, 'utf8'), this.home);
    // Resolve existing parents too: a symlink must not put the archive inside its source tree.
    this.data = await realpath(config.GROK_BOT_DATA_DIR);
    await mkdir(config.GROK_BOT_BACKUP_DIR, { recursive: true, mode: 0o700 });
    this.backups = await realpath(config.GROK_BOT_BACKUP_DIR);
    const locations = [this.data, this.backups, await realpath(this.installDir)];
    if (locations.some((a, i) => locations.some((b, j) => i !== j && within(a, b)))) {
      throw new Error('Data, installed releases and backups must not contain one another.');
    }
    await chmod(this.backups, 0o700);
    this.config = config;
  }

  async withLock(operation) {
    await mkdir(this.installDir, { recursive: true, mode: 0o700 });
    await chmod(this.installDir, 0o700);
    try { await mkdir(this.lock, { mode: 0o700 }); }
    catch (error) {
      if (error.code === 'EEXIST') throw new Error(`Another update holds ${this.lock}. If it crashed, verify no updater is running before removing that lock directory.`);
      throw error;
    }
    try {
      await writeFile(path.join(this.lock, 'pid'), `${process.pid}\n`, { mode: 0o600 });
      return await operation();
    } finally {
      // The resolved target is the exact private lock directory created above, never config input.
      if (path.resolve(this.lock) !== path.join(path.resolve(this.installDir), 'update.lock')) throw new Error('Unsafe lock path.');
      await rm(this.lock, { recursive: true });
    }
  }

  async stage() {
    await mkdir(this.releases, { recursive: true, mode: 0o700 });
    const id = new Date().toISOString().replace(/[-:]/g, '').replace(/\.\d+Z$/, 'Z');
    const release = await mkdtemp(path.join(this.releases, `${id}-`));
    // Build into a new immutable release; neither live web assets nor running binary is overwritten.
    await this.execute('npm', ['run', 'build', '--', '--outDir', path.join(release, 'dist')]);
    const target = path.resolve(root, process.env.CARGO_TARGET_DIR || 'target');
    // Embed this staged UI, not a missing or stale checkout dist/. Force only this Cargo build's
    // environment entry; do not mutate the updater process or overwrite live frontend assets.
    const webRoot = `env.GROK_BOT_BUILD_WEB_ROOT.value=${JSON.stringify(path.join(release, 'dist'))}`;
    await this.execute(process.execPath, ['scripts/cargo.mjs', 'build', '--locked', '--release', '--target-dir', target, '-p', 'grok-bot-server',
      '--config', webRoot, '--config', 'env.GROK_BOT_BUILD_WEB_ROOT.force=true']);
    await copyFile(path.join(target, 'release/grok-bot'), path.join(release, 'grok-bot'));
    await chmod(path.join(release, 'grok-bot'), 0o700);
    await lstat(path.join(release, 'dist/index.html'));
    return release;
  }

  async activate(release) {
    const resolved = await realpath(release);
    if (path.dirname(resolved) !== await realpath(this.releases)) throw new Error('Release is outside the managed releases directory.');
    const next = path.join(this.lock, 'current.next');
    await symlink(resolved, next);
    // Both paths are fixed children of this installation, not config-derived destinations.
    if (path.resolve(this.current) !== path.join(path.resolve(this.installDir), 'current')) throw new Error('Unsafe current path.');
    await rename(next, this.current);
    await syncPath(this.installDir);
  }

  async backup() {
    if (!(await lstat(path.join(this.data, 'state.json'))).isFile()) throw new Error('state.json must be a regular file.'); // Refuse a wrong/empty data directory.
    const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\.\d+Z$/, 'Z');
    const scratch = await mkdtemp(path.join(this.backups, '.pending-'));
    const archive = path.join(scratch, 'data.tar.gz');
    const destination = path.join(this.backups, `backup-${stamp}-${path.basename(scratch).slice(9)}.tar.gz`);
    try {
      await writeFile(archive, '', { flag: 'wx', mode: 0o600 });
      await this.execute('tar', ['-czf', archive, '-C', this.data, '.']);
      await this.execute('tar', ['-tzf', archive], { capture: true });
      await syncPath(archive);
      if (path.dirname(destination) !== this.backups) throw new Error('Unsafe archive destination.');
      await rename(archive, destination);
      await syncPath(this.backups);
      return destination;
    } finally {
      if (path.dirname(path.resolve(scratch)) !== this.backups || !path.basename(scratch).startsWith('.pending-')) throw new Error('Unsafe scratch path.');
      await rm(scratch, { recursive: true });
    }
  }

  async prune(newest) {
    const archives = [];
    for (const name of await readdir(this.backups)) {
      if (!/^backup-\d{8}T\d{6}Z-[a-zA-Z0-9]{6}\.tar\.gz$/.test(name)) continue;
      const file = path.join(this.backups, name);
      const stat = await lstat(file);
      if (stat.isFile()) archives.push({ file, time: stat.mtimeMs });
    }
    archives.sort((a, b) => Number(b.file === newest) - Number(a.file === newest) || b.time - a.time);
    for (const { file } of archives.slice(+this.config.GROK_BOT_BACKUP_KEEP)) {
      if (path.dirname(path.resolve(file)) !== this.backups) throw new Error('Unsafe retention target.');
      await rm(file);
    }
  }

  async healthy() {
    let last;
    for (let attempt = 0; attempt < 30; attempt++) {
      try {
        if (await this.ctl('is-active', unit) !== 'active') throw new Error('Service is not active.');
        const state = await this.http(this.config.PORT, '/api/state');
        if (!Array.isArray(state.bots) || !Array.isArray(state.conversations) || state.runtime?.available !== true) throw new Error('Unexpected Rust backend state.');
        const html = await this.http(this.config.PORT, '/');
        if (!/<html[\s>]/i.test(html)) throw new Error('Frontend is unavailable.');
        return;
      } catch (error) { last = error; }
      await this.wait(1000);
    }
    throw new Error(`Service did not become healthy: ${last?.message}`);
  }

  async update() {
    return this.withLock(async () => {
      await this.loadConfig();
      const previous = await realpath(this.current);
      if (path.dirname(previous) !== await realpath(this.releases)) throw new Error('Current release is not managed by this installer.');
      if (await this.ctl('is-active', unit) !== 'active') throw new Error('Start the installed service before updating.');
      const release = await this.stage();
      let stopRequested = false;
      let activated = false;
      let archive;
      try {
        // One server-side atomic check + latch: no GET-idle/stop race, including permission waits.
        const gate = await this.http(this.config.PORT, '/api/maintenance', 'POST');
        if (gate.maintenance !== true) throw new Error('Server does not support the maintenance gate; no restart performed.');
        if (typeof gate.dataDir !== 'string' || await realpath(gate.dataDir) !== this.data) {
          throw new Error('Configured data directory differs from the running server; no restart performed. Restore the original configuration before retrying.');
        }
        stopRequested = true;
        await this.ctl('stop', unit);
        if (await this.ctl('show', unit, '--property=ActiveState', '--value') !== 'inactive') throw new Error('Service did not stop cleanly; no backup/activation performed.');
        archive = await this.backup();
        await writeFile(path.join(release, 'deployment.json'), JSON.stringify({ backup: archive, previousRelease: previous }, null, 2), { mode: 0o600 });
        // A rename can succeed even if the following directory sync fails: recover conservatively.
        activated = true;
        await this.activate(release);
        await this.ctl('reset-failed', unit);
        await this.ctl('start', unit);
        await this.healthy();
      } catch (error) {
        let recovery = '';
        try {
          if (!stopRequested) {
            // Also handles a lost gate response. A failed gate must not strand the old service.
            await this.http(this.config.PORT, '/api/maintenance', 'DELETE');
          } else if (!activated) {
            const state = await this.ctl('show', unit, '--property=ActiveState', '--value');
            if (state === 'active') {
              await this.http(this.config.PORT, '/api/maintenance', 'DELETE');
            } else if (state === 'inactive' || state === 'failed') {
              await this.ctl('reset-failed', unit);
              await this.ctl('start', unit);
            } else {
              throw new Error(`Previous service is ${state}; wait for it to stop before recovery.`);
            }
            await this.healthy();
            recovery = ' The previous release was recovered.';
          } else {
            // Persistent safety: merely stopping is insufficient; an enabled unit would boot the old
            // binary against possibly migrated data after a power cycle.
            await this.disableAndStop();
            await this.activate(previous);
            recovery = ` The service was disabled and stopped; previous release ${previous} was reselected but NOT started. Restore from ${archive} before re-enabling it (see the runbook).`;
          }
        } catch (cleanup) { recovery += ` Recovery needs attention: ${cleanup.message}`; }
        throw new Error(`${error.message}${recovery}`);
      }
      await this.prune(archive);
      return { release, archive, previous };
    });
  }

  async install() {
    return this.withLock(async () => {
      try { await readlink(this.current); throw new Error('A release is already installed; use update instead.'); }
      catch (error) { if (error.code !== 'ENOENT') throw error; }
      const linger = await this.execute('loginctl', ['show-user', userInfo().username, '--property=Linger', '--value'], { capture: true });
      if (linger !== 'yes') throw new Error('Boot startup needs lingering. Run loginctl enable-linger "$USER" (administrator permission may be needed), then retry.');
      await mkdir(path.dirname(this.configFile), { recursive: true, mode: 0o700 });
      try {
        await writeFile(this.configFile, `# Literal values; no quotes, $HOME or ~ expansion. Paths must be absolute.\nHOST=127.0.0.1\nPORT=7777\nGROK_BOT_DATA_DIR=${path.join(this.home, '.local/share/grok-bot-q')}\nGROK_BOT_BACKUP_DIR=${path.join(this.home, '.local/state/grok-bot/backups')}\nGROK_BOT_BACKUP_KEEP=7\nGROK_BOT_ALLOWED_HOSTS=\n`, { flag: 'wx', mode: 0o600 });
      } catch (error) { if (error.code !== 'EEXIST') throw error; }
      const config = parseConfig(await readFile(this.configFile, 'utf8'), this.home);
      await mkdir(config.GROK_BOT_DATA_DIR, { recursive: true, mode: 0o700 });
      await this.loadConfig();
      // Refuse takeover of a manually running server, rather than interrupting its replies.
      try {
        await this.http(this.config.PORT, '/api/state');
        throw new Error('Port is occupied. Wait for replies to finish and stop the manual server before installing.');
      } catch (error) { if (error.cause?.code !== 'ECONNREFUSED') throw error; }
      const release = await this.stage();
      const state = await lstat(path.join(this.data, 'state.json')).catch(error => {
        if (error.code === 'ENOENT') return null;
        throw error;
      });
      const archive = state ? await this.backup() : undefined;
      await writeFile(path.join(release, 'deployment.json'), JSON.stringify({ backup: archive ?? null, previousRelease: null }, null, 2), { mode: 0o600 });
      await mkdir(path.dirname(this.unitFile), { recursive: true, mode: 0o700 });
      await writeFile(this.unitFile, await readFile(path.join(root, 'deploy/systemd', unit)), { flag: 'wx', mode: 0o600 });
      await this.activate(release);
      try {
        await this.ctl('daemon-reload');
        await this.ctl('enable', unit);
        await this.ctl('start', unit);
        await this.healthy();
      } catch (error) {
        let cleanup = '';
        try { await this.disableAndStop(); }
        catch (failure) { cleanup = ` Recovery needs attention: ${failure.message}`; }
        throw new Error(`${error.message}${cleanup}${archive ? ` Pre-install backup: ${archive}` : ''}`);
      }
      if (archive) await this.prune(archive);
      return { release, archive };
    });
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.umask(0o077);
  const command = process.argv[2];
  if (!['install', 'update'].includes(command) || process.argv.length !== 3) {
    console.error('Usage: node scripts/service.mjs install|update');
    process.exitCode = 1;
  } else {
    try {
      const result = await new Deployment()[command]();
      console.log(`Release service is healthy. Release: ${result.release}${result.archive ? `\nBackup: ${result.archive}` : ''}${result.previous ? `\nPrevious data-compatible release: ${result.previous}` : ''}`);
    } catch (error) {
      console.error(error.message);
      process.exitCode = 1;
    }
  }
}
