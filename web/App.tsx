import { useCallback, useEffect, useReducer, useRef, useState } from 'react';
import {
  Clock3,
  Repeat2,
  Menu,
  Monitor,
  Plus,
  Search,
  Settings2,
  Plug,
  SquarePen,
  X,
  ChevronRight,
  UserRound,
} from 'lucide-react';
import type {
  AppState,
  Bot,
  Conversation,
  Group,
  Routine,
  RoutineInput,
  RoutineRunResult,
  SettingsInput,
} from '../shared/types';
import { cancelQueuedTurn, errorText, getState, request } from './api';
import { BotDialog, History } from './Bots';
import { Composer, OtherRuns, Transcript } from './Conversation';
import { QueuedTurns } from './QueuedTurns';
import { BotChat } from './BotChat';
import { Sidebar } from './Sidebar';
import { Details, type DetailsTab } from './Details';
import { Computer } from './Computer';
import { Routines, type RoutineDraftStatus } from './Routines';
import { Settings } from './Settings';
import { Skills, confirmSkillsNavigation } from './Skills';
import { GroupDialog, GroupList, GroupView, shownGroupConversation } from './Groups';
import { ThreadLinks, ThreadPanel } from './Threads';
import {
  activeRun,
  conversationPreview,
  conversationsFor,
  draftKey,
  initialState,
  isDirty,
  isBotRunning,
  hasConversationWork,
  readRoute,
  queuedRoutineRun,
  reducer,
  recipientDelivery,
  runningConversation,
  selectedBot,
  selectedConversation,
  routeSearch,
  type AppRoute,
  type Overlay,
  type Selection,
  type SettingsTab,
} from './state';
import { Avatar, ErrorNotice, IconButton, Spinner } from './ui';
import { BotDeleteDialog, BotMenuExtras } from './BotMenu';
import { MainBotDialog } from './MainBot';
import { notificationDeniedText, notificationUnavailableText, requestNotificationPermission, useBotActivity } from './activity';
import { admitFiles, uploads, type PendingAttachment } from './Attachments';
import { LinkPreviewProvider } from './LinkPreviews';
import { CanvasButton, CanvasPanel, type CanvasDraftStatus } from './Canvas';

function historyPosition(): number {
  const position: unknown = window.history.state?.grokBotPosition;
  return typeof position === 'number' && Number.isSafeInteger(position) ? position : 0;
}

export function App() {
  const [state, dispatch] = useReducer(reducer, undefined, initialState);
  const [detailsTab, setDetailsTab] = useState<DetailsTab>('computer');
  const [reportNotice, setReportNotice] = useState<{ conversationId: string; text: string } | null>(null);
  const [groupDialog, setGroupDialog] = useState<{ group?: Group } | null>(null);
  const [connectionAttempt, reconnect] = useReducer((attempt: number) => attempt + 1, 0);
  const current = useRef(state);
  current.current = state;
  const commandLock = useRef(false);
  const navigationVersion = useRef(0);
  const position = useRef(historyPosition());
  const returningToPosition = useRef<number | null>(null);
  const writeRoute = (route: AppRoute, replace = false) => {
    const search = routeSearch(route);
    const unchanged = window.location.search === search && !window.location.hash;
    const recorded = window.history.state?.grokBotPosition === position.current;
    if (unchanged && recorded) return;
    const nextPosition = replace || unchanged ? position.current : position.current + 1;
    window.history[replace || unchanged ? 'replaceState' : 'pushState'](
      { ...window.history.state, grokBotPosition: nextPosition },
      '',
      `${window.location.pathname}${search}`,
    );
    position.current = nextPosition;
  };
  const routineDraft = useRef<RoutineDraftStatus>({ dirty: false, saving: false });
  const reportRoutineDraft = useCallback((status: RoutineDraftStatus) => {
    routineDraft.current = status;
  }, []);
  const bot = selectedBot(state);
  const conversation = selectedConversation(state);
  useEffect(() => setReportNotice(null), [conversation?.id]);
  const globalRun = runningConversation(state.data);
  const busy = state.command !== null;
  const data = state.data;
  const [linkPreviewsPaused, setLinkPreviewsPaused] = useState(false);
  const linkPreviewsEnabled = data?.linkPreviewsEnabled === true && !linkPreviewsPaused;
  const queuedRun = queuedRoutineRun(data, conversation);
  const ownWork = hasConversationWork(data, conversation);
  const selectionKey = draftKey(state.selection);
  const draft = state.drafts[selectionKey] ?? '';
  const [pendingFiles, setPendingFiles] = useState<Record<string, PendingAttachment[]>>({});
  const [fileErrors, setFileErrors] = useState<Record<string, string[]>>({});
  const pendingRef = useRef(pendingFiles);
  pendingRef.current = pendingFiles;
  const [deleting, setDeleting] = useState<Bot | null>(null);
  const [choosingMain, setChoosingMain] = useState<{ botId?: string } | null>(null);
  const reading = useRef(new Set<string>());
  const [canvasId, setCanvasId] = useState<string | null>(null);
  const [canvasChat, setCanvasChat] = useState(false);
  const canvasStatus = useRef<CanvasDraftStatus>({ dirty: false, saving: false });
  const reportCanvasDraft = useCallback((status: CanvasDraftStatus) => { canvasStatus.current = status; }, []);

  useEffect(() => {
    const controller = new AbortController();
    let receivedEvent = false;
    let disposed = false;
    dispatch({ type: 'connection', status: current.current.data ? 'reconnecting' : 'loading' });
    void getState(controller.signal)
      .then((snapshot) => {
        if (!disposed && !receivedEvent) dispatch({ type: 'snapshot', data: snapshot });
      })
      .catch((error) => {
        if (!controller.signal.aborted && !receivedEvent)
          dispatch({
            type: 'connection',
            status: current.current.data ? 'reconnecting' : 'error',
            error: errorText(error),
          });
      });
    const events = new EventSource('/api/events');
    events.addEventListener('state', (event) => {
      if (disposed) return;
      try {
        const snapshot: AppState = JSON.parse((event as MessageEvent<string>).data);
        if (
          !snapshot ||
          !Array.isArray(snapshot.bots) ||
          !Array.isArray(snapshot.conversations) ||
          !Array.isArray(snapshot.routines) ||
          !Array.isArray(snapshot.routineRuns) ||
          !snapshot.settings ||
          !snapshot.runtime
        )
          throw new Error('서버 상태 형식이 올바르지 않습니다.');
        receivedEvent = true;
        dispatch({ type: 'snapshot', data: snapshot });
      } catch (error) {
        dispatch({
          type: 'connection',
          status: 'error',
          error: `실시간 상태를 읽지 못했습니다. ${errorText(error)}`,
        });
      }
    });
    events.onopen = () => {
      if (current.current.data) dispatch({ type: 'connection', status: 'online' });
    };
    events.onerror = () => {
      if (!disposed)
        dispatch({
          type: 'connection',
          status: current.current.data ? 'reconnecting' : 'error',
          error: '로컬 서버 연결이 끊겼습니다. 자동으로 다시 연결합니다.',
        });
    };
    return () => {
      disposed = true;
      controller.abort();
      events.close();
    };
  }, [connectionAttempt]);

  const confirmFileChange = useCallback(
    (message = '저장하지 않은 파일 변경 사항을 버리고 이동할까요?') => {
      const editor = current.current.files.editor;
      if (editor.kind === 'ready' && editor.save === 'saving') {
        dispatch({ type: 'error', error: '파일 저장이 끝난 뒤 다시 시도해 주세요.' });
        return false;
      }
      if (!isDirty(editor)) return true;
      if (!window.confirm(message)) return false;
      dispatch({ type: 'close-file' });
      return true;
    },
    [],
  );
  const confirmRoutineChange = useCallback(() => {
    if (routineDraft.current.saving) {
      dispatch({ type: 'error', error: '루틴 저장이 끝난 뒤 다시 시도해 주세요.' });
      return false;
    }
    return (
      !routineDraft.current.dirty ||
      window.confirm('저장하지 않은 루틴 변경 사항을 버리고 이동할까요?')
    );
  }, []);
  const confirmCanvasChange = useCallback(() => {
    if (canvasStatus.current.saving) {
      dispatch({ type: 'error', error: '캔버스 저장이 끝난 뒤 다시 시도해 주세요.' });
      return false;
    }
    return !canvasStatus.current.dirty || window.confirm('저장하지 않은 캔버스 변경 사항을 버리고 이동할까요?');
  }, []);
  const confirmNavigation = useCallback(
    () => confirmSkillsNavigation() && confirmCanvasChange() && confirmRoutineChange() && confirmFileChange(),
    [confirmFileChange, confirmRoutineChange, confirmCanvasChange],
  );
  useEffect(() => {
    const routeChanged = () => {
      const nextPosition = historyPosition();
      if (returningToPosition.current !== null) {
        const delta = returningToPosition.current - nextPosition;
        if (delta) window.history.go(delta);
        else returningToPosition.current = null;
        return;
      }
      const next = readRoute();
      if (routeSearch(next) === routeSearch(current.current)) {
        position.current = nextPosition;
        return;
      }
      if (confirmNavigation()) {
        position.current = nextPosition;
        navigationVersion.current++;
        setCanvasId(null);
        dispatch({ type: 'route', route: next });
        dispatch({ type: 'overlay', overlay: null });
      } else {
        returningToPosition.current = position.current;
        window.history.go(position.current - nextPosition);
      }
    };
    const beforeUnload = (event: BeforeUnloadEvent) => {
      if (
        isDirty(current.current.files.editor) ||
        routineDraft.current.dirty ||
        routineDraft.current.saving ||
        canvasStatus.current.dirty ||
        canvasStatus.current.saving
      ) {
        event.preventDefault();
        event.returnValue = '';
      }
    };
    window.addEventListener('popstate', routeChanged);
    window.addEventListener('beforeunload', beforeUnload);
    return () => {
      window.removeEventListener('popstate', routeChanged);
      window.removeEventListener('beforeunload', beforeUnload);
    };
  }, [confirmNavigation]);
  useEffect(() => {
    if (data) writeRoute(state, true);
  }, [data !== null, state.selection.botId, state.selection.conversationId, state.surface.kind]);
  useEffect(() => {
    const media = window.matchMedia('(max-width: 760px)');
    const update = () =>
      dispatch({ type: 'viewport', viewport: media.matches ? 'mobile' : 'desktop' });
    media.addEventListener('change', update);
    return () => media.removeEventListener('change', update);
  }, []);
  useEffect(() => {
    if (!state.sidebarOpen) return;
    const previous = document.activeElement;
    const sidebar = document.querySelector<HTMLElement>('.sidebar');
    sidebar?.querySelector<HTMLElement>('button:not(:disabled), input')?.focus();
    const keyboard = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        dispatch({ type: 'sidebar', open: false });
        return;
      }
      if (event.key !== 'Tab' || !sidebar) return;
      const controls = [
        ...sidebar.querySelectorAll<HTMLElement>('button:not(:disabled), input'),
      ].filter((element) => element.getClientRects().length > 0);
      const first = controls[0];
      const last = controls.at(-1);
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last?.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first?.focus();
      }
    };
    document.addEventListener('keydown', keyboard);
    return () => {
      document.removeEventListener('keydown', keyboard);
      if (previous instanceof HTMLElement && previous.isConnected) previous.focus();
    };
  }, [state.sidebarOpen]);
  const previousRuns = useRef(new Map<string, string>());
  useEffect(() => {
    let finished = false;
    for (const item of data?.conversations ?? []) {
      const previous = previousRuns.current.get(item.id);
      if (
        previous &&
        ['starting', 'running', 'awaiting_permission'].includes(previous) &&
        !activeRun(item.run.status)
      )
        finished = true;
      previousRuns.current.set(item.id, item.run.status);
    }
    if (finished) {
      dispatch({ type: 'refresh-files' });
      dispatch({ type: 'refresh-document' });
    }
  }, [data]);

  const command = async <T,>(name: string, operation: () => Promise<T>): Promise<T | undefined> => {
    if (commandLock.current) return undefined;
    commandLock.current = true;
    dispatch({ type: 'command', command: name });
    dispatch({ type: 'error', error: null });
    try {
      return await operation();
    } catch (error) {
      dispatch({ type: 'error', error: errorText(error) });
      return undefined;
    } finally {
      commandLock.current = false;
      dispatch({ type: 'command', command: null });
    }
  };
  /** Small bot-state updates that must not take the command lock (they run beside active runs). */
  const botState = async (botId: string, route: 'unread' | 'notify', body: unknown) => {
    try {
      const snapshot = await request<AppState>(`/api/bots/${encodeURIComponent(botId)}/${route}`, { method: 'POST', body });
      dispatch({ type: 'snapshot', data: snapshot });
    } catch (error) {
      dispatch({ type: 'error', error: errorText(error) });
    }
  };
  const markRead = (botId: string) => {
    if (reading.current.has(botId)) return;
    reading.current.add(botId);
    void botState(botId, 'unread', { unread: false }).finally(() => reading.current.delete(botId));
  };
  const setNotify = async (bot: Bot, enabled: boolean) => {
    // Ask inside the click so the browser can show its permission prompt.
    const permission = enabled ? requestNotificationPermission() : null;
    await botState(bot.id, 'notify', { enabled });
    if (!permission) return;
    const support = await permission;
    if (support === 'unavailable') dispatch({ type: 'error', error: notificationUnavailableText });
    else if (support === 'denied') dispatch({ type: 'error', error: notificationDeniedText });
  };
  const addFiles = async (key: string, files: File[]) => {
    const { added, errors } = await admitFiles(pendingRef.current[key] ?? [], files);
    setPendingFiles((all) => ({ ...all, [key]: [...(all[key] ?? []), ...added] }));
    setFileErrors((all) => ({ ...all, [key]: errors }));
  };
  const deleteBot = async (bot: Bot) => {
    const deleted = await command('bot-delete', async () => {
      const snapshot = await request<AppState>(`/api/bots/${encodeURIComponent(bot.id)}`, { method: 'DELETE', body: {} });
      dispatch({ type: 'snapshot', data: snapshot });
      return true;
    });
    if (deleted) setDeleting(null);
  };
  const openOverlay = (overlay: Overlay) => {
    if (!confirmSkillsNavigation()) return;
    dispatch({ type: 'sidebar', open: false });
    dispatch({ type: 'error', error: null });
    dispatch({ type: 'overlay', overlay });
  };
  const settings = (tab: SettingsTab = 'general') => openOverlay({ kind: 'settings', tab });
  const navigate = (route: AppRoute) => {
    if (returningToPosition.current !== null) return false;
    if (routeSearch(route) === routeSearch(current.current)) {
      if (!confirmSkillsNavigation()) return false;
      dispatch({ type: 'sidebar', open: false });
      dispatch({ type: 'overlay', overlay: null });
      return true;
    }
    if (!confirmNavigation()) return false;
    setCanvasId(null);
    navigationVersion.current++;
    writeRoute(route);
    dispatch({ type: 'route', route });
    dispatch({ type: 'overlay', overlay: null });
    dispatch({ type: 'error', error: null });
    return true;
  };
  const select = (selection: Selection) => navigate({ selection, surface: { kind: 'chat' } });
  const selectCreated = (selection: Selection, expectedVersion: number) => {
    if (navigationVersion.current !== expectedVersion || !confirmCanvasChange()) return false;
    navigationVersion.current++;
    setCanvasId(null);
    writeRoute({ selection, surface: { kind: 'chat' } });
    dispatch({ type: 'select', selection });
    return true;
  };
  const newConversation = async () => {
    const origin = current.current.selection;
    const version = navigationVersion.current;
    const botId = origin.botId;
    if (!botId || !confirmNavigation()) return;
    await command('conversation', async () => {
      const result = await request<{ conversation: Conversation; state: AppState }>(
        '/api/conversations',
        { method: 'POST', body: { botId } },
      );
      dispatch({ type: 'snapshot', data: result.state });
      if (selectCreated({ botId, conversationId: result.conversation.id }, version))
        dispatch({ type: 'overlay', overlay: null });
    });
  };
  const send = async () => {
    const before = current.current;
    const selected = selectedBot(before);
    const text = (before.drafts[draftKey(before.selection)] ?? '').trim();
    const files = pendingRef.current[draftKey(before.selection)] ?? [];
    if (
      !selected ||
      (!text && !files.length) ||
      recipientDelivery(before.data, before.selection.conversationId) ||
      queuedRoutineRun(before.data, selectedConversation(before)) ||
      commandLock.current
    )
      return;
    if (before.connection !== 'online') {
      dispatch({ type: 'error', error: '서버에 다시 연결한 뒤 메시지를 보내 주세요.' });
      return;
    }
    if (!before.data?.settings.baseUrl || !before.data.settings.model) {
      settings('models');
      return;
    }
    if (!before.data.runtime.available) {
      dispatch({
        type: 'error',
        error:
          before.data.runtime.error ||
          '앱 엔진을 사용할 수 없습니다. 서버 로그를 확인한 뒤 grok-bot을 다시 시작해 주세요.',
      });
      return;
    }
    const originalDraft = before.drafts[draftKey(before.selection)] ?? '';
    const origin = before.selection;
    const version = navigationVersion.current;
    await command('message', async () => {
      let conversationId = origin.conversationId;
      let sentDraftKey = draftKey(origin);
      if (!conversationId) {
        const created = await request<{ conversation: Conversation; state: AppState }>(
          '/api/conversations',
          { method: 'POST', body: { botId: selected.id } },
        );
        conversationId = created.conversation.id;
        dispatch({ type: 'bind-draft', from: sentDraftKey, to: conversationId });
        sentDraftKey = conversationId;
        dispatch({ type: 'snapshot', data: created.state });
        selectCreated({ botId: selected.id, conversationId }, version);
      }
      const snapshot = files.length
        ? await request<AppState>(
            `/api/conversations/${encodeURIComponent(conversationId)}/attachment-messages`,
            { method: 'POST', body: { text, attachments: await uploads(files) } },
          )
        : await request<AppState>(
            `/api/conversations/${encodeURIComponent(conversationId)}/messages`,
            { method: 'POST', body: { text } },
          );
      dispatch({ type: 'snapshot', data: snapshot });
      dispatch({ type: 'draft-sent', key: sentDraftKey, text: originalDraft });
      if (files.length) {
        const sent = new Set(files.map((file) => file.key));
        const originKey = draftKey(origin);
        setPendingFiles((all) => ({ ...all, [originKey]: (all[originKey] ?? []).filter((file) => !sent.has(file.key)) }));
        setFileErrors((all) => ({ ...all, [originKey]: [] }));
      }
    });
  };
  const cancel = async () => {
    const before = current.current;
    const active = selectedConversation(before);
    if (!active || !hasConversationWork(before.data, active)) return;
    await command('cancel', async () => {
      const snapshot = await request<AppState>(
        `/api/conversations/${encodeURIComponent(active.id)}/cancel`,
        { method: 'POST', body: {} },
      );
      dispatch({ type: 'snapshot', data: snapshot });
    });
  };
  const cancelQueued = async (conversationId: string, turnId: string) => {
    await command('cancel-queued', async () => {
      dispatch({ type: 'snapshot', data: await cancelQueuedTurn(conversationId, turnId) });
    });
  };
  const cancelReports = async (conversationId: string, deliveryIds: string[]) => {
    await command('cancel-reports', async () => {
      setReportNotice(null);
      const result = await request<{ state: AppState; cancelledDeliveryIds: string[] }>(
        `/api/conversations/${encodeURIComponent(conversationId)}/bot-reports/cancel`,
        { method: 'POST', body: { deliveryIds } },
      );
      dispatch({ type: 'snapshot', data: result.state });
      if (!result.cancelledDeliveryIds.length) setReportNotice({ conversationId, text: '이미 실행을 시작했거나 대기 중이 아니어서 취소한 보고가 없습니다. 답변 중인 보고는 작업 중단을 사용하세요.' });
    });
  };
  const permission = async (optionId: string) => {
    const selected = selectedConversation(current.current);
    if (!selected?.run.permission) return;
    await command('permission', async () => {
      const snapshot = await request<AppState>(
        `/api/conversations/${encodeURIComponent(selected.id)}/permissions/${encodeURIComponent(selected.run.permission!.id)}`,
        { method: 'POST', body: { optionId } },
      );
      dispatch({ type: 'snapshot', data: snapshot });
    });
  };
  const saveRoutine = async (id: string | null, input: RoutineInput) => {
    return command('routine-save', async () => {
      const previousIds = new Set(current.current.data?.routines.map((routine) => routine.id));
      const snapshot = await request<AppState>(
        id ? `/api/routines/${encodeURIComponent(id)}` : '/api/routines',
        { method: id ? 'PUT' : 'POST', body: input },
      );
      dispatch({ type: 'snapshot', data: snapshot });
      return snapshot.routines.find((routine) =>
        id ? routine.id === id : !previousIds.has(routine.id),
      );
    });
  };
  const toggleRoutine = async (routine: Routine) => {
    await command(`routine-toggle:${routine.id}`, async () => {
      const snapshot = await request<AppState>(`/api/routines/${encodeURIComponent(routine.id)}`, {
        method: 'PATCH',
        body: { enabled: !routine.enabled },
      });
      dispatch({ type: 'snapshot', data: snapshot });
    });
  };
  const deleteRoutine = async (routine: Routine) => {
    const deleted = await command(`routine-delete:${routine.id}`, async () => {
      const snapshot = await request<AppState>(`/api/routines/${encodeURIComponent(routine.id)}`, {
        method: 'DELETE',
        body: {},
      });
      dispatch({ type: 'snapshot', data: snapshot });
      return true;
    });
    return deleted === true;
  };
  const runRoutine = async (routine: Routine) => {
    await command(`routine-run:${routine.id}`, async () => {
      const result = await request<RoutineRunResult>(
        `/api/routines/${encodeURIComponent(routine.id)}/run`,
        { method: 'POST', body: {} },
      );
      dispatch({ type: 'snapshot', data: result.state });
    });
  };
  const saveSettings = async (input: SettingsInput) => {
    if (
      input.workspace !== current.current.data?.settings.workspace &&
      !confirmFileChange(
        '작업 폴더를 바꾸면 저장하지 않은 파일 변경 사항이 사라집니다. 계속할까요?',
      )
    )
      return false;
    const result = await command('settings', async () => {
      const snapshot = await request<AppState>('/api/settings', { method: 'PUT', body: input });
      dispatch({ type: 'snapshot', data: snapshot });
      return true;
    });
    return result === true;
  };
  const setLinkPreviews = async (enabled: boolean) => {
    if (commandLock.current || typeof current.current.data?.linkPreviewsEnabled !== 'boolean') return;
    // Privacy-first: hide and cancel immediately, even if saving the off preference fails.
    // An explicit successful enable is required to resume after that failure.
    if (!enabled) setLinkPreviewsPaused(true);
    const saved = await command('link-preview-settings', async () => {
      try {
        const snapshot = await request<AppState>('/api/link-preview/settings', { method: 'PUT', body: { enabled } });
        if (snapshot.linkPreviewsEnabled !== enabled) throw new Error('링크 미리보기 설정을 저장하지 못했습니다. 다시 시도해 주세요.');
        dispatch({ type: 'snapshot', data: snapshot });
        return true;
      } catch (error) {
        if (!enabled) throw new Error(`서버에 저장하지 못해 이 탭에서만 링크 미리보기를 꺼 두었습니다. ${errorText(error)}`);
        throw error;
      }
    });
    if (saved) setLinkPreviewsPaused(false);
  };
  const toggleComputer = (open: boolean) => {
    if (open) {
      if (!confirmCanvasChange()) return;
      setCanvasId(null);
    }
    if (!open && !confirmFileChange('파일 탐색기를 닫고 저장하지 않은 변경 사항을 버릴까요?'))
      return;
    dispatch({ type: 'computer', open });
    if (open) {
      dispatch({ type: 'refresh-files' });
      dispatch({ type: 'refresh-document' });
    }
  };
  const closeOverlay = () => dispatch({ type: 'overlay', overlay: null });
  const prepareSkillRequest = () => {
    const before = current.current;
    const key = draftKey(before.selection);
    const text = '이 봇을 위한 재사용 가능한 비공개 스킬을 만들어줘. 이름, 사용 조건과 전체 지침을 먼저 정리한 뒤 저장 승인을 요청해줘.';
    const existing = before.drafts[key] ?? '';
    if (!navigate({ selection: before.selection, surface: { kind: 'chat' } })) return;
    dispatch({ type: 'draft', key, text: existing.trim() ? `${existing}\n\n${text}` : text });
    requestAnimationFrame(() => document.querySelector<HTMLTextAreaElement>('.composer textarea')?.focus());
  };
  const botConversations = conversationsFor(data, bot?.id ?? null);
  useBotActivity({ data, selection: state.selection, chat: state.surface.kind === 'chat', onRead: markRead, onOpen: select });
  // Group chats and threads (Groups.tsx, Threads.tsx).
  const snapshot = (next: AppState) => dispatch({ type: 'snapshot', data: next });
  const surface = state.surface;
  const threadId = surface.kind === 'chat' || surface.kind === 'group' ? surface.thread : undefined;
  const openThread = (thread?: string) => {
    const now = current.current.surface;
    if (now.kind === 'chat') navigate({ selection: current.current.selection, surface: thread ? { kind: 'chat', thread } : { kind: 'chat' } });
    else if (now.kind === 'group') navigate({ selection: current.current.selection, surface: { kind: 'group', groupId: now.groupId, conversationId: now.conversationId, ...(thread ? { thread } : {}) } });
  };
  const openConversation = (item: Conversation) => {
    const parent = item.thread ? data?.conversations.find(entry => entry.id === item.thread!.parentConversationId) ?? item : item;
    const thread = item.thread ? { thread: item.id } : {};
    if (parent.groupId) navigate({ selection: current.current.selection, surface: { kind: 'group', groupId: parent.groupId, conversationId: parent.id, ...thread } });
    else navigate({ selection: { botId: parent.botId, conversationId: parent.id }, surface: { kind: 'chat', ...thread } });
  };
  const shownIds = [surface.kind === 'chat' ? conversation?.id : surface.kind === 'group' ? shownGroupConversation(data, surface)?.id : undefined, threadId];
  const canvasConversation = data?.conversations.find(item => item.id === canvasId);
  const canvasVisible = !!canvasConversation && !canvasChat;
  const closeCanvas = () => { if (confirmCanvasChange()) setCanvasId(null); };
  const viewCanvasChat = () => { setCanvasChat(true); requestAnimationFrame(() => document.querySelector<HTMLTextAreaElement>(canvasConversation?.thread ? '[aria-label="스레드 메시지"]' : '.composer textarea')?.focus()); };
  const toggleCanvas = async (target?: string) => {
    if (target && target === canvasId) {
      if (canvasChat) setCanvasChat(false); else closeCanvas();
      return;
    }
    if (!confirmSkillsNavigation() || !confirmCanvasChange() || !confirmFileChange()) return;
    dispatch({ type: 'computer', open: false });
    if (target) { setCanvasId(target); setCanvasChat(false); return; }
    if (!data || busy) return;
    const version = navigationVersion.current;
    const groupId = surface.kind === 'group' ? surface.groupId : undefined;
    const from = groupId ? `group:${groupId}:new` : draftKey(current.current.selection);
    await command('canvas-conversation', async () => {
      if (!groupId && !bot) return;
      const created = await request<{ conversation: Conversation; state: AppState }>(groupId ? `/api/groups/${encodeURIComponent(groupId)}/conversations` : '/api/conversations', { method: 'POST', body: groupId ? {} : { botId: bot!.id } });
      snapshot(created.state);
      if (version !== navigationVersion.current) return;
      const selected = groupId ? navigate({ selection: current.current.selection, surface: { kind: 'group', groupId, conversationId: created.conversation.id } }) : selectCreated({ botId: created.conversation.botId, conversationId: created.conversation.id }, version);
      if (selected) {
        const to = groupId ? `group:${groupId}:${created.conversation.id}` : created.conversation.id;
        dispatch({ type: 'bind-draft', from, to });
        setPendingFiles(all => ({ ...all, [to]: all[from] ?? [], [from]: [] }));
        setFileErrors(all => ({ ...all, [to]: all[from] ?? [], [from]: [] }));
        setCanvasId(created.conversation.id); setCanvasChat(false);
      }
    });
  };
  const canvasControl = (target?: string) => <CanvasButton open={!!target && target === canvasId && !canvasChat} onClick={() => void toggleCanvas(target)} disabled={!data || busy || !!data.botDeliveries?.some(delivery => delivery.recipientConversationId === target)} />;
  const prepareCanvasExport = (text: string) => {
    if (!canvasConversation) return;
    const key = canvasConversation.thread ? `thread:${canvasConversation.id}` : canvasConversation.groupId ? `group:${canvasConversation.groupId}:${canvasConversation.id}` : canvasConversation.id;
    const existing = current.current.drafts[key] ?? '';
    dispatch({ type: 'draft', key, text: existing.trim() ? `${existing}\n\n${text}` : text });
    viewCanvasChat();
  };
  const openGroupDialog = (group?: Group) => { dispatch({ type: 'error', error: null }); dispatch({ type: 'sidebar', open: false }); setGroupDialog({ group }); };


  return (
    <LinkPreviewProvider enabled={linkPreviewsEnabled}>
    <div className={`app-shell ${state.computerOpen && !canvasVisible && state.surface.kind !== 'bot-chat' && state.surface.kind !== 'group' ? 'computer-is-open' : ''} ${threadId ? 'thread-is-open' : ''} ${canvasVisible ? 'canvas-is-open' : ''} ${canvasVisible && canvasId === threadId ? 'canvas-for-thread' : ''} ${canvasChat ? 'canvas-show-chat' : ''}`}>
      {state.sidebarOpen && (
        <button
          type="button"
          className="sidebar-backdrop"
          aria-label="사이드바 닫기"
          onClick={() => dispatch({ type: 'sidebar', open: false })}
        />
      )}
      <Sidebar data={data} selection={state.selection} lastConversation={state.lastConversation}
        chat={state.surface.kind === 'chat'} search={state.search} open={state.sidebarOpen} mobile={state.viewport === 'mobile'} busy={busy}
        onSearch={value => dispatch({ type: 'search', value })} onSelect={select}
        onClose={() => dispatch({ type: 'sidebar', open: false })} onNew={() => openOverlay({ kind: 'bot' })}
        onSettings={() => settings()} onCapabilities={() => openOverlay({ kind: 'capabilities' })}
        onRoutines={() => { if (state.surface.kind === 'routines') dispatch({ type: 'sidebar', open: false }); else navigate({ selection: current.current.selection, surface: { kind: 'routines' } }); }}
        onPatch={(id, patch) => { void command('bot-layout', async () => { const snapshot = await request<AppState>(`/api/bots/${encodeURIComponent(id)}`, { method: 'PATCH', body: patch }); dispatch({ type: 'snapshot', data: snapshot }); }); }}
        menuExtras={(item, close) => <BotMenuExtras bot={item} busy={busy || isBotRunning(data, item.id)} close={close}
          onUnread={(target, unread) => void botState(target.id, 'unread', { unread })}
          onNotify={(target, enabled) => void setNotify(target, enabled)}
          onDelete={(target) => { dispatch({ type: 'error', error: null }); setDeleting(target); }}
          onChooseMain={(target) => { dispatch({ type: 'error', error: null }); dispatch({ type: 'sidebar', open: false }); setChoosingMain({ botId: target.id }); }} />}
        extra={<GroupList data={data} search={state.search} selectedId={surface.kind === 'group' ? surface.groupId : null} busy={busy}
          onOpen={group => navigate({ selection: current.current.selection, surface: { kind: 'group', groupId: group.id, conversationId: surface.kind === 'group' && surface.groupId === group.id ? surface.conversationId : null } })}
          onCreate={() => openGroupDialog()} onEdit={group => openGroupDialog(group)} />}
      />
      <main
        className="chat-stage reference-chat"
        aria-label={state.surface.kind === 'routines' ? '루틴' : '대화'}
        inert={state.viewport === 'mobile' && state.sidebarOpen}
      >
        <header className="chat-header" hidden={state.surface.kind === 'bot-chat' || state.surface.kind === 'group'}>
          <div className="chat-identity-wrap">
            <IconButton
              label="봇 목록 열기"
              className="mobile-only"
              onClick={() => dispatch({ type: 'sidebar', open: true })}
            >
              <Menu size={19} />
            </IconButton>
            {state.surface.kind === 'routines' ? (
              <span className="routines-header-title">
                <Repeat2 size={17} />루틴
              </span>
            ) : bot ? (
              <button
                type="button"
                className="chat-identity"
                aria-label="봇 편집"
                onClick={() => { if (!confirmCanvasChange() || (state.computerOpen && !confirmFileChange())) return; setCanvasId(null); setDetailsTab('details'); dispatch({ type: 'computer', open: true }); }}
              >
                <Avatar color={bot.color} seed={bot.id} shape={bot.shape} size={28} />
                <span>{bot.name}</span>
              </button>
            ) : (
              <span className="muted">Grok Bot</span>
            )}
          </div>
          <div className="chat-controls">
            {state.surface.kind === 'chat' && (
              <>
                <IconButton
                  label="대화 기록"
                  onClick={() => openOverlay({ kind: 'history' })}
                  disabled={!bot}
                >
                  <Clock3 size={17} />
                </IconButton>
                <IconButton
                  label="새 대화"
                  onClick={() => void newConversation()}
                  disabled={!bot || busy}
                >
                  <SquarePen size={17} />
                </IconButton>
                {canvasControl(conversation?.id)}
              </>
            )}
            <IconButton
              label={state.computerOpen ? '컴퓨터 닫기' : '컴퓨터 열기'}
              aria-expanded={state.computerOpen}
              className={state.computerOpen ? 'active' : ''}
              onClick={() => { if (!state.computerOpen) setDetailsTab('computer'); toggleComputer(!state.computerOpen); }}
              disabled={!data}
            >
              <Monitor size={17} />
            </IconButton>
          </div>
        </header>
        {state.connection !== 'online' && (
          <div
            className={`connection-banner ${state.connection === 'error' ? 'is-error' : ''}`}
            role="status"
          >
            <span>
              {state.connection === 'loading'
                ? '로컬 서버에 연결 중…'
                : state.connectionError || '다시 연결 중…'}
            </span>
            {state.connection !== 'loading' && (
              <button type="button" onClick={reconnect}>
                다시 연결
              </button>
            )}
          </div>
        )}
        {state.error && !state.overlay && (
          <div className="chat-error">
            <ErrorNotice onDismiss={() => dispatch({ type: 'error', error: null })}>
              {state.error}
            </ErrorNotice>
          </div>
        )}
        {reportNotice && state.surface.kind === 'chat' && reportNotice.conversationId === conversation?.id && (
          <div className="connection-banner" role="status" aria-label="보고 취소 결과">
            <span>{reportNotice.text}</span>
            <button type="button" onClick={() => setReportNotice(null)}>보고 취소 안내 닫기</button>
          </div>
        )}
        {data && <OtherRuns data={data} shownIds={shownIds} onOpen={openConversation} />}
        {data && state.surface.kind === 'routines' ? (
          <Routines
            key={navigationVersion.current}
            data={data}
            selectedBotId={state.selection.botId}
            busy={busy}
            command={state.command}
            online={state.connection === 'online'}
            onSave={saveRoutine}
            onToggle={toggleRoutine}
            onDelete={deleteRoutine}
            onRun={runRoutine}
            onConversation={select}
            onSettings={settings}
            onCreateBot={() => openOverlay({ kind: 'bot' })}
            onDraftStatus={reportRoutineDraft}
          />
        ) : data && state.surface.kind === 'bot-chat' ? (
          <BotChat data={data} exchangeId={state.surface.exchangeId} onClose={() => navigate({ selection: current.current.selection, surface: { kind: 'chat' } })} onExecution={(botId, conversationId) => select({ botId, conversationId })} />
        ) : data && state.surface.kind === 'group' ? (
          <GroupView data={data} surface={state.surface} busy={busy} online={state.connection === 'online'} run={command} onSnapshot={snapshot}
            onRoute={next => navigate({ selection: current.current.selection, surface: next })} onSidebar={() => dispatch({ type: 'sidebar', open: true })}
            onEdit={group => openGroupDialog(group)} onThread={(parent, thread) => navigate({ selection: current.current.selection, surface: { kind: 'group', groupId: parent.groupId ?? parent.botId, conversationId: parent.id, thread } })}
            draft={key => state.drafts[key] ?? ''} onDraft={(key, text) => dispatch({ type: 'draft', key, text })} onSettings={() => settings('models')} canvasControl={canvasControl(shownGroupConversation(data, state.surface)?.id)} />
        ) : data && bot ? (
          <>
            <Transcript
              bot={bot}
              conversation={conversation}
              data={data}
              messageExtras={message => conversation && <ThreadLinks data={data} conversation={conversation} message={message} busy={busy} run={command} onSnapshot={snapshot} onOpen={thread => openThread(thread)} />}
              onBotChat={exchangeId => navigate({ selection: current.current.selection, surface: { kind: 'bot-chat', exchangeId } })}
              onCancelReports={(conversationId, deliveryIds) => void cancelReports(conversationId, deliveryIds)}
              cancelReportsDisabled={busy || state.connection !== 'online'}
              onRoutines={() => navigate({ selection: current.current.selection, surface: { kind: 'routines' } })}
              queuedRoutine={queuedRun}
              modelConfigured={!!data.settings.baseUrl && !!data.settings.model}
              runtimeAvailable={data.runtime.available}
              runtimeError={data.runtime.error}
              busy={busy}
              onSettings={() => settings('models')}
              onRuntimeSettings={() => settings('general')}
              onPermission={(optionId) => void permission(optionId)}
            />
            <QueuedTurns data={data} conversationId={conversation?.id} disabled={busy || state.connection !== 'online'}
              onCancel={turnId => { if (conversation) void cancelQueued(conversation.id, turnId); }} />
            <Composer
              botName={bot.name}
              value={draft}
              onChange={(text) => dispatch({ type: 'draft', key: selectionKey, text })}
              onSend={() => void send()}
              onFiles={() => toggleComputer(true)}
              onCancel={() => void cancel()}
              running={!!conversation && activeRun(conversation.run.status)}
              pendingWork={ownWork}
              disabled={state.connection !== 'online' || !!queuedRun || !!recipientDelivery(data, conversation?.id)}
              placeholder={recipientDelivery(data, conversation?.id) ? '다른 Bot의 요청으로 열린 대화라 여기서는 보낼 수 없습니다' : undefined}
              readOnly={!!recipientDelivery(data, conversation?.id)}
              busy={busy}
              model={data.settings.model}
              attachments={{
                items: pendingFiles[selectionKey] ?? [],
                errors: fileErrors[selectionKey] ?? [],
                add: (files) => void addFiles(selectionKey, files),
                remove: (key) => setPendingFiles((all) => ({ ...all, [selectionKey]: (all[selectionKey] ?? []).filter((file) => file.key !== key) })),
                dismiss: () => setFileErrors((all) => ({ ...all, [selectionKey]: [] })),
              }}
            />
          </>
        ) : (
          <div className="main-empty">
            {!data ? (
              state.connection === 'loading' ? (
                <Spinner label="로컬 작업 공간 불러오는 중" />
              ) : (
                <>
                  <Monitor size={29} />
                  <h1>서버에 연결할 수 없습니다</h1>
                  <p>로컬 서버를 실행한 뒤 다시 연결해 주세요.</p>
                  <button type="button" className="small-button" onClick={reconnect}>
                    다시 연결
                  </button>
                </>
              )
            ) : (
              <>
                <Plus size={28} />
                <h1>첫 봇을 추가하세요</h1>
                <p>이름과 지침을 정하고 대화를 시작할 수 있습니다.</p>
                <button
                  type="button"
                  className="small-button"
                  onClick={() => openOverlay({ kind: 'bot' })}
                >
                  봇 추가
                </button>
              </>
            )}
          </div>
        )}
      </main>
      {data && threadId && (
        <ThreadPanel key={threadId} data={data} threadId={threadId} busy={busy} online={state.connection === 'online'} run={command} onSnapshot={snapshot}
          onClose={() => openThread()} onSettings={() => settings('models')} canvasControl={canvasControl(threadId)}
          draft={state.drafts[`thread:${threadId}`] ?? ''} onDraft={text => dispatch({ type: 'draft', key: `thread:${threadId}`, text })} />
      )}
      {canvasConversation && <CanvasPanel key={`canvas:${canvasConversation.id}`} conversation={canvasConversation} active={canvasVisible} online={state.connection === 'online'} onSnapshot={snapshot} onClose={closeCanvas} onViewChat={viewCanvasChat} onDraftStatus={reportCanvasDraft} onExportRequest={prepareCanvasExport} />}
      {state.computerOpen && !canvasVisible && data && bot && state.surface.kind !== 'bot-chat' && state.surface.kind !== 'group' && (
        <Details bot={bot} conversation={conversation} data={data} files={state.files} dispatch={dispatch}
          tab={detailsTab} onTab={setDetailsTab} onClose={() => toggleComputer(false)}
          computer={<Computer ownerId={bot.id} canControl={!globalRun} />}
          onEdit={() => openOverlay({ kind: 'bot', bot })} onSettings={() => settings('files')}
          onBotCreated={(botId) => { select({ botId, conversationId: null }); }}
          onRoutines={() => { toggleComputer(false); navigate({ selection: current.current.selection, surface: { kind: 'routines' } }); }} />
      )}
      {state.overlay?.kind === 'settings' && data && (
        <Settings
          data={data}
          tab={state.overlay.tab}
          busy={busy}
          error={state.error}
          onTab={(tab) => dispatch({ type: 'overlay', overlay: { kind: 'settings', tab } })}
          onClose={closeOverlay}
          onSave={saveSettings}
          onFiles={() => toggleComputer(true)}
          linkPreviewsEnabled={linkPreviewsEnabled}
          linkPreviewsSaving={state.command === 'link-preview-settings'}
          onLinkPreviewsChange={setLinkPreviews}
        />
      )}
      {state.overlay?.kind === 'bot' && (
        <BotDialog
          bot={state.overlay.bot}
          data={data}
          busy={busy}
          error={state.error}
          onClose={closeOverlay}
          onSave={async (input) => {
            const editing = current.current.overlay;
            const editId = editing?.kind === 'bot' ? editing.bot?.id : undefined;
            if (!editId && !confirmNavigation()) return false;
            const version = navigationVersion.current;
            const originalBotIds = new Set(current.current.data?.bots.map((item) => item.id));
            const saved = await command('bot', async () => {
              const snapshot = await request<AppState>(
                editId ? `/api/bots/${encodeURIComponent(editId)}` : '/api/bots',
                { method: editId ? 'PATCH' : 'POST', body: input },
              );
              dispatch({ type: 'snapshot', data: snapshot });
              if (!editId) {
                const created = snapshot.bots.find((item) => !originalBotIds.has(item.id));
                if (created) selectCreated({ botId: created.id, conversationId: null }, version);
              }
              return true;
            });
            return saved === true;
          }}
        />
      )}
      {deleting && data && (
        <BotDeleteDialog bot={data.bots.find((item) => item.id === deleting.id) ?? deleting} data={data}
          busy={state.command === 'bot-delete'} error={state.error}
          onCancel={() => { setDeleting(null); dispatch({ type: 'error', error: null }); }}
          onConfirm={() => void deleteBot(deleting)} />
      )}
      {choosingMain && data && (
        <MainBotDialog data={data} initial={choosingMain.botId} busy={state.command === 'main-bot'} running={!!globalRun} error={state.error} run={command}
          onSnapshot={(snapshot) => dispatch({ type: 'snapshot', data: snapshot })} onClose={() => { setChoosingMain(null); dispatch({ type: 'error', error: null }); }} />
      )}
      {state.overlay?.kind === 'history' && bot && (
        <History
          bot={bot}
          conversations={botConversations}
          requestFrom={(item) => { const delivery = recipientDelivery(data, item.id); return delivery && (data?.bots.find((entry) => entry.id === delivery.fromBotId)?.name ?? '삭제된 Bot'); }}
          selectedId={state.selection.conversationId}
          busy={busy}
          onClose={closeOverlay}
          onNew={() => void newConversation()}
          onSelect={(id) => {
            select({ botId: bot.id, conversationId: id });
          }}
        />
      )}
      {groupDialog && data && (
        <GroupDialog data={data} group={groupDialog.group} busy={busy} error={state.error} run={command} onSnapshot={snapshot} onClose={() => setGroupDialog(null)}
          onCreated={group => navigate({ selection: current.current.selection, surface: { kind: 'group', groupId: group.id, conversationId: null } })}
          onDeleted={group => { const now = current.current.surface; if (now.kind === 'group' && now.groupId === group.id) navigate({ selection: current.current.selection, surface: { kind: 'chat' } }); }} />
      )}
      {state.overlay?.kind === 'capabilities' && data && bot && (
        <Skills key={bot.id} bot={bot} data={data} busy={busy} onClose={closeOverlay}
          onChange={snapshot => dispatch({ type: 'snapshot', data: snapshot })}
          onCreateWithBot={prepareSkillRequest} />
      )}
    </div>
    </LinkPreviewProvider>
  );
}
