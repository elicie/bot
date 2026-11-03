import { spawn, type ChildProcess } from 'node:child_process';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createServer } from 'node:net';

export async function startTestApp(options: { network?: boolean; hostname?: string; env?: Record<string, string> } = {}) {
  const root = await mkdtemp(join(tmpdir(), 'grok-bot-e2e-'));
  const workspace = join(root, 'workspace');
  const dataDir = join(root, 'private-data');
  await mkdir(workspace);
  await writeFile(join(workspace, 'notes.txt'), 'original local content\n');
  await mkdir(join(workspace, 'nested'));
  await mkdir(join(workspace, 'empty'));
  await writeFile(join(workspace, 'nested', 'readme.md'), '# Local workspace\n');
  await writeFile(join(workspace, 'binary.bin'), Buffer.from([0, 1, 2]));
  const listener = createServer();
  await new Promise<void>(resolve => listener.listen(0, '127.0.0.1', resolve));
  const address = listener.address();
  if (!address || typeof address === 'string') throw new Error('No available loopback port');
  const port = address.port;
  await new Promise<void>(resolve => listener.close(() => resolve()));
  const url = `http://${options.network ? options.hostname ?? 'grok-bot.test' : '127.0.0.1'}:${port}`;
  const probeUrl = `http://127.0.0.1:${port}`;
  // Build the frontend and Rust debug server before running e2e (`npm run build && npm run build:server`).
  const rustBinary = process.env.GROK_BOT_SERVER_BIN ?? join(process.cwd(), 'target', 'debug', 'grok-bot');
  const child: ChildProcess = spawn(rustBinary, [], {
    cwd: process.cwd(),
    env: { ...process.env, HOST: options.network ? '0.0.0.0' : '127.0.0.1', PORT: String(port), GROK_BOT_ALLOWED_HOSTS: options.network ? options.hostname ?? 'grok-bot.test' : '', GROK_BOT_DATA_DIR: dataDir, GROK_BOT_WEB_ROOT: join(process.cwd(), 'dist'), NODE_ENV: 'test', ...options.env },
    stdio: ['ignore', 'pipe', 'pipe'],
    detached: process.platform !== 'win32',
  });
  let logs = '';
  let spawnError: Error | undefined;
  let closed = false;
  child.once('error', error => { spawnError = error; });
  child.once('close', () => { closed = true; });
  child.stdout?.on('data', chunk => { logs += chunk.toString(); });
  child.stderr?.on('data', chunk => { logs += chunk.toString(); });
  const stop = async () => {
    if (!closed && child.exitCode === null && child.signalCode === null && child.pid) {
      const exited = new Promise<void>(resolve => child.once('close', () => resolve()));
      try { process.kill(process.platform === 'win32' ? child.pid : -child.pid, 'SIGTERM'); } catch {}
      await Promise.race([exited, new Promise(resolve => setTimeout(resolve, 4000))]);
      if (!closed && child.exitCode === null && child.signalCode === null) {
        try { process.kill(process.platform === 'win32' ? child.pid : -child.pid, 'SIGKILL'); } catch {}
        await exited;
      }
    }
    await rm(root, { recursive: true, force: true });
  };
  try {
    const deadline = Date.now() + 30_000;
    while (Date.now() < deadline) {
      if (spawnError) throw new Error(`Could not start Rust server at ${rustBinary}: ${spawnError.message}. Run npm run build:server first.`, { cause: spawnError });
      if (closed || child.exitCode !== null || child.signalCode !== null) throw new Error(`Application exited (${child.signalCode ?? child.exitCode}): ${logs}`);
      try {
        const response = await fetch(`${probeUrl}/api/state`);
        if (response.ok) return { url, workspace, dataDir, root, stop, logs: () => logs };
      } catch {}
      await new Promise(resolve => setTimeout(resolve, 200));
    }
    throw new Error(`Application startup timed out: ${logs}`);
  } catch (error) {
    await stop();
    throw error;
  }
}
