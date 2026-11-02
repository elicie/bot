// Run a CI command with resource snapshots, including while the command is silent.
// Usage: node scripts/ci-run.mjs <command> [args...]
import { spawn } from 'node:child_process';
import { access, readFile } from 'node:fs/promises';
import { constants } from 'node:os';

const [command, ...args] = process.argv.slice(2);
const intervalMs = Number(process.env.CI_RESOURCE_INTERVAL_MS ?? 60_000);
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
    access('target').then(() => inspect('du', ['-sh', 'target']), () => {}),
    ...['/sys/fs/cgroup/memory.current', '/sys/fs/cgroup/memory.max', '/sys/fs/cgroup/memory.events', '/proc/pressure/memory', '/proc/pressure/io'].map(async file => {
      try { console.log(`${file}:\n${(await readFile(file, 'utf8')).trimEnd()}`); } catch { /* Not available on every runner. */ }
    }),
  ]);
}

await snapshot('before command');
// A separate process group lets cancellation reach cargo/test subprocesses as well.
const grouped = process.platform !== 'win32';
const child = spawn(command, args, { stdio: 'inherit', detached: grouped });
console.log(`[ci-resources] command started: pid=${child.pid ?? 'unavailable'}`);
let probing;
const timer = setInterval(() => {
  if (!probing) probing = snapshot('heartbeat').finally(() => { probing = undefined; });
}, intervalMs);
let finished = false;
let killTimer;
let cancelledBy;

function groupAlive() {
  if (!grouped || !child.pid) return false;
  try { process.kill(-child.pid, 0); return true; }
  catch (error) { return error.code !== 'ESRCH'; }
}

function forward(signal) {
  console.log(`[ci-resources] received ${signal}; forwarding to command`);
  if (!child.pid) return;
  try {
    if (grouped) process.kill(-child.pid, signal);
    else child.kill(signal);
  } catch (error) {
    if (error.code !== 'ESRCH') console.error(error.message);
  }
}

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => {
    cancelledBy ??= signal;
    process.exitCode = 128 + constants.signals[cancelledBy];
    if (!finished || groupAlive()) {
      forward(signal);
      // Keep this timer referenced if descendants survive the direct child's exit.
      killTimer ??= setTimeout(() => forward('SIGKILL'), 5_000);
    }
  });
}

async function finish(code, signal, error) {
  if (finished) return;
  finished = true;
  clearInterval(timer);
  if (!groupAlive()) clearTimeout(killTimer);
  if (error) console.error(`[ci-resources] command could not start: ${error.message}`);
  console.log(`[ci-resources] command finished: exit=${code ?? 'none'} signal=${signal ?? 'none'}`);
  await probing;
  await snapshot('after command');
  const status = () => (cancelledBy ?? signal) ? 128 + (constants.signals[cancelledBy ?? signal] ?? 1) : error ? 1 : code ?? 1;
  if (status() !== 0 && !cancelledBy) {
    // Hosted Linux usually permits this; diagnostics must not replace the command's exit status.
    await inspect('sudo', ['-n', 'dmesg', '--ctime', '--level=err,warn'], -80);
  }
  process.exitCode = status();
}

child.once('error', error => finish(1, undefined, error));
child.once('close', (code, signal) => finish(code, signal));
