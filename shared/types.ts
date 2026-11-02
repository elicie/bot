import type { PublicMcpServer, PublicSecret } from './vault';
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
  /** Group conversations: the member bot that wrote this assistant message. */
  speakerBotId?: string;
  attachments?: Attachment[];
  /** Hidden input of an automatic follow-up turn: the bot deliveries whose results it reports (empty once they were deleted). Never shown as text. */
  reportDeliveryIds?: string[];
}
/** A user attachment; its bytes are served by `GET /api/conversations/:id/attachments/:attachmentId`. */
export interface Attachment {
  id: string;
  name: string;
  mimeType: string;
  kind: 'text' | 'image';
  size: number;
}
export interface AttachmentUpload {
  name: string;
  mimeType: string;
  /** Standard base64. */
  data: string;
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
  /** The follow-up turn that reports this delivery's result to the sender conversation. */
  report?: DeliveryReport;
}
export type ReportStatus = 'queued' | 'running' | 'completed' | 'cancelled' | 'error' | 'interrupted' | 'skipped';
export interface DeliveryReport {
  status: ReportStatus;
  runId?: string;
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
export type TodoStatus = 'pending' | 'in_progress' | 'completed' | 'cancelled';
export interface TodoItem { content: string; status: TodoStatus }
export interface RunState {
  id: string;
  status: RunStatus;
  error?: string;
  permission?: PendingPermission;
  startedAt?: string;
  finishedAt?: string;
  /** Latest complete engine plan for this turn; absent until the first update. */
  todos?: TodoItem[];
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
  /** Group conversations: the group ID (`botId` holds the same ID). */
  groupId?: string;
  /** Thread conversations: the message the thread replies to. */
  thread?: ThreadLink;
  /** Group conversations: members still to answer the latest user message. */
  groupRound?: GroupRound;
  memberSessions?: { botId: string; sessionId: string; sessionFingerprint: string }[];
}
export interface ThreadLink { parentConversationId: string; parentMessageId: string }
export interface GroupRound { id: string; userMessageId: string; pendingBotIds: string[]; createdAt: string }
export interface Group {
  id: string;
  name: string;
  color: string;
  shape?: AvatarShape;
  /** Speaking order. */
  memberBotIds: string[];
  pinned?: boolean;
  createdAt: string;
  updatedAt: string;
}
export interface GroupInput { name: string; color?: string; shape?: AvatarShape; memberBotIds: string[] }
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
  unread?: boolean;
  notifyOnUpdates?: boolean;
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
  groups?: Group[];
  routineSchedulingError?: string;
  settings: PublicSettings;
  runtime: RuntimeInfo;
  /** Vault secret names and scopes (never values). */
  secrets?: PublicSecret[];
  /** Custom MCP server summaries. */
  mcpServers?: PublicMcpServer[];
  /** The main bot ("메인 Bot"). Absent from older servers: the first bot is the main bot. */
  mainBotId?: string;
  /** Opt-in server-side link previews. Missing on legacy Node servers: unsupported. */
  linkPreviewsEnabled?: boolean;
}
export type PermissionCategory = 'file_write' | 'bot_message' | 'bot_create' | 'skill_save' | 'memory_save' | 'mcp_tool';
export type PermissionAction = 'ask' | 'allow';
export interface PermissionRule {
  id: string;
  category: PermissionCategory;
  action: PermissionAction;
  /** Absent: the rule applies to every bot. */
  botId?: string;
  /** `mcp_tool` only: the custom MCP server the rule is limited to (absent: every server). */
  server?: string;
  createdAt: string;
  updatedAt: string;
}
export interface PermissionRuleInput {
  category: PermissionCategory;
  action: PermissionAction;
  botId?: string;
  server?: string;
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
