import type {
  AppState,
  Bot,
  Conversation,
  FileDocument,
  FileListing,
  RunStatus,
} from '../shared/types';

export type SettingsTab = 'general' | 'models' | 'files' | 'permissions' | 'vault' | 'usage' | 'updates';
export type Overlay =
  | { kind: 'settings'; tab: SettingsTab }
  | { kind: 'bot'; bot?: Bot }
  | { kind: 'history' }
  | { kind: 'capabilities' }
  | null;
export interface Selection {
  botId: string | null;
  conversationId: string | null;
}
export type Surface =
  | { kind: 'chat'; thread?: string }
  | { kind: 'routines' }
  | { kind: 'bot-chat'; exchangeId: string }
  | { kind: 'group'; groupId: string; conversationId: string | null; thread?: string };
export interface AppRoute {
  selection: Selection;
  surface: Surface;
}
export type Resource<T> =
  { status: 'loading' } | { status: 'ready'; value: T } | { status: 'error'; error: string };
export type Editor =
  | { kind: 'closed' }
  | { kind: 'loading'; path: string; requestId: string }
  | { kind: 'error'; path: string; error: string }
  | {
      kind: 'ready';
      document: FileDocument;
      content: string;
      save: 'idle' | 'saving' | 'saved' | 'error';
      error?: string;
    };
export interface FilesState {
  directory: string;
  listing: Resource<FileListing>;
  editor: Editor;
  revision: number;
  documentRevision: number;
}
export interface UIState {
  data: AppState | null;
  connection: 'loading' | 'online' | 'reconnecting' | 'error';
  connectionError: string | null;
  selection: Selection;
  surface: Surface;
  lastConversation: Record<string, string | null>;
  drafts: Record<string, string>;
  overlay: Overlay;
  sidebarOpen: boolean;
  viewport: 'mobile' | 'desktop';
  computerOpen: boolean;
  command: string | null;
  error: string | null;
  search: string;
  files: FilesState;
}
export const activeRun = (status: RunStatus) =>
  status === 'starting' || status === 'running' || status === 'awaiting_permission';
export const runningConversation = (data: AppState | null) =>
  data?.conversations.find((conversation) => activeRun(conversation.run.status));
export const queuedRoutineRun = (data: AppState | null, conversation: Conversation | undefined) =>
  conversation?.run.status === 'idle'
    ? data?.routineRuns.find((run) => run.conversationId === conversation.id && !run.result)
    : undefined;
export const conversationsFor = (data: AppState | null, botId: string | null) =>
  (data?.conversations ?? [])
    .filter((conversation) => conversation.botId === botId && !conversation.thread)
    .sort((a, b) => b.updatedAt.localeCompare(a.updatedAt));
/** The main bot ("메인 Bot"): the chosen one, or the first bot when none is chosen (or the server has none). */
export const mainBotId = (data: AppState | null | undefined): string | null =>
  (data?.mainBotId && data.bots.some((bot) => bot.id === data.mainBotId) ? data.mainBotId : data?.bots[0]?.id) ?? null;
export const selectedBot = (state: UIState) =>
  state.data?.bots.find((bot) => bot.id === state.selection.botId);
export const selectedConversation = (state: UIState) =>
  state.data?.conversations.find(
    (conversation) =>
      conversation.id === state.selection.conversationId &&
      conversation.botId === state.selection.botId,
  );
export const draftKey = (selection: Selection) =>
  selection.conversationId ?? `new:${selection.botId ?? ''}`;
export const isDirty = (editor: Editor) =>
  editor.kind === 'ready' && editor.content !== editor.document.content;
export const emptyFiles = (): FilesState => ({
  directory: '',
  listing: { status: 'loading' },
  editor: { kind: 'closed' },
  revision: 0,
  documentRevision: 0,
});

export function readRoute(search = window.location.search): AppRoute {
  const params = new URLSearchParams(search);
  const thread = params.get('thread') ? { thread: params.get('thread')! } : {};
  const group = params.get('group');
  if (group)
    return {
      selection: { botId: params.get('bot'), conversationId: null },
      surface: { kind: 'group', groupId: group, conversationId: params.get('conversation'), ...thread },
    };
  return {
    selection: { botId: params.get('bot'), conversationId: params.get('conversation') },
    surface: params.get('view') === 'routines' ? { kind: 'routines' } : params.get('view') === 'bot-chat' && params.get('exchange') ? { kind: 'bot-chat', exchangeId: params.get('exchange')! } : { kind: 'chat', ...thread },
  };
}
export function routeSearch(route: AppRoute): string {
  const params = new URLSearchParams();
  if (route.surface.kind === 'group') {
    params.set('group', route.surface.groupId);
    if (route.surface.conversationId) params.set('conversation', route.surface.conversationId);
    if (route.surface.thread) params.set('thread', route.surface.thread);
    return `?${params}`;
  }
  if (route.selection.botId) params.set('bot', route.selection.botId);
  if (route.selection.conversationId) params.set('conversation', route.selection.conversationId);
  if (route.surface.kind === 'routines') params.set('view', 'routines');
  if (route.surface.kind === 'bot-chat') { params.set('view', 'bot-chat'); params.set('exchange', route.surface.exchangeId); }
  if (route.surface.kind === 'chat' && route.surface.thread) params.set('thread', route.surface.thread);
  return params.size ? `?${params}` : '';
}
export function initialState(): UIState {
  return {
    data: null,
    connection: 'loading',
    connectionError: null,
    ...readRoute(),
    lastConversation: {},
    drafts: {},
    overlay: null,
    sidebarOpen: false,
    viewport: window.matchMedia('(max-width: 760px)').matches ? 'mobile' : 'desktop',
    computerOpen: false,
    command: null,
    error: null,
    search: '',
    files: emptyFiles(),
  };
}
function reconcile(
  selection: Selection,
  data: AppState,
  history: Record<string, string | null>,
): Selection {
  const routedConversation = data.conversations.find(
    (conversation) => conversation.id === selection.conversationId,
  );
  const selectedBotId = data.bots.find((bot) => bot.id === selection.botId)?.id;
  const botId = selectedBotId ?? routedConversation?.botId ?? mainBotId(data);
  if (selectedBotId && selection.conversationId === null)
    return { botId, conversationId: null };
  const conversationId =
    routedConversation?.botId === botId
      ? routedConversation.id
      : (data.conversations.find(
          (conversation) =>
            conversation.id === history[botId ?? ''] && conversation.botId === botId,
        )?.id ??
        conversationsFor(data, botId)[0]?.id ??
        null);
  return { botId, conversationId };
}

export type Action =
  | { type: 'snapshot'; data: AppState }
  | { type: 'connection'; status: UIState['connection']; error?: string }
  | { type: 'select'; selection: Selection; expected?: Selection }
  | { type: 'route'; route: AppRoute }
  | { type: 'draft'; key: string; text: string }
  | { type: 'draft-sent'; key: string; text: string }
  | { type: 'bind-draft'; from: string; to: string }
  | { type: 'overlay'; overlay: Overlay }
  | { type: 'sidebar'; open: boolean }
  | { type: 'viewport'; viewport: UIState['viewport'] }
  | { type: 'computer'; open: boolean }
  | { type: 'command'; command: string | null }
  | { type: 'error'; error: string | null }
  | { type: 'search'; value: string }
  | { type: 'directory'; path: string }
  | { type: 'listing'; listing: Resource<FileListing> }
  | { type: 'refresh-files' }
  | { type: 'refresh-document' }
  | { type: 'open-file'; path: string; requestId: string }
  | { type: 'file-loaded'; document: FileDocument; requestId: string }
  | { type: 'document-revalidated'; document: FileDocument; previousHash: string }
  | { type: 'file-error'; path: string; error: string; requestId: string }
  | { type: 'close-file' }
  | { type: 'edit-file'; content: string }
  | { type: 'save-start' }
  | { type: 'file-saved'; document: FileDocument; previousHash: string }
  | { type: 'save-error'; path: string; previousHash: string; error: string };

export function reducer(state: UIState, action: Action): UIState {
  switch (action.type) {
    case 'snapshot': {
      if (state.data && action.data.revision < state.data.revision) return state;
      const workspaceChanged =
        state.data !== null && state.data.settings.workspace !== action.data.settings.workspace;
      const selection = reconcile(
        state.selection,
        action.data,
        state.lastConversation,
      );
      return {
        ...state,
        data: action.data,
        selection,
        connection: 'online',
        connectionError: null,
        files: workspaceChanged ? emptyFiles() : state.files,
        lastConversation:
          selection.botId
            ? { ...state.lastConversation, [selection.botId]: selection.conversationId }
            : state.lastConversation,
      };
    }
    case 'connection':
      return { ...state, connection: action.status, connectionError: action.error ?? null };
    case 'route':
    case 'select': {
      if (
        action.type === 'select' &&
        action.expected &&
        (state.selection.botId !== action.expected.botId ||
          state.selection.conversationId !== action.expected.conversationId)
      )
        return state;
      const next = action.type === 'route' ? action.route.selection : action.selection;
      const selection = state.data
        ? reconcile(next, state.data, state.lastConversation)
        : next;
      return {
        ...state,
        selection,
        surface: action.type === 'route' ? action.route.surface : { kind: 'chat' },
        sidebarOpen: false,
        lastConversation:
          selection.botId
            ? { ...state.lastConversation, [selection.botId]: selection.conversationId }
            : state.lastConversation,
      };
    }
    case 'draft':
      return { ...state, drafts: { ...state.drafts, [action.key]: action.text } };
    case 'draft-sent':
      return state.drafts[action.key] === action.text
        ? { ...state, drafts: { ...state.drafts, [action.key]: '' } }
        : state;
    case 'bind-draft':
      return {
        ...state,
        drafts: {
          ...state.drafts,
          [action.to]: state.drafts[action.from] ?? '',
          [action.from]: '',
        },
      };
    case 'overlay':
      return { ...state, overlay: action.overlay, sidebarOpen: false };
    case 'sidebar':
      return { ...state, sidebarOpen: action.open };
    case 'viewport':
      return { ...state, viewport: action.viewport, sidebarOpen: false };
    case 'computer':
      return { ...state, computerOpen: action.open };
    case 'command':
      return { ...state, command: action.command };
    case 'error':
      return { ...state, error: action.error };
    case 'search':
      return { ...state, search: action.value };
    case 'directory':
      return {
        ...state,
        files: { ...state.files, directory: action.path, listing: { status: 'loading' } },
      };
    case 'listing':
      return { ...state, files: { ...state.files, listing: action.listing } };
    case 'refresh-files':
      return { ...state, files: { ...state.files, revision: state.files.revision + 1 } };
    case 'refresh-document':
      return {
        ...state,
        files: { ...state.files, documentRevision: state.files.documentRevision + 1 },
      };
    case 'document-revalidated': {
      const editor = state.files.editor;
      if (editor.kind !== 'ready' || editor.document.path !== action.document.path || editor.document.hash !== action.previousHash || editor.save === 'saving') return state;
      if (editor.document.hash === action.document.hash) return state;
      if (isDirty(editor)) return { ...state, files: { ...state.files, editor: { ...editor, error: '파일이 외부 작업으로 변경되었습니다. 입력 내용을 보관한 뒤 새로고침해 주세요.' } } };
      return { ...state, files: { ...state.files, editor: { kind: 'ready', document: action.document, content: action.document.content, save: 'idle' } } };
    }
    case 'open-file':
      return {
        ...state,
        files: {
          ...state.files,
          editor: { kind: 'loading', path: action.path, requestId: action.requestId },
        },
      };
    case 'file-loaded':
      return state.files.editor.kind === 'loading' &&
        state.files.editor.requestId === action.requestId
        ? {
            ...state,
            files: {
              ...state.files,
              editor: {
                kind: 'ready',
                document: action.document,
                content: action.document.content,
                save: 'idle',
              },
            },
          }
        : state;
    case 'file-error':
      return state.files.editor.kind === 'loading' &&
        state.files.editor.requestId === action.requestId
        ? {
            ...state,
            files: {
              ...state.files,
              editor: { kind: 'error', path: action.path, error: action.error },
            },
          }
        : state;
    case 'close-file':
      return { ...state, files: { ...state.files, editor: { kind: 'closed' } } };
    case 'edit-file':
      return state.files.editor.kind === 'ready'
        ? {
            ...state,
            files: {
              ...state.files,
              editor: {
                ...state.files.editor,
                content: action.content,
                save: state.files.editor.save === 'saving' ? 'saving' : 'idle',
                error: undefined,
              },
            },
          }
        : state;
    case 'save-start':
      return state.files.editor.kind === 'ready'
        ? {
            ...state,
            files: {
              ...state.files,
              editor: { ...state.files.editor, save: 'saving', error: undefined },
            },
          }
        : state;
    case 'file-saved':
      return state.files.editor.kind === 'ready' &&
        state.files.editor.document.path === action.document.path &&
        state.files.editor.document.hash === action.previousHash
        ? {
            ...state,
            files: {
              ...state.files,
              revision: state.files.revision + 1,
              editor: {
                ...state.files.editor,
                document: action.document,
                save: state.files.editor.content === action.document.content ? 'saved' : 'idle',
                error: undefined,
              },
            },
          }
        : state;
    case 'save-error':
      return state.files.editor.kind === 'ready' &&
        state.files.editor.document.path === action.path &&
        state.files.editor.document.hash === action.previousHash
        ? {
            ...state,
            files: {
              ...state.files,
              editor: { ...state.files.editor, save: 'error', error: action.error },
            },
          }
        : state;
  }
}

export const runLabels: Record<RunStatus, string> = {
  idle: '대화 준비',
  starting: '시작 중',
  running: '작업 중',
  awaiting_permission: '승인 대기',
  completed: '완료',
  cancelled: '중단됨',
  error: '실행 오류',
  interrupted: '연결이 끊겨 작업이 중단되었습니다',
};
export function conversationPreview(conversation: Conversation | undefined): string {
  if (!conversation) return '새 대화를 시작해 보세요';
  if (activeRun(conversation.run.status)) return runLabels[conversation.run.status];
  if (conversation.run.status === 'error') return conversation.run.error ?? '실행 오류';
  const message = [...conversation.messages].reverse().find((message) => message.text.trim());
  return message?.text.replace(/\s+/g, ' ') ?? '아직 메시지가 없습니다';
}
