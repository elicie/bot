import { execFile } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import type { RuntimeInfo } from '../shared/types.js';

export const ENGINE_COMMAND = fileURLToPath(new URL('../.engine/grok-bot-engine', import.meta.url));
export const ENGINE_SOURCE_REVISION = '2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8';
const ENGINE_VERSION = '0.1.0';
const buildAdvice = 'Run npm run build:engine in the project directory, then restart the app.';

export function probeEngine(command: string, home: string, env: NodeJS.ProcessEnv): Promise<RuntimeInfo> {
  return new Promise(resolve => {
    execFile(command, ['--version'], { env, cwd: home, timeout: 4000, maxBuffer: 4096 }, (error, stdout) => {
      const identity = stdout.trim().match(/^grok-bot-engine (\d+\.\d+\.\d+) \(([a-f\d]{40})\)$/);
      if (error || !identity) {
        resolve({ available: false, error: `The application engine is unavailable. ${buildAdvice}` });
        return;
      }
      const [, version, revision] = identity;
      const info = { version: `grok-bot-engine ${version} (${revision.slice(0, 12)})` };
      resolve(version === ENGINE_VERSION && revision === ENGINE_SOURCE_REVISION
        ? { ...info, available: true }
        : { ...info, available: false, error: `The application engine does not match this app's pinned source. ${buildAdvice}` });
    });
  });
}
