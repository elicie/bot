// Native Tauri build wrapper; Node is a build tool, never a packaged backend.
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { rustEnv, root, run } from './rust-env.mjs';

const [mode = 'build', ...extra] = process.argv.slice(2);
if (!['build', 'dev'].includes(mode)) throw new Error('Use desktop.mjs build|dev');
if (mode === 'dev' && (process.env.CARGO_BUILD_TARGET || extra.some(arg => arg === '--target' || arg.startsWith('--target=')))) {
  throw new Error('dev:desktop runs the native host only; use build:desktop for explicit targets.');
}
await run('npm', ['run', 'build']);
const env = await rustEnv();
if (env.CARGO_TARGET_DIR) env.CARGO_TARGET_DIR = path.resolve(root, env.CARGO_TARGET_DIR);
const cli = path.join(root, 'node_modules/@tauri-apps/cli/tauri.js');
const args = mode === 'dev' ? ['build', '--debug', '--no-bundle'] : ['build'];
const child = spawn(process.execPath, [cli, ...args, '--features', 'native', ...extra], {
  cwd: fileURLToPath(new URL('../crates/desktop', import.meta.url)), env, stdio: 'inherit',
});
const code = await new Promise((resolve, reject) => { child.once('error', reject); child.once('exit', code => resolve(code ?? 1)); });
if (code !== 0) process.exitCode = code;
else if (mode === 'dev') {
  const binary = path.join(env.CARGO_TARGET_DIR ?? path.join(root, 'target'), 'debug/grok-bot-desktop');
  await run(binary, [] , env);
}
