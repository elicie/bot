import { homedir, hostname, networkInterfaces } from 'node:os';
import path from 'node:path';
import { createBackend } from './app.js';

const port = Number(process.env.PORT ?? 3000);
const host = process.env.HOST ?? '127.0.0.1';
if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('PORT must be an integer between 1 and 65535.');
if (!['127.0.0.1', '0.0.0.0'].includes(host)) throw new Error('HOST must be 127.0.0.1 or 0.0.0.0.');
const addresses = Object.values(networkInterfaces()).flatMap(entries => entries ?? [])
  .filter(entry => entry.family === 'IPv4' && !entry.internal).map(entry => entry.address);
const machineName = hostname();
const configuredHosts = (process.env.GROK_BOT_ALLOWED_HOSTS ?? '').split(',').map(value => value.trim()).filter(Boolean);
for (const value of configuredHosts) {
  let url: URL;
  try { url = new URL(`http://${value}`); } catch { throw new Error('GROK_BOT_ALLOWED_HOSTS must contain comma-separated hostnames without ports or URLs.'); }
  if (/[\s/@?#\\:*]/.test(value) || url.port || url.pathname !== '/') throw new Error('GROK_BOT_ALLOWED_HOSTS must contain comma-separated hostnames without ports or URLs.');
}
const backend = await createBackend({
  dataDir: process.env.GROK_BOT_DATA_DIR ?? path.join(homedir(), '.local/share/grok-bot-q'),
  dev: process.argv.includes('--dev'),
  allowedHosts: host === '0.0.0.0' ? ['0.0.0.0', machineName, `${machineName}.local`, ...addresses, ...configuredHosts] : [],
});
let stopping = false;
const stop = async () => {
  if (stopping) return;
  stopping = true;
  try { await backend.close(); }
  catch { process.exitCode = 1; }
};
process.once('SIGINT', () => { void stop(); });
process.once('SIGTERM', () => { void stop(); });
backend.server.once('error', error => {
  console.error(`Local server could not start. ${error.message}`);
  void stop();
  process.exitCode = 1;
});
backend.server.listen(port, host, () => {
  console.log(`Grok Bot is listening at http://${host}:${port}`);
  if (host === '0.0.0.0') {
    console.warn('Network access is enabled without login. Use a trusted network.');
    for (const address of addresses) console.log(`Network URL: http://${address}:${port}`);
  }
});
