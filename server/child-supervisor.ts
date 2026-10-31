import { spawn } from 'node:child_process';

const { command, args, cancelDeadlineMs } = JSON.parse(process.argv[2]) as {
  command: string;
  args: string[];
  cancelDeadlineMs: number;
};
const child = spawn(command, args, { stdio: 'pipe', detached: true });
let stopping = false;
let timer: NodeJS.Timeout | undefined;

function signalGroup(signal: NodeJS.Signals): void {
  if (!child.pid) return;
  try { process.kill(-child.pid, signal); }
  catch (error) { if ((error as NodeJS.ErrnoException).code !== 'ESRCH') throw error; }
}

function stop(): void {
  if (stopping) return;
  stopping = true;
  signalGroup('SIGTERM');
  timer = setTimeout(() => signalGroup('SIGKILL'), cancelDeadlineMs);
}

process.stdin.pipe(child.stdin);
child.stdout.pipe(process.stdout, { end: false });
child.stderr.pipe(process.stderr, { end: false });
child.stdin.on('error', () => {});
process.stdout.on('error', stop);
process.stderr.on('error', stop);
child.once('error', () => process.exit(1));
child.once('exit', () => {
  clearTimeout(timer);
  signalGroup('SIGKILL');
});
child.once('close', code => {
  const finish = () => process.exit(code ?? 1);
  if (process.stdout.destroyed || process.stdout.writableFinished) finish();
  process.stdout.end(finish);
  setTimeout(finish, cancelDeadlineMs).unref();
});
process.on('message', (message: { type?: string; signal?: string }) => {
  if (message.type !== 'signal') return;
  if (message.signal === 'SIGKILL') signalGroup('SIGKILL');
  else if (message.signal === 'SIGTERM') stop();
});
process.on('disconnect', stop);
process.on('SIGTERM', stop);
process.on('SIGINT', stop);
if (!process.connected) stop();
