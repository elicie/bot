import { useLayoutEffect, useRef, useState } from 'react';
import { Clock3, Pause, Pencil, Play, Plus, Repeat2, Trash2 } from 'lucide-react';
import type { AppState, Routine, RoutineInput, RoutineRun, RoutineSchedule } from '../shared/types';
import { activeRun, mainBotId, runLabels, type Selection } from './state';
import { Avatar, ErrorNotice } from './ui';
import { RoutineWebhook } from './RoutineWebhook';
import './routines.css';

type DraftSchedule =
  | { kind: 'once'; at: string }
  | { kind: 'interval'; minutes: string }
  | { kind: 'daily'; time: string; timezone: string };
interface RoutineDraft {
  name: string;
  botId: string;
  prompt: string;
  enabled: boolean;
  schedule: DraftSchedule;
}
type RoutineView =
  | { kind: 'overview' }
  | { kind: 'detail'; id: string }
  | { kind: 'editor'; id: string | null; initial: RoutineDraft; draft: RoutineDraft };
export interface RoutineDraftStatus {
  dirty: boolean;
  saving: boolean;
}

function localTimezone() {
  return Intl.DateTimeFormat().resolvedOptions().timeZone || 'Asia/Seoul';
}
function localDatetime(date: Date) {
  const pad = (value: number) => String(value).padStart(2, '0');
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`;
}
function draftSchedule(schedule?: RoutineSchedule): DraftSchedule {
  if (schedule?.kind === 'daily') return { ...schedule };
  if (schedule?.kind === 'interval') return { kind: 'interval', minutes: String(schedule.minutes) };
  return {
    kind: 'once',
    at: localDatetime(schedule ? new Date(schedule.at) : new Date(Date.now() + 3_600_000)),
  };
}
function routineDraft(botId: string, routine?: Routine): RoutineDraft {
  return {
    name: routine?.name ?? '',
    botId: routine?.botId ?? botId,
    prompt: routine?.prompt ?? '',
    enabled: routine?.enabled ?? true,
    schedule: draftSchedule(routine?.schedule),
  };
}
function routineInput(draft: RoutineDraft, data: AppState): RoutineInput {
  if (!draft.name.trim()) throw new Error('루틴 이름을 입력해 주세요.');
  if (!data.bots.some((bot) => bot.id === draft.botId)) throw new Error('실행할 봇을 선택해 주세요.');
  if (!draft.prompt.trim()) throw new Error('실행할 지시를 입력해 주세요.');
  let schedule: RoutineSchedule;
  if (draft.schedule.kind === 'once') {
    const date = new Date(draft.schedule.at);
    if (
      !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(draft.schedule.at) ||
      !Number.isFinite(date.getTime()) ||
      localDatetime(date) !== draft.schedule.at
    )
      throw new Error('실행 시각이 올바르지 않습니다. 실제 날짜와 시각을 입력해 주세요.');
    if (date.getTime() <= Date.now()) throw new Error('실행 시각은 현재보다 미래로 지정해 주세요.');
    schedule = { kind: 'once', at: date.toISOString() };
  } else if (draft.schedule.kind === 'interval') {
    const minutes = Number(draft.schedule.minutes);
    if (!Number.isInteger(minutes) || minutes < 1 || minutes > 10080)
      throw new Error('반복 간격은 1~10080 사이의 정수로 입력해 주세요.');
    schedule = { kind: 'interval', minutes };
  } else {
    if (!/^([01]\d|2[0-3]):[0-5]\d$/.test(draft.schedule.time))
      throw new Error('실행 시각을 시와 분으로 입력해 주세요.');
    const timezone = draft.schedule.timezone.trim();
    try {
      if (!timezone || /^[+-]/.test(timezone)) throw new Error();
      new Intl.DateTimeFormat(undefined, { timeZone: timezone }).format();
    } catch {
      throw new Error('시간대가 올바르지 않습니다. Asia/Seoul 같은 IANA 시간대를 입력해 주세요.');
    }
    schedule = { kind: 'daily', time: draft.schedule.time, timezone };
  }
  return {
    name: draft.name.trim(),
    botId: draft.botId,
    prompt: draft.prompt.trim(),
    enabled: draft.enabled,
    schedule,
  };
}
function formatTime(value: string, timezone = localTimezone()) {
  return `${new Date(value).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short', timeZone: timezone })} (${timezone})`;
}
function scheduleLabel(schedule: RoutineSchedule) {
  if (schedule.kind === 'interval') return `${schedule.minutes}분마다`;
  if (schedule.kind === 'daily') return `매일 ${schedule.time} (${schedule.timezone})`;
  return `한 번 · ${formatTime(schedule.at)}`;
}
function pendingRun(data: AppState, routineId: string) {
  return data.routineRuns.find((run) => {
    if (run.routineId !== routineId || run.result) return false;
    const conversation = data.conversations.find((item) => item.id === run.conversationId);
    return (
      conversation &&
      (!run.executionRunId || run.executionRunId === conversation.run.id) &&
      (conversation.run.status === 'idle' || activeRun(conversation.run.status))
    );
  });
}
function routineStatus(data: AppState, routine: Routine) {
  const pending = pendingRun(data, routine.id);
  const conversation = data.conversations.find((item) => item.id === pending?.conversationId);
  if (conversation) return conversation.run.status === 'idle' ? '대기 중' : runLabels[conversation.run.status];
  return routine.enabled ? '예약됨' : '일시정지';
}

function RunHistory({
  data,
  runs,
  onConversation,
}: {
  data: AppState;
  runs: RoutineRun[];
  onConversation: (selection: Selection) => void;
}) {
  return (
    <section className="routine-history" aria-label="루틴 실행 기록">
      <h2>
        <Clock3 size={15} />실행 기록 <span>{runs.length}</span>
      </h2>
      {runs.length === 0 ? (
        <p className="routine-history-empty">아직 실행 기록이 없습니다. 지금 실행하거나 예약 시각을 기다려 주세요.</p>
      ) : (
        <div className="routine-history-list">
          {runs.map((run) => {
            const conversation = data.conversations.find((item) => item.id === run.conversationId);
            const status = run.result?.status ?? conversation?.run.status;
            const bot = data.bots.find((item) => item.id === run.botId);
            const deleted = !data.routines.some((routine) => routine.id === run.routineId);
            const error = run.result?.error ?? (!run.result ? conversation?.run.error : undefined);
            return (
              <article className="routine-history-row" key={run.id}>
                <div className="routine-history-copy">
                  <div className="routine-history-title">
                    <strong>{run.routineName}</strong>
                    <span className={`routine-status ${status === 'awaiting_permission' ? 'needs-approval' : ''}`}>
                      {status === 'idle' ? '대기 중' : status ? runLabels[status] : '대화 없음'}
                    </span>
                  </div>
                  <span>
                    {bot?.name ?? '삭제된 봇'} · {run.trigger === 'manual' ? '수동 실행' : run.trigger === 'webhook' ? '웹훅 실행' : '예약 실행'}
                    {deleted ? ' · 삭제된 루틴' : ''}
                  </span>
                  <time dateTime={run.createdAt}>{formatTime(run.createdAt)}</time>
                  {status === 'idle' && <p>다른 작업이 끝나면 실행합니다.</p>}
                  {status === 'awaiting_permission' && <p>대화에서 파일 작업 승인 요청을 확인해 주세요.</p>}
                  {error && <p className="danger-text">{error}</p>}
                </div>
                <button
                  type="button"
                  className="small-button"
                  disabled={!conversation || !data.bots.some((item) => item.id === conversation.botId)}
                  onClick={() => conversation && onConversation({
                    botId: conversation.botId, conversationId: conversation.id,
                  })}
                >
                  대화 보기
                </button>
              </article>
            );
          })}
        </div>
      )}
    </section>
  );
}

export function Routines({
  data,
  selectedBotId,
  busy,
  command,
  online,
  onSave,
  onToggle,
  onDelete,
  onRun,
  onConversation,
  onSettings,
  onCreateBot,
  onDraftStatus,
}: {
  data: AppState;
  selectedBotId: string | null;
  busy: boolean;
  command: string | null;
  online: boolean;
  onSave: (id: string | null, input: RoutineInput) => Promise<Routine | undefined>;
  onToggle: (routine: Routine) => Promise<void>;
  onDelete: (routine: Routine) => Promise<boolean>;
  onRun: (routine: Routine) => Promise<void>;
  onConversation: (selection: Selection) => void;
  onSettings: (tab: 'models' | 'general') => void;
  onCreateBot: () => void;
  onDraftStatus: (status: RoutineDraftStatus) => void;
}) {
  const [view, setView] = useState<RoutineView>(() =>
    data.routines[0] ? { kind: 'detail', id: data.routines[0].id } : { kind: 'overview' },
  );
  const [formError, setFormError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const nameInput = useRef<HTMLInputElement>(null);
  const formErrorRef = useRef<HTMLDivElement>(null);
  const dirty = view.kind === 'editor' && JSON.stringify(view.draft) !== JSON.stringify(view.initial);
  const routine = view.kind === 'overview' ? undefined : data.routines.find((item) => item.id === view.id);
  const selectedId = view.kind === 'overview' ? null : view.id;
  const disabled = busy || !online;
  const configured = !!data.settings.baseUrl && !!data.settings.model;
  const canRun = configured && data.runtime.available;
  const pending = routine ? pendingRun(data, routine.id) : undefined;
  const pendingConversation = data.conversations.find((item) => item.id === pending?.conversationId);
  const runs = data.routineRuns
    .filter((run) => view.kind === 'overview' || (view.kind === 'detail' && run.routineId === view.id))
    .sort((a, b) => b.createdAt.localeCompare(a.createdAt));

  useLayoutEffect(() => {
    onDraftStatus({ dirty, saving });
    return () => onDraftStatus({ dirty: false, saving: false });
  }, [dirty, saving, onDraftStatus]);
  useLayoutEffect(() => {
    if (view.kind === 'editor') nameInput.current?.focus();
  }, [view.kind, selectedId]);
  useLayoutEffect(() => {
    if (formError) formErrorRef.current?.scrollIntoView({ block: 'nearest' });
  }, [formError]);

  const changeView = (next: RoutineView) => {
    if (saving || (dirty && !window.confirm('저장하지 않은 루틴 변경 사항을 버리고 이동할까요?'))) return;
    setFormError(null);
    setView(next);
  };
  const edit = (item?: Routine) => {
    const draft = routineDraft(selectedBotId ?? mainBotId(data) ?? '', item);
    changeView({ kind: 'editor', id: item?.id ?? null, initial: draft, draft });
  };
  const updateDraft = (change: Partial<RoutineDraft>) => {
    setView((before) => before.kind === 'editor'
      ? { ...before, draft: { ...before.draft, ...change } }
      : before,
    );
    setFormError(null);
  };
  const changeSchedule = (kind: DraftSchedule['kind']) => {
    const schedule: DraftSchedule = kind === 'daily'
      ? { kind, time: '09:00', timezone: localTimezone() }
      : kind === 'interval' ? { kind, minutes: '60' } : draftSchedule();
    updateDraft({ schedule });
  };

  return (
    <div className="routines-screen">
      <header className="routines-toolbar">
        <div>
          <h1>루틴</h1>
          <p>정해진 일정에 봇이 새 대화에서 지시를 실행합니다.</p>
        </div>
        <button
          type="button"
          className="primary-button"
          onClick={() => edit()}
          disabled={disabled || !data.bots.length || saving}
        >
          <Plus size={14} />루틴 만들기
        </button>
      </header>
      <p className="routines-safety">
        예약 실행에는 로컬 서버가 켜져 있어야 합니다. 파일 쓰기는 권한 규칙에 따라 대화에서 승인합니다.
      </p>
      {!canRun && (
        <div className="routine-setup" role="status">
          <span>
            {!configured
              ? '실행하려면 OpenAI 호환 모델을 먼저 설정해 주세요.'
              : data.runtime.error || '앱 엔진을 사용할 수 없습니다. 서버 컴퓨터의 앱 폴더에서 npm run build:engine 명령으로 엔진을 빌드한 뒤 서버를 다시 시작해 주세요.'}
          </span>
          <button type="button" className="text-button" onClick={() => onSettings(!configured ? 'models' : 'general')}>
            {!configured ? '모델 설정 열기' : '실행 환경 설정'}
          </button>
        </div>
      )}
      {data.routineSchedulingError && (
        <ErrorNotice>
          새 예약 실행을 대기 중입니다. {data.routineSchedulingError}
        </ErrorNotice>
      )}
      <div className="routines-layout">
        <nav className="routine-list" aria-label="루틴 목록">
          <button
            type="button"
            className={`routine-overview-button ${view.kind === 'overview' ? 'selected' : ''}`}
            aria-current={view.kind === 'overview' ? 'page' : undefined}
            onClick={() => changeView({ kind: 'overview' })}
          >
            전체 실행 기록 <span>{data.routineRuns.length}</span>
          </button>
          {data.routines.map((item) => {
            const bot = data.bots.find((entry) => entry.id === item.botId);
            const status = routineStatus(data, item);
            return (
              <button
                type="button"
                className={`routine-list-row ${selectedId === item.id ? 'selected' : ''}`}
                key={item.id}
                aria-current={selectedId === item.id ? 'true' : undefined}
                onClick={() => selectedId !== item.id && changeView({ kind: 'detail', id: item.id })}
              >
                <Repeat2 size={15} />
                <span className="routine-list-copy">
                  <strong>{item.name}</strong>
                  <small>{bot?.name ?? '삭제된 봇'} · {status}</small>
                  <small>{scheduleLabel(item.schedule)}</small>
                </span>
              </button>
            );
          })}
          {data.routines.length === 0 && <p className="routine-list-empty">등록된 루틴이 없습니다.</p>}
        </nav>
        <div className="routine-content">
          {view.kind === 'editor' ? (
            <form
              className="routine-form"
              aria-label={view.id ? '루틴 편집' : '루틴 만들기'}
              noValidate
              onSubmit={async (event) => {
                event.preventDefault();
                if (disabled || saving || pending) return;
                let input: RoutineInput;
                try {
                  input = routineInput(view.draft, data);
                } catch (error) {
                  setFormError(error instanceof Error ? error.message : '입력 내용을 확인해 주세요.');
                  return;
                }
                setFormError(null);
                setSaving(true);
                try {
                  const saved = await onSave(view.id, input);
                  if (saved) setView({ kind: 'detail', id: saved.id });
                } finally {
                  setSaving(false);
                }
              }}
            >
              <h2>{view.id ? '루틴 편집' : '새 루틴'}</h2>
              <fieldset disabled={disabled || saving}>
                <label className="settings-field">
                  <span>루틴 이름</span>
                  <input
                    aria-label="루틴 이름"
                    ref={nameInput}
                    value={view.draft.name}
                    onChange={(event) => updateDraft({ name: event.target.value })}
                    maxLength={100}
                    required
                    autoComplete="off"
                    placeholder="예: 아침 작업 요약"
                  />
                </label>
                <label className="settings-field">
                  <span>실행할 봇</span>
                  <select
                    aria-label="실행할 봇"
                    value={view.draft.botId}
                    onChange={(event) => updateDraft({ botId: event.target.value })}
                    required
                  >
                    <option value="" disabled>봇 선택</option>
                    {data.bots.map((bot) => <option value={bot.id} key={bot.id}>{bot.name}</option>)}
                  </select>
                </label>
                <label className="settings-field">
                  <span>실행할 지시</span>
                  <textarea
                    aria-label="실행할 지시"
                    rows={5}
                    value={view.draft.prompt}
                    onChange={(event) => updateDraft({ prompt: event.target.value })}
                    maxLength={100000}
                    required
                    placeholder="봇이 실행할 작업을 구체적으로 적어 주세요."
                  />
                </label>
                <div className="routine-schedule-fields">
                  <label className="settings-field">
                    <span>실행 일정</span>
                    <select
                      aria-label="실행 일정"
                      value={view.draft.schedule.kind}
                      onChange={(event) => changeSchedule(event.target.value as DraftSchedule['kind'])}
                    >
                      <option value="once">한 번</option>
                      <option value="interval">일정 간격</option>
                      <option value="daily">매일</option>
                    </select>
                  </label>
                  {view.draft.schedule.kind === 'once' && (
                    <label className="settings-field">
                      <span>실행 시각</span>
                      <input
                        aria-label="실행 시각"
                        type="datetime-local"
                        value={view.draft.schedule.at}
                        onChange={(event) => updateDraft({ schedule: { kind: 'once', at: event.target.value } })}
                        required
                      />
                      <small>이 브라우저의 시간대 · {localTimezone()}</small>
                    </label>
                  )}
                  {view.draft.schedule.kind === 'interval' && (
                    <label className="settings-field">
                      <span>반복 간격(분)</span>
                      <input
                        aria-label="반복 간격(분)"
                        type="number"
                        min={1}
                        max={10080}
                        step={1}
                        value={view.draft.schedule.minutes}
                        onChange={(event) => updateDraft({ schedule: { kind: 'interval', minutes: event.target.value } })}
                        required
                      />
                      <small>1~10080분. 다음 실행 시각은 서버가 계산합니다.</small>
                    </label>
                  )}
                  {view.draft.schedule.kind === 'daily' && (
                    <>
                      <label className="settings-field">
                        <span>실행 시각</span>
                        <input
                          aria-label="실행 시각"
                          type="time"
                          value={view.draft.schedule.time}
                          onChange={(event) => view.draft.schedule.kind === 'daily' && updateDraft({
                            schedule: { ...view.draft.schedule, time: event.target.value },
                          })}
                          required
                        />
                      </label>
                      <label className="settings-field routine-timezone">
                        <span>시간대</span>
                        <input
                          aria-label="시간대"
                          value={view.draft.schedule.timezone}
                          onChange={(event) => view.draft.schedule.kind === 'daily' && updateDraft({
                            schedule: { ...view.draft.schedule, timezone: event.target.value },
                          })}
                          placeholder="Asia/Seoul"
                          autoComplete="off"
                          spellCheck={false}
                          list="routine-timezones"
                          required
                        />
                        <datalist id="routine-timezones">
                          {[...new Set([localTimezone(), 'Asia/Seoul', 'UTC'])].map((timezone) => (
                            <option value={timezone} key={timezone} />
                          ))}
                        </datalist>
                        <small>IANA 시간대. 예: Asia/Seoul, America/New_York</small>
                      </label>
                    </>
                  )}
                </div>
                <label className="checkbox-label">
                  <input
                    type="checkbox"
                    checked={view.draft.enabled}
                    onChange={(event) => updateDraft({ enabled: event.target.checked })}
                  />
                  예약 실행 사용
                </label>
              </fieldset>
              {pending && (
                <p className="routine-form-note" role="status">
                  이 루틴이 대기 중이거나 실행 중입니다. 실행이 끝나면 편집 내용을 저장할 수 있습니다.
                </p>
              )}
              {formError && <div ref={formErrorRef}><ErrorNotice>{formError}</ErrorNotice></div>}
              {routine && <RoutineWebhook routine={routine} disabled={disabled || saving} />}
              <footer className="routine-form-actions">
                {dirty && <span>저장하지 않은 변경 사항</span>}
                <button
                  type="button"
                  className="small-button"
                  disabled={saving}
                  onClick={() => changeView(view.id ? { kind: 'detail', id: view.id } : { kind: 'overview' })}
                >
                  취소
                </button>
                <button
                  type="submit"
                  className="primary-button"
                  disabled={disabled || saving || !!pending || !dirty}
                >
                  {saving ? '저장 중…' : '루틴 저장'}
                </button>
              </footer>
            </form>
          ) : routine ? (
            <section className="routine-detail" aria-label="루틴 상세">
              <div className="routine-detail-heading">
                <h2>{routine.name}</h2>
                <span className={`routine-status ${pending ? 'is-active' : ''}`}>
                  {routineStatus(data, routine)}
                </span>
              </div>
              <div className="routine-detail-bot">
                <Avatar color={data.bots.find((bot) => bot.id === routine.botId)?.color} seed={routine.botId} size={22} />
                <span>{data.bots.find((bot) => bot.id === routine.botId)?.name ?? '삭제된 봇'}</span>
              </div>
              <p className="routine-prompt">{routine.prompt}</p>
              <dl className="routine-metadata">
                <div><dt>실행 일정</dt><dd>{scheduleLabel(routine.schedule)}</dd></div>
                <div>
                  <dt>다음 실행</dt>
                  <dd>
                    {routine.enabled && routine.nextRunAt ? (
                      <time dateTime={routine.nextRunAt}>
                        {formatTime(routine.nextRunAt, routine.schedule.kind === 'daily'
                          ? routine.schedule.timezone : localTimezone())}
                      </time>
                    ) : routine.enabled ? '예정된 실행 없음' : '일시정지'}
                  </dd>
                </div>
              </dl>
              <div className="routine-actions">
                <button
                  type="button"
                  className="primary-button"
                  aria-label="지금 실행"
                  disabled={disabled || !canRun || !!pending}
                  onClick={() => void onRun(routine)}
                >
                  <Play size={13} />
                  {command === `routine-run:${routine.id}` ? '요청 중…' : '지금 실행'}
                </button>
                <button
                  type="button"
                  className="small-button"
                  disabled={disabled || !!pending}
                  onClick={() => edit(routine)}
                >
                  <Pencil size={13} />루틴 편집
                </button>
                <button
                  type="button"
                  className="small-button"
                  disabled={disabled}
                  onClick={() => void onToggle(routine)}
                >
                  {routine.enabled ? <Pause size={13} /> : <Play size={13} />}
                  {routine.enabled ? '일시정지' : '다시 시작'}
                </button>
                <button
                  type="button"
                  className="small-button danger-text"
                  disabled={disabled || !!pending}
                  onClick={async () => {
                    if (!window.confirm(`'${routine.name}' 루틴을 삭제할까요? 실행 기록과 대화는 남습니다.`)) return;
                    if (await onDelete(routine)) {
                      setView((before) => before.kind === 'detail' && before.id === routine.id
                        ? { kind: 'overview' } : before);
                    }
                  }}
                >
                  <Trash2 size={13} />루틴 삭제
                </button>
              </div>
              <p className="routine-form-note">
                일시정지는 이후 예약 실행만 멈춥니다. 대기 중이거나 실행 중인 작업은 유지됩니다.
              </p>
              {pendingConversation && (
                <div className="routine-pending" role="status">
                  <span>
                    {pendingConversation.run.status === 'idle'
                      ? '대기 중입니다. 다른 작업이 끝나면 실행합니다.'
                      : pendingConversation.run.status === 'awaiting_permission'
                        ? '파일 작업 승인을 기다리고 있습니다.'
                        : '이 루틴이 실행 중입니다.'}
                  </span>
                  <button
                    type="button"
                    className="text-button"
                    onClick={() => onConversation({
                      botId: pendingConversation.botId, conversationId: pendingConversation.id,
                    })}
                  >
                    {pendingConversation.run.status === 'awaiting_permission' ? '승인 요청 보기' : '대화 보기'}
                  </button>
                </div>
              )}
              <RoutineWebhook routine={routine} disabled={disabled} />
              <RunHistory data={data} runs={runs} onConversation={onConversation} />
            </section>
          ) : view.kind === 'detail' ? (
            <div className="routine-empty">
              <h2>이 루틴은 삭제되었습니다</h2>
              <p>전체 실행 기록에서 이전 대화를 확인할 수 있습니다.</p>
              <button type="button" className="small-button" onClick={() => changeView({ kind: 'overview' })}>
                전체 실행 기록
              </button>
            </div>
          ) : (
            <>
              {data.routines.length === 0 && (
                <div className="routine-empty">
                  <Repeat2 size={24} />
                  <h2>반복할 작업을 루틴으로 만드세요</h2>
                  <p>봇과 지시, 일정을 정하면 실행 결과가 새 대화로 남습니다.</p>
                  {data.bots.length === 0 ? (
                    <button type="button" className="small-button" onClick={onCreateBot} disabled={disabled}>
                      먼저 봇 추가
                    </button>
                  ) : (
                    <button type="button" className="small-button" onClick={() => edit()} disabled={disabled}>
                      <Plus size={13} />루틴 만들기
                    </button>
                  )}
                </div>
              )}
              <RunHistory data={data} runs={runs} onConversation={onConversation} />
            </>
          )}
        </div>
      </div>
    </div>
  );
}
