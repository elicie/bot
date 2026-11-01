// Per-bot memories, knowledge files, and bot templates. Mirrors `crates/model/src/bot_data.rs`.
import type { AppState, AvatarShape, Bot } from './types';

export type MemorySource = 'user' | 'bot' | 'import';
export interface Memory {
  id: string;
  text: string;
  source: MemorySource;
  createdAt: string;
}
export interface KnowledgeFile {
  id: string;
  name: string;
  bytes: number;
  hash: string;
  createdAt: string;
}
export interface KnowledgeDocument {
  file: KnowledgeFile;
  content: string;
}
export interface MemoryPreviewItem {
  text: string;
  duplicate: boolean;
  error?: string;
}
export interface BotTemplate {
  format: 'grok-bot.bot-template';
  version: 1;
  bot: { name: string; color: string; instructions: string; shape?: AvatarShape };
  skills: { name: string; description: string; instructions: string; enabled: boolean }[];
  memories?: { text: string }[];
}
export interface TemplateImportResult {
  bot: Bot;
  state: AppState;
}
