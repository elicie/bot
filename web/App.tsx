import { useCallback, useEffect, useReducer, useRef } from 'react';
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
  Conversation,
  Routine,
  RoutineInput,
  RoutineRunResult,
  SettingsInput,
} from '../shared/types';
import { errorText, getState, request } from './api';
import { BotDialog, History } from './Bots';
import { Composer, Transcript } from './Conversation';
import { Files } from './Files';
import { Routines, type RoutineDraftStatus } from './Routines';
import { Capabilities, Settings } from './Settings';
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
  routeHash,
  type AppRoute,
  type Overlay,
  type Selection,
  type SettingsTab,
} from './state';
import { Avatar, ErrorNotice, IconButton, Spinner } from './ui';

function writeRoute(route: AppRoute, replace = false) {
  const hash = routeHash(route);
  if (window.location.hash === hash) return;
  window.history[replace ? 'replaceState' : 'pushState'](
    null,
    '',
    `${window.location.pathname}${window.location.search}${hash}`,
  );
}

export function App() {
  const [state, dispatch] = useReducer(reducer, undefined, initialState);
  const [connectionAttempt, reconnect] = useReducer((attempt: number) => attempt + 1, 0);
  const current = useRef(state);
  current.current = state;
  const commandLock = useRef(false);
  const navigationVersion = useRef(0);
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
    () => confirmRoutineChange() && confirmFileChange(),
    [confirmFileChange, confirmRoutineChange],
  );
  useEffect(() => {
    const hashChanged = () => {
      const next = readRoute();
      if (routeHash(next) === routeHash(current.current)) return;
      if (confirmNavigation()) {
        navigationVersion.current++;
        dispatch({ type: 'route', route: next });
        dispatch({ type: 'overlay', overlay: null });
      } else writeRoute(current.current, true);
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
    window.addEventListener('hashchange', hashChanged);
    window.addEventListener('beforeunload', beforeUnload);
    return () => {
      window.removeEventListener('hashchange', hashChanged);
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
  const openOverlay = (overlay: Overlay) => {
    dispatch({ type: 'error', error: null });
    dispatch({ type: 'overlay', overlay });
  };
  const settings = (tab: SettingsTab = 'general') => openOverlay({ kind: 'settings', tab });
  const navigate = (route: AppRoute) => {
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
    if (
      !selected ||
      !text ||
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
          'Grok Build CLI를 사용할 수 없습니다. 서버 컴퓨터의 설치 상태를 확인해 주세요.',
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
      const snapshot = await request<AppState>(
        `/api/conversations/${encodeURIComponent(conversationId)}/messages`,
        { method: 'POST', body: { text } },
      );
      dispatch({ type: 'snapshot', data: snapshot });
      dispatch({ type: 'draft-sent', key: sentDraftKey, text: originalDraft });
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
  const botConversations = conversationsFor(data, bot?.id ?? null);
  const filteredBots =
    data?.bots.filter((item) => {
      const preview = conversationPreview(conversationsFor(data, item.id)[0]);
      return `${item.name} ${preview}`
        .toLocaleLowerCase()
        .includes(state.search.toLocaleLowerCase());
    }) ?? [];

  return (
    <div className={`app-shell ${state.computerOpen ? 'computer-is-open' : ''}`}>
      {state.sidebarOpen && (
        <button
          type="button"
          className="sidebar-backdrop"
          aria-label="사이드바 닫기"
          onClick={() => dispatch({ type: 'sidebar', open: false })}
        />
      )}
      <aside
        className={`sidebar ${state.sidebarOpen ? 'is-open' : ''}`}
        aria-label="봇 목록"
        inert={state.viewport === 'mobile' && !state.sidebarOpen}
      >
        <header className="sidebar-header">
          <span className="app-title">
            Grok Bot <small>Local</small>
          </span>
          <div>
            <IconButton
              label="봇 추가"
              onClick={() => openOverlay({ kind: 'bot' })}
              disabled={!data || busy}
            >
              <Plus size={19} />
            </IconButton>
            <IconButton
              label="사이드바 닫기"
              className="mobile-only"
              onClick={() => dispatch({ type: 'sidebar', open: false })}
            >
              <X size={18} />
            </IconButton>
          </div>
        </header>
        <label className="search-box">
          <Search size={15} />
          <input
            aria-label="봇 검색"
            placeholder="검색"
            value={state.search}
            onChange={(event) => dispatch({ type: 'search', value: event.target.value })}
          />
          {state.search && (
            <IconButton label="검색 지우기" onClick={() => dispatch({ type: 'search', value: '' })}>
              <X size={13} />
            </IconButton>
          )}
        </label>
        <div className="bot-list">
          {data ? (
            filteredBots.map((item) => {
              const latest = conversationsFor(data, item.id)[0];
              const running = data.conversations.some(
                (entry) => entry.botId === item.id && activeRun(entry.run.status),
              );
              return (
                <button
                  type="button"
                  key={item.id}
                  className={`bot-row ${state.surface.kind === 'chat' && item.id === state.selection.botId ? 'selected' : ''}`}
                  aria-current={state.surface.kind === 'chat' && item.id === state.selection.botId ? 'true' : undefined}
                  onClick={() =>
                    select({
                      botId: item.id,
                      conversationId: state.selection.botId === item.id
                        ? state.selection.conversationId
                        : Object.hasOwn(state.lastConversation, item.id)
                          ? state.lastConversation[item.id]
                          : latest?.id ?? null,
                    })
                  }
                >
                  <span className="bot-avatar">
                    <Avatar color={item.color} seed={item.id} />
                    {running && <span className="running-dot" />}
                  </span>
                  <span className="bot-copy">
                    <span>{item.name}</span>
                    <small>{conversationPreview(latest)}</small>
                  </span>
                  {latest && (
                    <time dateTime={latest.updatedAt}>
                      {new Date(latest.updatedAt).toLocaleDateString('ko-KR', {
                        month: 'numeric',
                        day: 'numeric',
                      })}
                    </time>
                  )}
                </button>
              );
            })
          ) : (
            <div className="sidebar-loading">
              <Spinner />
            </div>
          )}
          {data && filteredBots.length === 0 && (
            <p className="sidebar-empty">
              {state.search ? '검색 결과가 없습니다.' : '봇을 추가해 대화를 시작하세요.'}
            </p>
          )}
        </div>
        <footer className="sidebar-footer">
          <button
            type="button"
            className={`footer-button ${state.surface.kind === 'routines' ? 'selected' : ''}`}
            aria-current={state.surface.kind === 'routines' ? 'page' : undefined}
            onClick={() => {
              if (state.surface.kind === 'routines') dispatch({ type: 'sidebar', open: false });
              else navigate({ selection: current.current.selection, surface: { kind: 'routines' } });
            }}
            disabled={!data}
          >
            <span className="footer-icon">
              <Repeat2 size={17} />
            </span>
            루틴
          </button>
          <button
            type="button"
            className="footer-button"
            onClick={() => openOverlay({ kind: 'capabilities' })}
          >
            <span className="footer-icon">
              <Plug size={17} />
            </span>
            Plugins
          </button>
          <button
            type="button"
            className="footer-button"
            onClick={() => settings()}
            disabled={!data}
          >
            <span className="footer-icon">
              <Settings2 size={17} />
            </span>
            설정
          </button>
          <button
            type="button"
            className="footer-button profile-button"
            onClick={() => settings()}
            disabled={!data}
          >
            <span className="profile-avatar">
              <UserRound size={18} />
            </span>
            <span>
              로컬 사용자<small>이 컴퓨터에서 실행</small>
            </span>
            <ChevronRight size={14} />
          </button>
        </footer>
      </aside>
      <main
        className="chat-stage"
        aria-label={state.surface.kind === 'routines' ? '루틴' : '대화'}
        inert={state.viewport === 'mobile' && state.sidebarOpen}
      >
        <header className="chat-header">
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
                onClick={() => openOverlay({ kind: 'bot', bot })}
              >
                <Avatar color={bot.color} seed={bot.id} size={25} />
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
              onClick={() => toggleComputer(!state.computerOpen)}
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
        ) : data && bot ? (
          <>
            <Transcript
              bot={bot}
              conversation={conversation}
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
      {state.computerOpen && data && (
        <Files
          workspace={data.settings.workspace}
          files={state.files}
          dispatch={dispatch}
          onClose={() => toggleComputer(false)}
          onSettings={() => settings('files')}
        />
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
      {state.overlay?.kind === 'capabilities' && <Capabilities onClose={closeOverlay} />}
    </div>
  );
}
