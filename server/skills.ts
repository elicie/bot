import { createHash, randomUUID } from 'node:crypto';
import { z } from 'zod';
import { parseDocument } from 'yaml';
import { HttpError } from './files.js';
import type { Skill } from '../shared/types.js';

export const MAX_SKILLS_PER_BOT = 100;
export const MAX_SKILL_RESPONSE_BYTES = 18 * 1024;
export const MAX_ENABLED_SKILL_BYTES = 128 * 1024;

export function skillResponseText(value: unknown): string {
  // Upstream scans MCP text for data URIs before JSON reaches the model.
  return JSON.stringify(value).replace(/data:/gi, match => `${match.slice(0, -1)}\\u003a`);
}
const skillFieldsSchema = z.object({
  name: z.string().trim().min(1).max(100).refine(value => !/[\u0000-\u001f\u007f]/.test(value), 'Skill names cannot contain control characters.'),
  description: z.string().trim().min(1).max(2000),
  instructions: z.string().min(1).max(30000).refine(value => value.trim().length > 0 && !value.includes('\0'), 'Provide nonempty skill instructions without NUL bytes.'),
}).strict();
const reviewableSkill = (value: { name: string; description: string; instructions: string }) => Buffer.byteLength(JSON.stringify(value), 'utf8') <= MAX_SKILL_RESPONSE_BYTES;
const sizeError = 'Skill text exceeds the 18 KiB serialized limit. Shorten it so the engine receives the complete instructions.';
export const skillInputSchema = skillFieldsSchema.refine(reviewableSkill, sizeError);
export type SkillInput = z.infer<typeof skillInputSchema>;
export const skillPatchSchema = skillFieldsSchema.partial().extend({ revision: z.number().int().min(1), enabled: z.boolean().optional() }).strict();
export const skillSchema = skillFieldsSchema.extend({
  id: z.string().uuid(), enabled: z.boolean(), revision: z.number().int().min(1), createdAt: z.iso.datetime(), updatedAt: z.iso.datetime(),
}).strict() satisfies z.ZodType<Skill>;

export function skillSetBytes(skills: readonly Skill[]): number {
  return Buffer.byteLength(JSON.stringify(skills.filter(skill => skill.enabled).map(({ id, name, description, instructions }) => ({ id, name, description, instructions }))), 'utf8');
}

export function validateSkillSet(skills: readonly Skill[]): void {
  if (skills.some(skill => !reviewableSkill({ name: skill.name, description: skill.description, instructions: skill.instructions }))) throw new HttpError(413, sizeError);
  if (skills.length > MAX_SKILLS_PER_BOT) throw new HttpError(409, 'A bot can contain at most 100 private skills.');
  if (new Set(skills.map(skill => skill.id)).size !== skills.length) throw new HttpError(400, 'Private skill IDs must be unique.');
  if (new Set(skills.map(skill => skill.name.toLocaleLowerCase('en-US'))).size !== skills.length) throw new HttpError(409, 'A skill with this name already exists for the bot.');
  if (skillSetBytes(skills) > MAX_ENABLED_SKILL_BYTES) throw new HttpError(413, 'Enabled private skill instructions exceed the 128 KiB per-bot limit. Disable a skill before adding more.');
}

export function newSkill(input: SkillInput, now = new Date().toISOString()): Skill {
  return { ...skillInputSchema.parse(input), id: randomUUID(), enabled: true, revision: 1, createdAt: now, updatedAt: now };
}

export function nextSkill(current: Skill, input: z.infer<typeof skillPatchSchema>, now = new Date().toISOString()): Skill {
  const { revision, ...fields } = skillPatchSchema.parse(input);
  if (current.revision !== revision) throw new HttpError(409, 'The skill changed since it was opened. Reload its latest version before saving.');
  if (!Object.keys(fields).length) throw new HttpError(400, 'Provide a skill field to update.');
  const next = { ...current, ...fields, revision: current.revision + 1, updatedAt: now };
  if (!reviewableSkill({ name: next.name, description: next.description, instructions: next.instructions })) throw new HttpError(413, sizeError);
  return next;
}

export function skillFingerprint(skills: readonly Skill[]): string {
  return createHash('sha256').update(JSON.stringify(skills.filter(skill => skill.enabled).map(({ id, revision, name, description, instructions }) => ({ id, revision, name, description, instructions })))).digest('hex');
}

export function importSkillMarkdown(markdown: string): SkillInput {
  if (Buffer.byteLength(markdown, 'utf8') > 20 * 1024 || markdown.includes('\0')) throw new HttpError(413, 'SKILL.md must be UTF-8 text of at most 20 KiB without NUL bytes.');
  const match = markdown.replace(/^\uFEFF/, '').match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n([\s\S]*)$/);
  if (!match || Buffer.byteLength(match[1], 'utf8') > 4096) throw new HttpError(400, 'SKILL.md requires a YAML frontmatter block of at most 4096 bytes.');
  const document = parseDocument(match[1], { strict: true, uniqueKeys: true });
  if (document.errors.length || document.warnings.length) throw new HttpError(400, 'SKILL.md frontmatter must contain valid YAML with unique fields.');
  let value: unknown;
  try { value = document.toJS({ maxAliasCount: 0 }); } catch { throw new HttpError(400, 'SKILL.md YAML aliases and custom values are unsupported.'); }
  const fields = z.object({ name: z.string().max(64).regex(/^[a-z0-9]+(?:-[a-z0-9]+)*$/), description: z.string().trim().min(1).max(1024) }).strict().safeParse(value);
  if (!fields.success) throw new HttpError(400, 'SKILL.md supports only name (lowercase letters, digits, hyphens; 64 characters) and description (1024 characters). Tool, script, model, and hook fields are unsupported.');
  return skillInputSchema.parse({ ...fields.data, instructions: match[2] });
}
