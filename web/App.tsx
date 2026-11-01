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
  Routine,
  RoutineInput,
  RoutineRunResult,
  SettingsInput,
} from '../shared/types';
import { errorText, getState, request } from './api';
import { BotDialog, History } from './Bots';
import { Composer, Transcript } from './Conversation';
import { BotChat } from './BotChat';
import { Sidebar } from './Sidebar';
import { Details, type DetailsTab } from './Details';
import { Computer } from './Computer';
import { Routines, type RoutineDraftStatus } from './Routines';
import { Settings } from './Settings';
import { Skills, confirmSkillsNavigation } from './Skills';
import {
  activeRun,
  conversationPreview,
  conversationsFor,
  draftKey,
  initialState,
  isDirty,
  readRoute,
  queuedRoutineRun,
  reducer,
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
import { notificationDeniedText, notificationUnavailableText, requestNotificationPermission, useBotActivity } from './activity';
import { admitFiles, uploads, type PendingAttachment } from './Attachments';

function historyPosition(): number {
  const position: unknown = window.history.state?.grokBotPosition;
  return typeof position === 'number' && Number.isSafeInteger(position) ? position : 0;
}

export function App() {
  const [state, dispatch] = useReducer(reducer, undefined, initialState);
  const [detailsTab, setDetailsTab] = useState<DetailsTab>('computer');
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
  const globalRun = runningConversation(state.data);
  const busy = state.command !== null;
  const data = state.data;
  const queuedRun = queuedRoutineRun(data, conversation);
  const selectionKey = draftKey(state.selection);
  const draft = state.drafts[selectionKey] ?? '';
  const [pendingFiles, setPendingFiles] = useState<Record<string, PendingAttachment[]>>({});
  const [fileErrors, setFileErrors] = useState<Record<string, string[]>>({});
  const pendingRef = useRef(pendingFiles);
  pendingRef.current = pendingFiles;
  const [deleting, setDeleting] = useState<Bot | null>(null);
  const reading = useRef(new Set<string>());

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
  const confirmNavigation = useCallback(
    () => confirmSkillsNavigation() && confirmRoutineChange() && confirmFileChange(),
    [confirmFileChange, confirmRoutineChange],
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
        routineDraft.current.saving
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
    navigationVersion.current++;
    writeRoute(route);
    dispatch({ type: 'route', route });
    dispatch({ type: 'overlay', overlay: null });
    dispatch({ type: 'error', error: null });
    return true;
  };
  const select = (selection: Selection) => navigate({ selection, surface: { kind: 'chat' } });
  const selectCreated = (selection: Selection, expectedVersion: number) => {
    if (navigationVersion.current !== expectedVersion) return false;
    navigationVersion.current++;
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
      runningConversation(before.data) ||
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
          '앱 엔진을 사용할 수 없습니다. 서버 컴퓨터의 앱 폴더에서 npm run build:engine 명령으로 엔진을 빌드한 뒤 서버를 다시 시작해 주세요.',
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
    if (!active || (!activeRun(active.run.status) && !queuedRoutineRun(before.data, active))) return;
    await command('cancel', async () => {
      const snapshot = await request<AppState>(
        `/api/conversations/${encodeURIComponent(active.id)}/cancel`,
        { method: 'POST', body: {} },
      );
      dispatch({ type: 'snapshot', data: snapshot });
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
  const toggleComputer = (open: boolean) => {
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


  return (
    <div className={`app-shell ${state.computerOpen && state.surface.kind !== 'bot-chat' ? 'computer-is-open' : ''}`}>
      {state.sidebarOpen && (
        <button
          type="button"
          className="sidebar-backdrop"
          aria-label="사이드바 닫기"
          onClick={() => dispatch({ type: 'sidebar', open: false })}
        />
      )}
      <Sidebar data={data} selection={state.selection} lastConversation={state.lastConversation}
        chat={state.surface.kind === 'chat'} search={state.search} open={state.sidebarOpen} mobile={state.viewport === 'mobile'} busy={busy || !!globalRun}
        onSearch={value => dispatch({ type: 'search', value })} onSelect={select}
        onClose={() => dispatch({ type: 'sidebar', open: false })} onNew={() => openOverlay({ kind: 'bot' })}
        onSettings={() => settings()} onCapabilities={() => openOverlay({ kind: 'capabilities' })}
        onRoutines={() => { if (state.surface.kind === 'routines') dispatch({ type: 'sidebar', open: false }); else navigate({ selection: current.current.selection, surface: { kind: 'routines' } }); }}
        onPatch={(id, patch) => { void command('bot-layout', async () => { const snapshot = await request<AppState>(`/api/bots/${encodeURIComponent(id)}`, { method: 'PATCH', body: patch }); dispatch({ type: 'snapshot', data: snapshot }); }); }}
        menuExtras={(item, close) => <BotMenuExtras bot={item} busy={busy || !!globalRun} close={close}
          onUnread={(target, unread) => void botState(target.id, 'unread', { unread })}
          onNotify={(target, enabled) => void setNotify(target, enabled)}
          onDelete={(target) => { dispatch({ type: 'error', error: null }); setDeleting(target); }} />}
      />
      <main
        className="chat-stage reference-chat"
        aria-label={state.surface.kind === 'routines' ? '루틴' : '대화'}
        inert={state.viewport === 'mobile' && state.sidebarOpen}
      >
        <header className="chat-header" hidden={state.surface.kind === 'bot-chat'}>
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
                onClick={() => { if (state.computerOpen && !confirmFileChange()) return; setDetailsTab('details'); dispatch({ type: 'computer', open: true }); }}
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
                  disabled={!bot || busy || !!globalRun}
                >
                  <SquarePen size={17} />
                </IconButton>
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
        {globalRun && (state.surface.kind === 'routines' || globalRun.id !== conversation?.id) && (
          <div className="other-run">
            <span>다른 대화에서 작업이 실행 중입니다.</span>
            <button
              type="button"
              onClick={() => select({ botId: globalRun.botId, conversationId: globalRun.id })}
            >
              실행 중인 대화 보기
            </button>
          </div>
        )}
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
        ) : data && bot ? (
          <>
            <Transcript
              bot={bot}
              conversation={conversation}
              data={data}
              onBotChat={exchangeId => navigate({ selection: current.current.selection, surface: { kind: 'bot-chat', exchangeId } })}
              queuedRoutine={queuedRun}
              modelConfigured={!!data.settings.baseUrl && !!data.settings.model}
              runtimeAvailable={data.runtime.available}
              runtimeError={data.runtime.error}
              busy={busy}
              onSettings={() => settings('models')}
              onRuntimeSettings={() => settings('general')}
              onPermission={(optionId) => void permission(optionId)}
            />
            <Composer
              botName={bot.name}
              value={draft}
              onChange={(text) => dispatch({ type: 'draft', key: selectionKey, text })}
              onSend={() => void send()}
              onFiles={() => toggleComputer(true)}
              onCancel={() => void cancel()}
              running={!!queuedRun || (!!conversation && activeRun(conversation.run.status))}
              disabled={state.connection !== 'online' || !!globalRun || !!queuedRun}
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
      {state.computerOpen && data && bot && state.surface.kind !== 'bot-chat' && (
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
        />
      )}
      {state.overlay?.kind === 'bot' && (
        <BotDialog
          bot={state.overlay.bot}
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
      {state.overlay?.kind === 'history' && bot && (
        <History
          bot={bot}
          conversations={botConversations}
          selectedId={state.selection.conversationId}
          busy={busy || !!globalRun}
          onClose={closeOverlay}
          onNew={() => void newConversation()}
          onSelect={(id) => {
            select({ botId: bot.id, conversationId: id });
          }}
        />
      )}
      {state.overlay?.kind === 'capabilities' && bot && (
        <Skills key={bot.id} bot={bot} busy={busy || !!globalRun} onClose={closeOverlay}
          onChange={snapshot => dispatch({ type: 'snapshot', data: snapshot })}
          onCreateWithBot={prepareSkillRequest} />
      )}
    </div>
  );
}
