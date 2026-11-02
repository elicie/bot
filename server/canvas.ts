import { z } from 'zod';
import type { Canvas, CanvasInput, CanvasVersion } from '../shared/types.js';
import { HttpError } from './files.js';

export const MAX_CANVAS_CONTENT_BYTES = 128 * 1024;
export const MAX_CANVAS_HISTORY = 20;
const revisionSchema = z.number().int().min(0).max(Number.MAX_SAFE_INTEGER);
// Unicode-mode matching rejects lone surrogates, not valid astral pairs.
const wellFormed = (value: string) => !/[\uD800-\uDFFF]/u.test(value);
const contentSchema = z.string()
  .refine(wellFormed, 'Canvas text must contain well-formed Unicode.')
  .refine(value => !value.includes('\0'), 'Canvas text cannot contain NUL bytes.')
  .refine(value => Buffer.byteLength(value, 'utf8') <= MAX_CANVAS_CONTENT_BYTES, 'Canvas text must be at most 128 KiB of UTF-8.');
// Zod measures string limits in code points; this contract uses JS UTF-16 units.
const titleSchema = z.string().min(1)
  .refine(wellFormed, 'Canvas titles must contain well-formed Unicode.')
  .refine(value => value.length <= 100, 'Canvas titles must be at most 100 JS units.')
  .refine(value => !value.includes('\0'), 'Canvas titles cannot contain NUL bytes.');
const fieldsSchema = z.object({
  title: z.string().trim().pipe(titleSchema),
  language: z.string().regex(/^[a-z][a-z0-9-]{0,31}$/).refine(value => value.trim() === value, 'Canvas language cannot contain whitespace.'),
  content: contentSchema,
}).strict();
export const canvasInputSchema = fieldsSchema.extend({ revision: revisionSchema }).strict() satisfies z.ZodType<CanvasInput>;
export const canvasRestoreSchema = z.object({ revision: revisionSchema.min(1), targetRevision: revisionSchema.min(1) }).strict();
export const canvasRangeSchema = z.object({
  revision: revisionSchema.min(1),
  oldText: contentSchema.refine(value => value.length > 0, 'Choose a nonempty exact canvas range.'),
  replacement: contentSchema,
}).strict();
export type CanvasRestoreInput = z.infer<typeof canvasRestoreSchema>;
export type CanvasRangeInput = z.infer<typeof canvasRangeSchema>;
const canvasVersionSchema = fieldsSchema.extend({
  title: titleSchema.refine(value => value.trim() === value, 'Persisted canvas titles must already be trimmed.'),
  revision: revisionSchema.min(1), updatedAt: z.iso.datetime(), source: z.enum(['user', 'bot', 'restore']), redacted: z.literal(false).optional(),
}).strict() satisfies z.ZodType<CanvasVersion>;
export const canvasSchema = canvasVersionSchema.extend({
  history: z.array(canvasVersionSchema).max(MAX_CANVAS_HISTORY).default([]), redacted: z.literal(false).optional(),
}).strict().superRefine((canvas, context) => {
  if (canvas.history.length !== Math.min(canvas.revision - 1, MAX_CANVAS_HISTORY)) {
    context.addIssue({ code: 'custom', path: ['history'], message: 'Canvas history must retain the most recent 20 older revisions, or all older revisions when fewer exist.' });
  }
  for (const [index, version] of canvas.history.entries()) {
    if (version.revision !== canvas.revision - canvas.history.length + index) {
      context.addIssue({ code: 'custom', path: ['history', index, 'revision'], message: 'Canvas history revisions must be consecutive and precede the current revision.' });
    }
  }
}) satisfies z.ZodType<Canvas>;

const versionOf = ({ revision, title, language, content, updatedAt, source }: CanvasVersion): CanvasVersion =>
  ({ revision, title, language, content, updatedAt, source });
const sameFields = (left: Pick<CanvasVersion, 'title' | 'language' | 'content'>, right: Pick<CanvasVersion, 'title' | 'language' | 'content'>) =>
  left.title === right.title && left.language === right.language && left.content === right.content;

export function canvasHasSecret(canvas: Pick<CanvasVersion, 'title' | 'language' | 'content'>, secret: string): boolean {
  return Boolean(secret) && [canvas.title, canvas.language, canvas.content].some(value => value.includes(secret));
}

function assertEditable(current: Canvas | undefined, incoming: Pick<CanvasVersion, 'title' | 'language' | 'content'>, secret: string): void {
  if ((current && canvasHasSecret(current, secret)) || canvasHasSecret(incoming, secret)) {
    throw new HttpError(403, 'Canvas is read-only because it contains a configured provider secret. Change that secret in settings before editing or restoring.');
  }
}

function assertRevision(current: Canvas | undefined, expected: number): void {
  if ((current?.revision ?? 0) !== expected) throw new HttpError(409, 'Canvas changed. Read the latest revision before saving.');
}

function nextVersion(current: Canvas | undefined, fields: Pick<CanvasVersion, 'title' | 'language' | 'content'>, source: CanvasVersion['source'], now: string): Canvas {
  if (current?.revision === Number.MAX_SAFE_INTEGER) throw new HttpError(409, 'Canvas revision limit reached. This canvas cannot be changed.');
  return {
    ...fields, revision: (current?.revision ?? 0) + 1, updatedAt: now, source,
    history: current ? [...current.history.map(versionOf), versionOf(current)].slice(-MAX_CANVAS_HISTORY) : [],
  };
}

export function saveCanvas(current: Canvas | undefined, input: CanvasInput, now: string, secret = '', source: 'user' | 'bot' = 'user'): Canvas {
  const { revision, ...fields } = canvasInputSchema.parse(input);
  assertRevision(current, revision);
  assertEditable(current, fields, secret);
  if (current && sameFields(current, fields)) return current;
  return nextVersion(current, fields, source, now);
}

export function restoreCanvas(current: Canvas | undefined, input: CanvasRestoreInput, now: string, secret = ''): Canvas {
  const { revision, targetRevision } = canvasRestoreSchema.parse(input);
  assertRevision(current, revision);
  if (!current) throw new HttpError(404, 'Canvas does not exist.');
  // A secret-bearing current canvas stays read-only even when the target is clean.
  assertEditable(current, current, secret);
  const target = current.history.find(version => version.revision === targetRevision);
  if (!target) throw new HttpError(404, 'Canvas history revision does not exist.');
  assertEditable(current, target, secret);
  return nextVersion(current, { title: target.title, language: target.language, content: target.content }, 'restore', now);
}

/** All captured text is immutable and private; approval presents a redacted copy only. */
export type CanvasRangeProposal = Readonly<CanvasRangeInput & Pick<CanvasVersion, 'title' | 'language' | 'content'>>;
export function prepareCanvasRange(current: Canvas | undefined, input: CanvasRangeInput, secret = ''): CanvasRangeProposal {
  const parsed = canvasRangeSchema.parse(input);
  assertRevision(current, parsed.revision);
  if (!current) throw new HttpError(404, 'Canvas does not exist.');
  assertEditable(current, current, secret);
  const start = current.content.indexOf(parsed.oldText);
  if (start === -1) throw new HttpError(409, 'The exact old text is absent from the canvas. Read the latest canvas before proposing another range.');
  // Start one JS unit later to count overlapping occurrences too. No byte offsets are exposed.
  if (current.content.indexOf(parsed.oldText, start + 1) !== -1) throw new HttpError(409, 'The exact old text is ambiguous. Choose a range that occurs exactly once.');
  const content = current.content.slice(0, start) + parsed.replacement + current.content.slice(start + parsed.oldText.length);
  const fields = fieldsSchema.parse({ title: current.title, language: current.language, content });
  assertEditable(current, fields, secret);
  return Object.freeze({ ...parsed, ...fields });
}

export function replaceCanvasRange(current: Canvas | undefined, proposal: CanvasRangeProposal, now: string, secret = ''): Canvas {
  const checked = prepareCanvasRange(current, { revision: proposal.revision, oldText: proposal.oldText, replacement: proposal.replacement }, secret);
  if (!sameFields(checked, proposal)) throw new HttpError(409, 'Canvas changed while approval was pending. Read the latest canvas before proposing another range.');
  return saveCanvas(current, { revision: proposal.revision, title: proposal.title, language: proposal.language, content: proposal.content }, now, secret, 'bot');
}

/** Keep structural metadata intact; redact only user-controlled fields, once. */
export function publicCanvas(canvas: Canvas, secret: string): Canvas {
  const redact = (value: string) => secret ? value.split(secret).join('[redacted]') : value;
  const project = (version: CanvasVersion): CanvasVersion => ({
    ...versionOf(version), title: redact(version.title), language: redact(version.language), content: redact(version.content),
    ...(canvasHasSecret(version, secret) ? { redacted: true } : {}),
  });
  return {
    ...project(canvas), history: canvas.history.map(project),
    ...(canvasHasSecret(canvas, secret) ? { redacted: true } : {}),
  };
}

export type CanvasRead = Omit<Canvas, 'history'> & { history: Omit<CanvasVersion, 'content'>[] };
export function readCanvas(canvas: Canvas | undefined, secret = ''): CanvasRead | null {
  if (!canvas) return null;
  const projected = publicCanvas(canvas, secret);
  return { ...projected, history: projected.history.map(({ content: _content, ...metadata }) => metadata) };
}
