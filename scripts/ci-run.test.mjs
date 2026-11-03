import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';
import { setTimeout as delay } from 'node:timers/promises';

const runnerUrl = new URL('./ci-run.mjs', import.meta.url);
const runner = fileURLToPath(runnerUrl);
const run = (source, env = {}) => spawnSync(process.execPath, [runner, process.execPath, '-e', source], {
  encoding: 'utf8', timeout: 30_000, env: { ...process.env, ...env },
});

function observeRunner(t, child) {
  let output = '';
  let error = '';
  child.stdout.on('data', chunk => { output += chunk; });
  child.stderr.on('data', chunk => { error += chunk; });
  const closed = new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('close', resolve);
  });
  t.after(() => {
    const group = Number(output.match(/command started: pid=(\d+)/)?.[1]);
    if (Number.isSafeInteger(group) && group > 1 && group !== process.pid) {
      try { process.kill(-group, 'SIGKILL'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
    }
    if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
  });
  return { get output() { return output; }, get error() { return error; }, closed };
}

async function assertNotRunning(pid) {
  assert.ok(pid, 'expected a recorded process id');
  const deadline = Date.now() + 5_000;
  while (true) {
    try {
      // A container init may retain a reparented zombie; it is no longer running.
      const stat = readFileSync(`/proc/${pid}/stat`, 'utf8');
      if (['Z', 'X'].includes(stat.slice(stat.lastIndexOf(')') + 2).split(' ')[0])) return;
    } catch (error) {
      if (error.code === 'ENOENT') return;
      throw error;
    }
    assert.ok(Date.now() < deadline, `process ${pid} is still running after wrapper exit`);
    await delay(25);
  }
}

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
  const result = spawnSync(process.execPath, [runner, '/nonexistent-ci-command'], { encoding: 'utf8', timeout: 30_000 });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /command could not start/);
});

test('reports a synchronous command spawn failure without losing diagnostics', () => {
  const source = `
    import childProcess from 'node:child_process';
    import { syncBuiltinESMExports } from 'node:module';
    const spawn = childProcess.spawn;
    childProcess.spawn = (command, args, options) => {
      if (command === 'ci-synchronous-spawn-error') throw new Error('simulated spawn failure');
      return spawn(command, args, options);
    };
    syncBuiltinESMExports();
    process.argv = [process.execPath, ${JSON.stringify(runner)}, 'ci-synchronous-spawn-error'];
    await import(${JSON.stringify(runnerUrl.href)});
  `;
  const result = spawnSync(process.execPath, ['--input-type=module', '-e', source], { encoding: 'utf8', timeout: 30_000 });
  assert.equal(result.status, 1, result.stderr);
  assert.match(result.stderr, /command could not start: simulated spawn failure/);
  assert.match(result.stdout, /after command/);
});

test('honors an external CARGO_TARGET_DIR without logging command arguments or environment', () => {
  const secretArgument = 'ci-argument-secret-marker';
  const secretEnvironment = 'ci-environment-secret-marker';
  const source = `
    import assert from 'node:assert/strict';
    import childProcess from 'node:child_process';
    import { syncBuiltinESMExports } from 'node:module';
    const spawn = childProcess.spawn;
    childProcess.spawn = (command, args, options) => {
      if (command === 'du') {
        assert.deepEqual(args, ['-sh', '--', process.env.CARGO_TARGET_DIR]);
        return spawn(process.execPath, ['-e', 'console.log(' + JSON.stringify('selected-cargo-target:' + process.env.CARGO_TARGET_DIR) + ')'], options);
      }
      return spawn(command, args, options);
    };
    syncBuiltinESMExports();
    process.argv = [process.execPath, ${JSON.stringify(runner)}, process.execPath, '-e', '', '--', ${JSON.stringify(secretArgument)}];
    await import(${JSON.stringify(runnerUrl.href)});
  `;
  const result = spawnSync(process.execPath, ['--input-type=module', '-e', source], {
    encoding: 'utf8', timeout: 30_000,
    env: { ...process.env, CARGO_TARGET_DIR: tmpdir(), CI_TEST_SECRET: secretEnvironment },
  });
  assert.equal(result.status, 0, result.stderr);
  assert.ok(result.stdout.includes(`selected-cargo-target:${tmpdir()}`), result.stdout);
  assert.ok(!(result.stdout + result.stderr).includes(secretArgument));
  assert.ok(!(result.stdout + result.stderr).includes(secretEnvironment));
});

test('rejects invalid and overflowing intervals instead of spinning', () => {
  for (const interval of ['0', '-1', 'NaN', '1.5', '2147483648']) {
    const result = run('process.exit(0)', { CI_RESOURCE_INTERVAL_MS: interval });
    assert.equal(result.status, 2, interval);
  }
});

test('forwards cancellation to the command process group', { timeout: 30_000 }, async () => {
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
  const result = spawnSync(process.execPath, ['--input-type=module', '-e', source], { encoding: 'utf8', timeout: 30_000 });
  assert.equal(result.status, 7, result.stderr);
  assert.match(result.stdout, /recent-oom-marker/);
  assert.ok(result.stdout.length < 32 * 1024, 'diagnostic output should stay bounded');
});

test('cancellation is forwarded before a slow diagnostic probe finishes', { timeout: 30_000 }, async t => {
  const source = `
    import childProcess from 'node:child_process';
    import { syncBuiltinESMExports } from 'node:module';
    const spawn = childProcess.spawn;
    let calls = 0;
    let cancelled = false;
    childProcess.spawn = (command, args, options) => {
      if (command === 'df' && ++calls > 2) {
        console.log('slow-probe-started');
        const probe = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], options);
        probe.once('close', () => console.log('slow-probe-finished'));
        // Request cancellation in the wrapper's next microtask, not after a
        // timing-sensitive round trip through the test runner's output pipe.
        if (!cancelled) {
          cancelled = true;
          queueMicrotask(() => process.emit('SIGTERM'));
        }
        return probe;
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
  t.after(() => {
    const group = output.match(/command started: pid=(\d+)/)?.[1];
    if (group) { try { process.kill(-Number(group), 'SIGKILL'); } catch {} }
    if (child.exitCode === null) child.kill('SIGKILL');
  });
  child.stderr.on('data', chunk => { error += chunk; });
  child.stdout.on('data', chunk => { output += chunk; });
  const code = await new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('close', resolve);
  });
  assert.equal(code, 143, error);
  const started = output.indexOf('slow-probe-started');
  const forwarded = output.indexOf('received SIGTERM');
  const completed = output.indexOf('slow-probe-finished');
  assert.ok(started >= 0 && forwarded > started && completed > forwarded, output);
});

test('escalates cancellation after the direct child exits but a grandchild ignores SIGTERM', {
  timeout: 30_000, skip: process.platform !== 'linux',
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
  assert.match(output, /escalating command cleanup to SIGKILL/);
  await assertNotRunning(output.match(/grandchild-ready:(\d+)/)?.[1]);
});

test('cleans up a SIGTERM-resistant grandchild after the direct child exits successfully', {
  timeout: 30_000, skip: process.platform !== 'linux',
}, async t => {
  const grandchildSource = 'process.on("SIGTERM", () => console.log("grandchild-ignored")); console.log("grandchild-ready:" + process.pid); process.send("ready"); setInterval(() => {}, 1000)';
  const source = `
    const { spawn } = require('node:child_process');
    const grandchild = spawn(process.execPath, ['-e', ${JSON.stringify(grandchildSource)}], { stdio: ['ignore', 'inherit', 'inherit', 'ipc'] });
    grandchild.once('message', () => process.exit(0));
  `;
  const observed = observeRunner(t, spawn(process.execPath, [runner, process.execPath, '-e', source]));
  const code = await observed.closed;
  assert.equal(code, 0, observed.error);
  assert.match(observed.output, /command finished: exit=0 signal=none/);
  assert.match(observed.output, /cleaning up command process group with SIGTERM/);
  assert.match(observed.output, /grandchild-ignored/);
  assert.match(observed.output, /escalating command cleanup to SIGKILL/);
  await assertNotRunning(observed.output.match(/grandchild-ready:(\d+)/)?.[1]);
});

for (const [description, action, status] of [
  ['explicit process.exit', 'process.exit(23)', 23],
  ['uncaught wrapper failure', 'throw new Error("forced-wrapper-fault")', 1],
]) {
  test(`synchronous exit guard cleans up the owned group on ${description}`, {
    timeout: 30_000, skip: process.platform !== 'linux',
  }, async t => {
    const grandchildSource = 'process.on("SIGTERM", () => {}); console.log("grandchild-ready:" + process.pid); setInterval(() => {}, 1000)';
    const commandSource = `
      const { spawn } = require('node:child_process');
      process.on('SIGTERM', () => {});
      spawn(process.execPath, ['-e', ${JSON.stringify(grandchildSource)}], { stdio: 'inherit' });
    `;
    const wrapperSource = `
      process.on('SIGUSR2', () => { ${action}; });
      process.argv = [process.execPath, ${JSON.stringify(runner)}, process.execPath, '-e', ${JSON.stringify(commandSource)}];
      await import(${JSON.stringify(runnerUrl.href)});
    `;
    const child = spawn(process.execPath, ['--input-type=module', '-e', wrapperSource]);
    const observed = observeRunner(t, child);
    let triggered = false;
    child.stdout.on('data', () => {
      if (!triggered && observed.output.includes('grandchild-ready:')) {
        triggered = true;
        child.kill('SIGUSR2');
      }
    });
    const code = await observed.closed;
    assert.equal(code, status, observed.error);
    assert.ok(triggered, observed.output);
    assert.doesNotMatch(observed.output, /command finished:/, 'must exercise the exit guard, not normal finish');
    if (status === 1) assert.match(observed.error, /forced-wrapper-fault/);
    await assertNotRunning(observed.output.match(/command started: pid=(\d+)/)?.[1]);
    await assertNotRunning(observed.output.match(/grandchild-ready:(\d+)/)?.[1]);
  });
}
