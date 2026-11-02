import { chmod, copyFile, cp, mkdir, rename } from 'node:fs/promises';
import path from 'node:path';
import { root, run, rustEnv } from './rust-env.mjs';

const output = path.join(root, '.engine');
await mkdir(output, { recursive: true });
const env = await rustEnv();
const targetDir = path.resolve(env.CARGO_TARGET_DIR ?? path.join(root, 'engine', 'target'));
env.CARGO_TARGET_DIR = targetDir;
await run('cargo', [`+${process.env.RUSTUP_TOOLCHAIN || 'stable'}`, 'build', '--locked', '--manifest-path', 'engine/Cargo.toml', '--bin', 'grok-bot-engine', '--jobs', process.env.CARGO_BUILD_JOBS ?? '4'], env);
const artifact = path.join(output, 'grok-bot-engine');
const temporary = path.join(output, `grok-bot-engine-${process.pid}.tmp`);
await copyFile(path.join(targetDir, 'debug', 'grok-bot-engine'), temporary);
await chmod(temporary, 0o755);
await rename(temporary, artifact);
await copyFile(path.join(root, 'engine', 'NOTICE'), path.join(output, 'NOTICE'));
await cp(path.join(root, 'engine', 'licenses'), path.join(output, 'licenses'), { recursive: true });
await run(artifact, ['--version']);
console.log('Application engine built from the pinned Grok Build runtime source.');
