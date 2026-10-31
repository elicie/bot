import { constants } from 'node:fs';
import { open, mkdir, lstat, readdir, rename, unlink, realpath } from 'node:fs/promises';
import type { FileHandle } from 'node:fs/promises';
import { createHash, randomUUID } from 'node:crypto';
import path from 'node:path';
import type { FileDocument, FileListing } from '../shared/types.js';

export class HttpError extends Error {
  constructor(public readonly status: number, message: string) { super(message); }
}

export const MAX_FILE_BYTES = 1024 * 1024;
const directoryFlags = constants.O_RDONLY | constants.O_DIRECTORY | constants.O_NOFOLLOW;
const hash = (bytes: Uint8Array) => createHash('sha256').update(bytes).digest('hex');
export const isMissing = (error: unknown) => (error as NodeJS.ErrnoException).code === 'ENOENT';

async function directory(absolute: string, create = false): Promise<FileHandle> {
  let walked = path.parse(absolute).root;
  let current = await open(walked, directoryFlags);
  try {
    for (const segment of absolute.split(path.sep).filter(Boolean)) {
      walked = path.join(walked, segment);
      const next = process.platform === 'linux' ? `/proc/self/fd/${current.fd}/${segment}` : walked;
      let child: FileHandle;
      try { child = await open(next, directoryFlags); }
      catch (error) {
        if (!create || !isMissing(error)) throw error;
        await mkdir(next, { mode: 0o700 }).catch(error => { if ((error as NodeJS.ErrnoException).code !== 'EEXIST') throw error; });
        child = await open(next, directoryFlags);
      }
      await current.close();
      current = child;
    }
    return current;
  } catch (error) {
    await current.close();
    const code = (error as NodeJS.ErrnoException).code;
    if (code === 'ELOOP' || code === 'ENOTDIR') throw new HttpError(403, 'Symbolic links and non-directory parents are not allowed.');
    throw error;
  }
}

function currentPath(handle: FileHandle, fallback: string) {
  return process.platform === 'linux' ? `/proc/self/fd/${handle.fd}` : fallback;
}

export async function canonicalDirectory(input: string): Promise<string> {
  if (!path.isAbsolute(input) || input.includes('\0') || input.split(path.sep).includes('..')) throw new HttpError(400, 'Workspace must be an absolute directory path.');
  const resolved = path.resolve(input);
  const handle = await directory(resolved);
  await handle.close();
  return realpath(resolved);
}

export async function privateDirectory(input: string): Promise<string> {
  const resolved = path.resolve(input);
  if (resolved === path.parse(resolved).root) throw new HttpError(400, 'Application data must use its own private directory.');
  const handle = await directory(resolved, true);
  try { await handle.chmod(0o700); } finally { await handle.close(); }
  return resolved;
}

export async function atomicPrivateFile(filename: string, content: string): Promise<void> {
  const temp = `${filename}.${randomUUID()}.tmp`;
  const parent = await directory(path.dirname(filename));
  const anchor = currentPath(parent, path.dirname(filename));
  const temporaryPath = path.join(anchor, path.basename(temp));
  try {
    const file = await open(temporaryPath, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, 0o600);
    try { await file.writeFile(content); await file.sync(); } finally { await file.close(); }
    await rename(temporaryPath, path.join(anchor, path.basename(filename)));
    await parent.sync();
  } finally {
    await unlink(temporaryPath).catch(error => { if (!isMissing(error)) throw error; });
    await parent.close();
  }
}

function text(bytes: Uint8Array): string {
  if (bytes.byteLength > MAX_FILE_BYTES) throw new HttpError(413, 'Text files are limited to 1 MiB.');
  if (bytes.some(byte => byte === 0 || byte < 9 || (byte > 13 && byte < 32))) throw new HttpError(415, 'Binary files cannot be edited.');
  try { return new TextDecoder('utf-8', { fatal: true }).decode(bytes); }
  catch { throw new HttpError(415, 'Only UTF-8 text files can be edited.'); }
}

export class WorkspaceFiles {
  constructor(public readonly root: string) {}

  relative(input: string, allowRoot = false): string {
    if (typeof input !== 'string' || input.includes('\0') || input.includes('\\') || path.isAbsolute(input)) throw new HttpError(403, 'Use a workspace-relative path.');
    if ((input === '' || input === '.') && allowRoot) return '';
    if (!input || input.split('/').some(part => !part || part === '.' || part === '..')) throw new HttpError(403, 'Path traversal is not allowed.');
    return input;
  }

  private async parent<T>(input: string, work: (filename: string, parent: FileHandle) => Promise<T>): Promise<T> {
    const relative = this.relative(input);
    const parentPath = path.join(this.root, path.dirname(relative));
    const handle = await directory(parentPath);
    try { return await work(path.join(currentPath(handle, parentPath), path.basename(relative)), handle); }
    catch (error) {
      const code = (error as NodeJS.ErrnoException).code;
      if (code === 'ELOOP') throw new HttpError(403, 'Symbolic links are not allowed.');
      if (code === 'ENOENT') throw new HttpError(404, 'File or directory does not exist.');
      if (code === 'EACCES' || code === 'EPERM') throw new HttpError(403, 'File access was denied.');
      throw error;
    } finally { await handle.close(); }
  }

  private async document(filename: string, relative: string): Promise<FileDocument> {
    const file = await open(filename, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
    try {
      const stat = await file.stat();
      if (!stat.isFile()) throw new HttpError(415, 'Only regular text files can be edited.');
      if (stat.nlink !== 1) throw new HttpError(403, 'Hard-linked files are not allowed.');
      if (stat.size > MAX_FILE_BYTES) throw new HttpError(413, 'Text files are limited to 1 MiB.');
      const bytes = Buffer.alloc(MAX_FILE_BYTES + 1);
      let length = 0;
      while (length < bytes.length) {
        const result = await file.read(bytes, length, bytes.length - length, null);
        if (!result.bytesRead) break;
        length += result.bytesRead;
      }
      const content = bytes.subarray(0, length);
      return { path: relative, content: text(content), hash: hash(content) };
    } finally { await file.close(); }
  }

  async list(input = ''): Promise<FileListing> {
    const relative = this.relative(input, true);
    const target = path.join(this.root, relative);
    const handle = await directory(target);
    try {
      const entries = await readdir(currentPath(handle, target), { withFileTypes: true });
      if (entries.length > 5000) throw new HttpError(413, 'This directory has more than 5,000 entries.');
      return {
        path: relative,
        entries: entries.filter(entry => entry.isFile() || entry.isDirectory()).map(entry => ({
          name: entry.name, path: [relative, entry.name].filter(Boolean).join('/'),
          type: entry.isDirectory() ? 'directory' as const : 'file' as const,
        })).sort((a, b) => a.type === b.type ? a.name.localeCompare(b.name) : a.type === 'directory' ? -1 : 1),
      };
    } catch (error) {
      if (isMissing(error)) throw new HttpError(404, 'Directory does not exist.');
      throw error;
    } finally { await handle.close(); }
  }

  async read(input: string): Promise<FileDocument> {
    return this.parent(input, filename => this.document(filename, input));
  }

  async expected(input: string): Promise<string> {
    return this.parent(input, async filename => {
      try { return (await this.document(filename, input)).hash; }
      catch (error) { if (isMissing(error)) return ''; throw error; }
    });
  }

  async save(input: string, content: string, expectedHash: string): Promise<FileDocument> {
    const bytes = Buffer.from(content, 'utf8');
    if (bytes.toString('utf8') !== content) throw new HttpError(400, 'Content must contain valid Unicode text.');
    text(bytes);
    return this.parent(input, async (filename, parent) => {
      let mode = 0o600;
      let actualHash = '';
      try {
        actualHash = (await this.document(filename, input)).hash;
        mode = (await lstat(filename)).mode & 0o777;
      } catch (error) { if (!isMissing(error)) throw error; }
      if (actualHash !== expectedHash) throw new HttpError(409, 'File changed on disk. Reload it before saving.');
      const temporaryPath = path.join(path.dirname(filename), `.grok-bot-${randomUUID()}.tmp`);
      try {
        const file = await open(temporaryPath, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, mode);
        try { await file.writeFile(bytes); await file.sync(); } finally { await file.close(); }
        const currentHash = await this.document(filename, input).then(doc => doc.hash).catch(error => { if (isMissing(error)) return ''; throw error; });
        if (currentHash !== expectedHash) throw new HttpError(409, 'File changed on disk. Reload it before saving.');
        await rename(temporaryPath, filename);
        await parent.sync();
        return { path: input, content, hash: hash(bytes) };
      } finally { await unlink(temporaryPath).catch(error => { if (!isMissing(error)) throw error; }); }
    });
  }
}
