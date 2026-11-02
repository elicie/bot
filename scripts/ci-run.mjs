// Run a CI command with resource snapshots, including while the command is silent.
// Usage: node scripts/ci-run.mjs <command> [args...]
import { spawn } from 'node:child_process';
import { access, readFile } from 'node:fs/promises';
import { constants } from 'node:os';

const [command, ...args] = process.argv.slice(2);
const intervalMs = Number(process.env.CI_RESOURCE_INTERVAL_MS ?? 60_000);
const targetDirectory = process.env.CARGO_TARGET_DIR || 'target';
if (!command || !Number.isSafeInteger(intervalMs) || intervalMs <= 0 || intervalMs > 2_147_483_647) {
  console.error('Usage: node scripts/ci-run.mjs <command> [args...] (CI_RESOURCE_INTERVAL_MS must be 1..2147483647)');
  process.exit(2);
}

function inspect(command, args, maxLines) {
  // Async, parallel probes keep cancellation responsive even when a filesystem is slow.
  return new Promise(resolve => {
    const probe = spawn(command, args, { stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '';
    let stderr = '';
    let error;
    // Keep a bounded tail for kernel warnings, rather than losing the newest OOM evidence.
    probe.stdout.on('data', chunk => {
      stdout = maxLines < 0 ? (stdout + chunk).slice(-128 * 1024) : (stdout + chunk).slice(0, 128 * 1024);
    });
    probe.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-8192); });
    const timeout = setTimeout(() => probe.kill('SIGKILL'), 2_000);
    probe.once('error', value => { error = value; });
    probe.once('close', (code, signal) => {
      clearTimeout(timeout);
      if (stdout) {
        const lines = stdout.trimEnd().split('\n');
        console.log(maxLines ? (maxLines > 0 ? lines.slice(0, maxLines) : lines.slice(maxLines)).join('\n') : stdout.trimEnd());
      }
      if (stderr) console.error(stderr.trimEnd());
      if (error || code !== 0) console.log(`[ci-resources] ${command} unavailable: ${error?.message ?? signal ?? code}`);
      resolve();
    });
  });
}

async function snapshot(phase) {
  console.log(`\n[ci-resources] ${new Date().toISOString()} ${phase}`);
  await Promise.all([
    inspect('df', ['-h', '/', '.', '/tmp']),
    inspect('df', ['-i', '/', '.', '/tmp']),
    inspect('free', ['-m']),
    // Command names only: process arguments and environment can contain secrets.
    inspect('ps', ['-eo', 'pid,ppid,stat,etime,%cpu,%mem,rss,comm', '--sort=-rss'], 16),
    access(targetDirectory).then(() => inspect('du', ['-sh', '--', targetDirectory]), () => {}),
    ...['/sys/fs/cgroup/memory.current', '/sys/fs/cgroup/memory.max', '/sys/fs/cgroup/memory.events', '/proc/pressure/memory', '/proc/pressure/io'].map(async file => {
      try { console.log(`${file}:\n${(await readFile(file, 'utf8')).trimEnd()}`); } catch { /* Not available on every runner. */ }
    }),
  ]);
}

await snapshot('before command');
// POSIX detachment is only for an OWNED process group, never daemonization: keep
// the child referenced and explicitly clean up the group on every completion.
// Without this isolation, a negative-pid signal could reach the runner's group.
const grouped = process.platform !== 'win32';
let child;
let ownedGroup;
let probing;
let timer;
let finished = false;
let killTimer;
let cleanup;
let resolveCleanup;
let cancelledBy;

function commandAlive() {
  if (!grouped) return Boolean(child?.pid && child.exitCode === null && child.signalCode === null);
  if (!ownedGroup) return false;
  try { process.kill(-ownedGroup, 0); return true; }
  catch (error) {
    // Retire ownership once the group disappears; never signal a later reused id.
    if (error.code === 'ESRCH') ownedGroup = undefined;
    return error.code !== 'ESRCH';
  }
}

function signalCommand(signal, reportErrors = true) {
  try {
    if (grouped) {
      if (ownedGroup) process.kill(-ownedGroup, signal);
    } else child?.kill(signal);
  } catch (error) {
    if (error.code === 'ESRCH') ownedGroup = undefined;
    else if (reportErrors) console.error(`[ci-resources] command signal failed: ${error.code ?? 'unknown'}`);
  }
}

function completeCleanup() {
  clearTimeout(killTimer);
  killTimer = undefined;
  resolveCleanup?.();
  resolveCleanup = undefined;
}

function cleanupCommand(signal = 'SIGTERM') {
  if (!commandAlive()) {
    completeCleanup();
    return cleanup ?? Promise.resolve();
  }
  if (!cleanup) {
    cleanup = new Promise(resolve => { resolveCleanup = resolve; });
    console.log(`[ci-resources] cleaning up command ${grouped ? 'process group' : 'process'} with ${signal}`);
    signalCommand(signal);
    // This stays referenced even if the direct child exits before its descendants.
    killTimer = setTimeout(() => {
      if (commandAlive()) {
        console.log('[ci-resources] escalating command cleanup to SIGKILL');
        signalCommand('SIGKILL');
      }
      completeCleanup();
    }, 5_000);
  }
  return cleanup;
}

process.once('exit', () => {
  if (!commandAlive()) return;
  // process.exit()/uncaught failures cannot await the normal five-second grace
  // period. This synchronous last chance must not intentionally leave daemons.
  // SIGKILL of this wrapper or host loss cannot execute any exit handler.
  signalCommand('SIGTERM', false);
  signalCommand('SIGKILL', false);
});

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => {
    cancelledBy ??= signal;
    process.exitCode = 128 + constants.signals[cancelledBy];
    if (!finished || commandAlive()) {
      console.log(`[ci-resources] received ${signal}; forwarding to command`);
      if (cleanup) signalCommand(signal);
      void cleanupCommand(signal);
    }
  });
}

async function finish(code, signal, error) {
  if (finished) return;
  finished = true;
  clearInterval(timer);
  if (error) console.error(`[ci-resources] command could not start: ${error.message}`);
  console.log(`[ci-resources] command finished: exit=${code ?? 'none'} signal=${signal ?? 'none'}`);
  const status = () => (cancelledBy ?? signal) ? 128 + (constants.signals[cancelledBy ?? signal] ?? 1) : error ? 1 : code ?? 1;
  process.exitCode = status();
  // Start teardown immediately on exit/error, including a successful command
  // which left descendants behind. Diagnostics run within the grace period.
  const cleaning = cleanupCommand(cancelledBy ?? 'SIGTERM');
  try {
    await probing;
    await snapshot('after command');
    if (status() !== 0 && !cancelledBy) {
      // Diagnostics must not replace the command's exit status.
      await inspect('sudo', ['-n', 'dmesg', '--ctime', '--level=err,warn'], -80);
    }
  } catch {
    console.error('[ci-resources] resource diagnostics unavailable');
  } finally {
    if (!commandAlive()) completeCleanup();
    await cleaning;
    process.exitCode = status();
  }
}

try {
  child = spawn(command, args, { stdio: 'inherit', detached: grouped });
  if (grouped && Number.isSafeInteger(child.pid) && child.pid > 1 && child.pid !== process.pid) ownedGroup = child.pid;
  console.log(`[ci-resources] command started: pid=${child.pid ?? 'unavailable'}`);
  timer = setInterval(() => {
    if (!probing) probing = snapshot('heartbeat').finally(() => { probing = undefined; });
  }, intervalMs);
  child.once('error', error => finish(1, undefined, error));
  child.once('exit', (code, signal) => finish(code, signal));
} catch (error) {
  await finish(1, undefined, error);
}
