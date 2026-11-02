// Copy stdin to stdout, clipping each line to MAX_LINE characters. CI pipes test output through this
// because the GitHub Actions runner stalls (ignoring step timeouts) on multi-megabyte log lines (#13).
import { createInterface } from 'node:readline';

const MAX_LINE = Number(process.env.CLIP_LINE_CHARS ?? 20000);
const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
for await (const line of lines) {
  const text = line.length > MAX_LINE ? `${line.slice(0, MAX_LINE)} …[${line.length - MAX_LINE} more chars clipped]` : line;
  if (!process.stdout.write(text + '\n')) await new Promise(resolve => process.stdout.once('drain', resolve));
}
