import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';

const runnerUrl = new URL('./ci-run.mjs', import.meta.url);
const runner = fileURLToPath(runnerUrl);
const run = (source, env = {}) => spawnSync(process.execPath, [runner, process.execPath, '-e', source], {
  encoding: 'utf8', timeout: 15_000, env: { ...process.env, ...env },
});

test('prints resources before and after a successful command and preserves its output', () => {
  const result = run('console.log("child-output")');
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /before command/);
  assert.match(result.stdout, /child-output/);
  assert.match(result.stdout, /command finished: exit=0 signal=none/);
  assert.match(result.stdout, /after command/);
  assert.match(result.stdout, /Filesystem/);
});

test('preserves a failing exit code even when diagnostic commands are unavailable', () => {
  const result = run('process.exit(7)', { PATH: '' });
  assert.equal(result.status, 7, result.stderr);
  assert.match(result.stdout, /df unavailable/);
  assert.match(result.stdout, /command finished: exit=7/);
});

test('records heartbeats while a command is silent', () => {
  const result = run('setTimeout(() => {}, 350)', { CI_RESOURCE_INTERVAL_MS: '50' });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /heartbeat/);
});

test('records a command killed by a signal and returns the conventional exit code', () => {
  const result = run('process.kill(process.pid, "SIGKILL")');
  assert.equal(result.status, 137, result.stderr);
  assert.match(result.stdout, /signal=SIGKILL/);
  assert.match(result.stdout, /after command/);
});

test('reports a command that cannot start', () => {
  const result = spawnSync(process.execPath, [runner, '/nonexistent-ci-command'], { encoding: 'utf8', timeout: 15_000 });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /command could not start/);
});

test('rejects invalid and overflowing intervals instead of spinning', () => {
  for (const interval of ['0', '-1', 'NaN', '1.5', '2147483648']) {
    const result = run('process.exit(0)', { CI_RESOURCE_INTERVAL_MS: interval });
    assert.equal(result.status, 2, interval);
  }
});

test('forwards cancellation to the command process group', { timeout: 15_000 }, async () => {
  const child = spawn(process.execPath, [runner, process.execPath, '-e', 'console.log("child-ready"); setInterval(() => {}, 1000)']);
  let output = '';
  let error = '';
  let cancelled = false;
  child.stderr.on('data', chunk => { error += chunk; });
  child.stdout.on('data', chunk => {
    output += chunk;
    if (!cancelled && output.includes('child-ready')) {
      cancelled = true;
      child.kill('SIGTERM');
    }
  });
  const code = await new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('close', resolve);
  });
  assert.equal(code, 143, error);
  assert.match(output, /received SIGTERM/);
  assert.match(output, /signal=SIGTERM/);
});

test('retains the newest kernel warning when diagnostic output exceeds its buffer', () => {
  const source = `
    import childProcess from 'node:child_process';
    import { syncBuiltinESMExports } from 'node:module';
    const spawn = childProcess.spawn;
    childProcess.spawn = (command, args, options) => command === 'sudo'
      ? spawn(process.execPath, ['-e', 'console.log("old-warning\\\\n".repeat(20000) + "recent-oom-marker")'], options)
      : spawn(command, args, options);
    syncBuiltinESMExports();
    process.argv = [process.execPath, ${JSON.stringify(runner)}, process.execPath, '-e', 'process.exit(7)'];
    await import(${JSON.stringify(runnerUrl.href)});
  `;
  const result = spawnSync(process.execPath, ['--input-type=module', '-e', source], { encoding: 'utf8', timeout: 15_000 });
  assert.equal(result.status, 7, result.stderr);
  assert.match(result.stdout, /recent-oom-marker/);
  assert.ok(result.stdout.length < 32 * 1024, 'diagnostic output should stay bounded');
});

test('cancellation stays responsive while diagnostics are slow', { timeout: 15_000 }, async t => {
  const source = `
    import childProcess from 'node:child_process';
    import { syncBuiltinESMExports } from 'node:module';
    const spawn = childProcess.spawn;
    let calls = 0;
    childProcess.spawn = (command, args, options) => {
      if (command === 'df' && ++calls > 2) {
        console.log('slow-probe-started');
        return spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], options);
      }
      return spawn(command, args, options);
    };
    syncBuiltinESMExports();
    process.argv = [process.execPath, ${JSON.stringify(runner)}, process.execPath, '-e', 'setInterval(() => {}, 1000)'];
    await import(${JSON.stringify(runnerUrl.href)});
  `;
  const child = spawn(process.execPath, ['--input-type=module', '-e', source], {
    env: { ...process.env, CI_RESOURCE_INTERVAL_MS: '50' },
  });
  let output = '';
  let error = '';
  let sentAt;
  let forwardedAt;
  t.after(() => {
    const group = output.match(/command started: pid=(\d+)/)?.[1];
    if (group) { try { process.kill(-Number(group), 'SIGKILL'); } catch {} }
    if (child.exitCode === null) child.kill('SIGKILL');
  });
  child.stderr.on('data', chunk => { error += chunk; });
  child.stdout.on('data', chunk => {
    output += chunk;
    if (!sentAt && output.includes('slow-probe-started')) {
      sentAt = Date.now();
      child.kill('SIGTERM');
    }
    if (!forwardedAt && output.includes('received SIGTERM')) forwardedAt = Date.now();
  });
  const code = await new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('close', resolve);
  });
  assert.equal(code, 143, error);
  assert.ok(forwardedAt >= sentAt && forwardedAt - sentAt < 1500, 'signal forwarding must not wait for resource probes');
});

test('escalates cancellation after the direct child exits but a grandchild ignores SIGTERM', {
  timeout: 15_000, skip: process.platform !== 'linux',
}, async t => {
  const grandchildSource = 'process.on("SIGTERM", () => console.log("grandchild-ignored")); console.log("grandchild-ready:" + process.pid); setInterval(() => {}, 1000)';
  const source = `
    const { spawn } = require('node:child_process');
    process.on('SIGTERM', () => process.exit(0));
    spawn(process.execPath, ['-e', ${JSON.stringify(grandchildSource)}], { stdio: 'inherit' });
  `;
  const child = spawn(process.execPath, [runner, process.execPath, '-e', source]);
  let output = '';
  let error = '';
  let cancelled = false;
  t.after(() => {
    const group = output.match(/command started: pid=(\d+)/)?.[1];
    if (group) { try { process.kill(-Number(group), 'SIGKILL'); } catch {} }
    if (child.exitCode === null) child.kill('SIGKILL');
  });
  child.stderr.on('data', chunk => { error += chunk; });
  child.stdout.on('data', chunk => {
    output += chunk;
    if (!cancelled && output.includes('grandchild-ready:')) {
      cancelled = true;
      child.kill('SIGTERM');
    }
  });
  const code = await new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('close', resolve);
  });
  assert.equal(code, 143, error);
  assert.match(output, /grandchild-ignored/);
  assert.match(output, /received SIGKILL/);
  const pid = output.match(/grandchild-ready:(\d+)/)?.[1];
  assert.ok(pid);
  try {
    // Some container init processes leave a reparented zombie briefly; it is not running.
    const stat = readFileSync(`/proc/${pid}/stat`, 'utf8');
    assert.equal(stat.slice(stat.lastIndexOf(')') + 2).split(' ')[0], 'Z', stat);
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
  }
});
