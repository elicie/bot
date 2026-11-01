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
  localOnly?: boolean;
  /** ID of the permission rule that approved this action without asking. */
  approvedByRule?: string;
}
export interface Message {
  id: string;
  role: 'user' | 'assistant';
  text: string;
  createdAt: string;
  tools: ToolActivity[];
  botExchangeIds?: string[];
  sourceBotId?: string;
}
export interface BotExchange {
  id: string;
  botIds: [string, string];
  messages: { id: string; botId: string; text: string; createdAt: string }[];
  sourceConversationId: string;
  sourceMessageId: string;
  updatedAt: string;
}
export interface BotDelivery {
  id: string;
  exchangeId: string;
  sourceConversationId: string;
  sourceMessageId: string;
  recipientConversationId: string;
  fromBotId: string;
  toBotId: string;
  text: string;
  chainId: string;
  hop: number;
  createdAt: string;
  status: 'queued' | 'running' | 'completed' | 'cancelled' | 'error' | 'interrupted';
  requestId?: string;
  executionRunId?: string;
  startedAt?: string;
  finishedAt?: string;
  error?: string;
}
export interface PermissionOption { id: string; label: string; kind: string }
export interface PendingPermission {
  id: string;
  title: string;
  kind: string;
  description: string;
  options: PermissionOption[];
  paths: string[];
  localOnly?: boolean;
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
export type AvatarShape = 'blob' | 'cloud' | 'square' | 'spark' | 'clover' | 'heart' | 'flower' | 'droplet' | 'pill' | 'wedge' | 'house' | 'star' | 'hexagon' | 'pumpkin';
export interface Skill {
  id: string;
  name: string;
  description: string;
  instructions: string;
  enabled: boolean;
  revision: number;
  createdAt: string;
  updatedAt: string;
}
export interface Bot {
  id: string;
  name: string;
  color: string;
  instructions: string;
  shape?: AvatarShape;
  skills?: Skill[];
  pinned?: boolean;
  hidden?: boolean;
  createdAt: string;
  memories?: import('./bot-data').Memory[];
  knowledge?: import('./bot-data').KnowledgeFile[];
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
export type RoutineSchedule =
  | { kind: 'once'; at: string }
  | { kind: 'interval'; minutes: number }
  | { kind: 'daily'; time: string; timezone: string };
export interface Routine {
  id: string;
  botId: string;
  name: string;
  prompt: string;
  schedule: RoutineSchedule;
  enabled: boolean;
  nextRunAt?: string;
  createdAt: string;
  updatedAt: string;
  webhook?: RoutineWebhook;
}
/** Public view of a routine webhook: the key itself is shown only once, when it is created. */
export interface RoutineWebhook {
  keyHint: string;
  createdAt: string;
}
export interface RoutineWebhookKey {
  key: string;
  keyHint: string;
  createdAt: string;
  state: AppState;
}
export interface RoutineInput {
  botId: string;
  name: string;
  prompt: string;
  schedule: RoutineSchedule;
  enabled: boolean;
}
export interface RoutineResult {
  status: 'completed' | 'cancelled' | 'error' | 'interrupted';
  startedAt?: string;
  finishedAt: string;
  error?: string;
}
export interface RoutineRun {
  id: string;
  routineId: string;
  routineName: string;
  botId: string;
  conversationId: string;
  prompt: string;
  trigger: 'manual' | 'scheduled' | 'webhook';
  scheduledFor: string;
  createdAt: string;
  executionRunId?: string;
  result?: RoutineResult;
}
export interface RoutineRunResult {
  run: RoutineRun;
  state: AppState;
}
export interface AppState {
  revision: number;
  bots: Bot[];
  conversations: Conversation[];
  routines: Routine[];
  routineRuns: RoutineRun[];
  botExchanges?: BotExchange[];
  botDeliveries?: BotDelivery[];
  permissionRules?: PermissionRule[];
  routineSchedulingError?: string;
  settings: PublicSettings;
  runtime: RuntimeInfo;
}
export type PermissionCategory = 'file_write' | 'bot_message' | 'bot_create' | 'skill_save' | 'memory_save';
export type PermissionAction = 'ask' | 'allow';
export interface PermissionRule {
  id: string;
  category: PermissionCategory;
  action: PermissionAction;
  /** Absent: the rule applies to every bot. */
  botId?: string;
  createdAt: string;
  updatedAt: string;
}
export interface PermissionRuleInput {
  category: PermissionCategory;
  action: PermissionAction;
  botId?: string;
}
export interface PermissionRuleList {
  rules: PermissionRule[];
  canAutoAllow: boolean;
}
export interface FileEntry { name: string; path: string; type: 'directory' | 'file' }
export interface FileListing { path: string; entries: FileEntry[] }
export interface FileDocument { path: string; content: string; hash: string }
export interface ProviderCheck { ok: boolean; models: string[]; error?: string }
export interface ApiError { error: string }
