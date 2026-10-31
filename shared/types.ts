export type RunStatus = 'idle' | 'starting' | 'running' | 'awaiting_permission' | 'completed' | 'cancelled' | 'error' | 'interrupted';
export type ToolStatus = 'pending' | 'in_progress' | 'completed' | 'failed';
export interface ToolActivity {
  id: string;
  title: string;
  kind: string;
  status: ToolStatus;
  input?: string;
  output?: string;
  paths?: string[];
}
export interface Message {
  id: string;
  role: 'user' | 'assistant';
  text: string;
  createdAt: string;
  tools: ToolActivity[];
}
export interface PermissionOption { id: string; label: string; kind: string }
export interface PendingPermission {
  id: string;
  title: string;
  kind: string;
  description: string;
  options: PermissionOption[];
  paths: string[];
}
export interface RunState {
  id: string;
  status: RunStatus;
  error?: string;
  permission?: PendingPermission;
  startedAt?: string;
  finishedAt?: string;
}
export interface Conversation {
  id: string;
  botId: string;
  title: string;
  messages: Message[];
  sessionId?: string;
  sessionFingerprint?: string;
  run: RunState;
  updatedAt: string;
}
export interface Bot {
  id: string;
  name: string;
  color: string;
  instructions: string;
  createdAt: string;
}
export interface PublicSettings {
  baseUrl: string;
  model: string;
  hasApiKey: boolean;
  workspace: string;
}
export interface SettingsInput {
  baseUrl: string;
  model: string;
  workspace: string;
  apiKey?: string;
  clearApiKey?: boolean;
}
export interface RuntimeInfo {
  available: boolean;
  version?: string;
  error?: string;
}
export interface AppState {
  revision: number;
  bots: Bot[];
  conversations: Conversation[];
  settings: PublicSettings;
  runtime: RuntimeInfo;
}
export interface FileEntry { name: string; path: string; type: 'directory' | 'file' }
export interface FileListing { path: string; entries: FileEntry[] }
export interface FileDocument { path: string; content: string; hash: string }
export interface ProviderCheck { ok: boolean; models: string[]; error?: string }
export interface ApiError { error: string }
