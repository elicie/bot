import { spawn, execFileSync } from 'node:child_process';
import { chmod, copyFile, cp, mkdir, mkdtemp, rename, rm, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const output = path.join(root, '.engine');
const targets = {
  'linux-x64': ['linux-x86_64', '3e866620c5be27664f3d2fa2d656b5f3e09b5152b42f1bedbf427b333e90021a'],
};
const scratch = await mkdtemp(path.join(tmpdir(), 'grok-bot-engine-build-'));
const run = (command, args, env = process.env) => new Promise((resolve, reject) => {
  const child = spawn(command, args, { cwd: root, env, stdio: 'inherit' });
  child.once('error', reject);
  child.once('exit', (code, signal) => code === 0 ? resolve() : reject(new Error(`${command} failed (${signal ?? code}).`)));
});

try {
  await mkdir(output, { recursive: true });
  let protoc = process.env.PROTOC;
  if (!protoc) {
    const platform = targets[`${process.platform}-${process.arch}`];
    if (!platform) throw new Error('Set PROTOC to a compatible Protocol Buffers compiler for this build host.');
    const [target, checksum] = platform;
    const response = await fetch(`https://github.com/protocolbuffers/protobuf/releases/download/v29.3/protoc-29.3-${target}.zip`, { signal: AbortSignal.timeout(120_000) });
    if (!response.ok) throw new Error(`Protocol Buffers compiler download failed (${response.status}).`);
    const bytes = Buffer.from(await response.arrayBuffer());
    if (createHash('sha256').update(bytes).digest('hex') !== checksum) throw new Error('Protocol Buffers compiler checksum mismatch.');
    const archive = path.join(scratch, 'protoc.zip');
    await writeFile(archive, bytes);
    const tools = path.join(output, 'tools', 'protoc-29.3');
    await mkdir(tools, { recursive: true });
    await run('unzip', ['-oq', archive, '-d', tools]);
    protoc = path.join(tools, 'bin', 'protoc');
    await chmod(protoc, 0o755);
  }
  execFileSync(protoc, ['--version'], { stdio: 'inherit' });
  const env = { ...process.env, PROTOC: protoc };
  if (!env.GROK_SHELL_BUNDLE_RG_PATH) {
    try { env.GROK_SHELL_BUNDLE_RG_PATH = execFileSync('which', ['rg'], { encoding: 'utf8' }).trim(); }
    catch { throw new Error('Install ripgrep for the engine source build, or set GROK_SHELL_BUNDLE_RG_PATH.'); }
  }
  env.GROK_TOOLS_BUNDLE_RG_PATH ??= env.GROK_SHELL_BUNDLE_RG_PATH;
  const targetDir = path.resolve(env.CARGO_TARGET_DIR ?? path.join(root, 'engine', 'target'));
  env.CARGO_TARGET_DIR = targetDir;
  await run('cargo', ['+stable', 'build', '--locked', '--manifest-path', 'engine/Cargo.toml', '--jobs', process.env.CARGO_BUILD_JOBS ?? '4'], env);
  const artifact = path.join(output, 'grok-bot-engine');
  const temporary = path.join(output, `grok-bot-engine-${process.pid}.tmp`);
  await copyFile(path.join(targetDir, 'debug', 'grok-bot-engine'), temporary);
  await chmod(temporary, 0o755);
  await rename(temporary, artifact);
  await copyFile(path.join(root, 'engine', 'NOTICE'), path.join(output, 'NOTICE'));
  await cp(path.join(root, 'engine', 'licenses'), path.join(output, 'licenses'), { recursive: true });
  await run(artifact, ['--version']);
  console.log('Application engine built from the pinned Grok Build runtime source.');
} finally {
  await rm(scratch, { recursive: true, force: true });
}
