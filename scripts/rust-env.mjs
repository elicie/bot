// Build environment for the Rust workspace: a verified protoc and ripgrep paths for upstream's bundle.
import { spawn, execFileSync } from 'node:child_process';
import { access, chmod, mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const output = path.join(root, '.engine');
const targets = {
  'linux-x64': ['linux-x86_64', '3e866620c5be27664f3d2fa2d656b5f3e09b5152b42f1bedbf427b333e90021a'],
};

export const run = (command, args, env = process.env) => new Promise((resolve, reject) => {
  const child = spawn(command, args, { cwd: root, env, stdio: 'inherit' });
  child.once('error', reject);
  child.once('exit', (code, signal) => code === 0 ? resolve() : reject(new Error(`${command} failed (${signal ?? code}).`)));
});

async function stagedProtoc() {
  const platform = targets[`${process.platform}-${process.arch}`];
  if (!platform) throw new Error('Set PROTOC to a compatible Protocol Buffers compiler for this build host.');
  const [target, checksum] = platform;
  const tools = path.join(output, 'tools', 'protoc-29.3');
  const protoc = path.join(tools, 'bin', 'protoc');
  try { await access(protoc); return protoc; } catch {}
  const scratch = await mkdtemp(path.join(tmpdir(), 'grok-bot-protoc-'));
  try {
    const response = await fetch(`https://github.com/protocolbuffers/protobuf/releases/download/v29.3/protoc-29.3-${target}.zip`, { signal: AbortSignal.timeout(120_000) });
    if (!response.ok) throw new Error(`Protocol Buffers compiler download failed (${response.status}).`);
    const bytes = Buffer.from(await response.arrayBuffer());
    if (createHash('sha256').update(bytes).digest('hex') !== checksum) throw new Error('Protocol Buffers compiler checksum mismatch.');
    const archive = path.join(scratch, 'protoc.zip');
    await writeFile(archive, bytes);
    await mkdir(tools, { recursive: true });
    await run('unzip', ['-oq', archive, '-d', tools]);
    await chmod(protoc, 0o755);
    return protoc;
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
}

export async function rustEnv() {
  const env = { ...process.env };
  env.PROTOC ??= await stagedProtoc();
  execFileSync(env.PROTOC, ['--version'], { stdio: 'ignore' });
  if (!env.GROK_SHELL_BUNDLE_RG_PATH) {
    try { env.GROK_SHELL_BUNDLE_RG_PATH = execFileSync('which', ['rg'], { encoding: 'utf8' }).trim(); }
    catch { throw new Error('Install ripgrep for the engine source build, or set GROK_SHELL_BUNDLE_RG_PATH.'); }
  }
  env.GROK_TOOLS_BUNDLE_RG_PATH ??= env.GROK_SHELL_BUNDLE_RG_PATH;
  return env;
}
