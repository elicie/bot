import { useEffect, useId, useRef, useState } from 'react';
import { Plus, ShieldAlert, ShieldCheck, Trash2 } from 'lucide-react';
import type {
  AppState,
  PermissionAction,
  PermissionCategory,
  PermissionRule,
  PermissionRuleInput,
  PermissionRuleList,
} from '../shared/types';
import { errorText, request } from './api';
import { ErrorNotice, IconButton } from './ui';
import './permissions.css';

export const MAX_PERMISSION_RULES = 100;

export const permissionCategories: { id: PermissionCategory; label: string; description: string }[] = [
  { id: 'file_write', label: '작업 폴더의 파일 만들기·바꾸기', description: '경로와 전체 내용을 검토할 수 있는 쓰기 제안' },
  { id: 'bot_message', label: '다른 Bot에게 메시지 보내기', description: '받는 Bot이 대화 전용으로 답하는 메시지' },
  { id: 'bot_create', label: '새 Bot 만들기', description: '이름·지침·모양을 정한 로컬 Bot' },
  { id: 'skill_save', label: 'Bot 스킬 저장', description: '이 Bot의 이후 대화에서 쓰는 비공개 스킬' },
  { id: 'memory_save', label: 'Bot 메모리 저장', description: 'Bot이 기억해 두는 내용' },
];
const actions: { id: PermissionAction; label: string; description: string }[] = [
  { id: 'ask', label: '항상 묻기', description: '작업이 이 규칙과 일치하면 멈추고 물어봐요' },
  { id: 'allow', label: '묻지 않고 허용', description: '작업이 이 규칙과 일치하면 자동으로 계속해요' },
];
export const categoryLabel = (category: PermissionCategory) =>
  permissionCategories.find((item) => item.id === category)?.label ?? category;

interface Draft {
  category: PermissionCategory;
  botId: string;
  action: PermissionAction;
}

/** Whether a change can make actions proceed without asking (loopback clients only). */
const loosens = (rule: PermissionRule | null, action: PermissionAction | 'delete') =>
  action === 'allow' || (action === 'delete' && rule?.action === 'ask');

export function PermissionRules({ data, disabled }: { data: AppState; disabled: boolean }) {
  const rules = data.permissionRules ?? [];
  const [canAutoAllow, setCanAutoAllow] = useState<boolean | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [pending, setPending] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState<string | null>(null);
  const lock = useRef(false);
  const prefix = useId();
  const categoryInput = useRef<HTMLSelectElement>(null);

  useEffect(() => {
    const controller = new AbortController();
    request<PermissionRuleList>('/api/permission-rules', { signal: controller.signal })
      .then((list) => setCanAutoAllow(list.canAutoAllow))
      .catch((reason) => {
        if (!controller.signal.aborted) setError(`권한을 불러올 수 없어요. ${errorText(reason)}`);
      });
    return () => controller.abort();
  }, []);
  const drafting = draft !== null;
  useEffect(() => {
    if (drafting) categoryInput.current?.focus();
  }, [drafting]);

  const botName = (botId?: string) =>
    botId ? data.bots.find((bot) => bot.id === botId)?.name ?? '삭제된 Bot' : '모든 Bot';
  const blocked = disabled || pending !== null;
  const mutate = async (key: string, path: string, method: 'POST' | 'PATCH' | 'DELETE', body: unknown, done: string) => {
    if (lock.current) return false;
    lock.current = true;
    setPending(key);
    setError(null);
    setStatus(null);
    try {
      await request<AppState>(path, { method, body });
      setStatus(done);
      return true;
    } catch (reason) {
      setError(`규칙을 저장하지 못했습니다. ${errorText(reason)}`);
      return false;
    } finally {
      lock.current = false;
      setPending(null);
    }
  };
  const save = async () => {
    if (!draft) return;
    const input: PermissionRuleInput = {
      category: draft.category,
      action: draft.action,
      ...(draft.botId ? { botId: draft.botId } : {}),
    };
    if (await mutate('add', '/api/permission-rules', 'POST', input, '규칙을 저장했어요.')) setDraft(null);
  };
  const startDraft = (action: PermissionAction) => {
    setError(null);
    setStatus(null);
    const used = new Set(rules.filter((rule) => !rule.botId).map((rule) => rule.category));
    const category = permissionCategories.find((item) => !used.has(item.id))?.id ?? 'file_write';
    setDraft({ category, botId: '', action });
  };
  const duplicate = draft && rules.some((rule) => rule.category === draft.category && (rule.botId ?? '') === draft.botId);
  const localOnlyBlocked = (rule: PermissionRule | null, action: PermissionAction | 'delete') =>
    canAutoAllow === false && loosens(rule, action);

  const list = (action: PermissionAction, heading: string, empty: string) => {
    const items = rules.filter((rule) => rule.action === action);
    return (
      <section className="settings-section" aria-label={heading}>
        <h3>{heading} <span className="permission-count">{items.length}</span></h3>
        <div className="settings-card permission-rule-list">
          {items.length === 0 && (
            <div className="settings-row"><span className="muted">{empty}</span></div>
          )}
          {items.map((rule) => (
            <div className="settings-row permission-rule" key={rule.id} role="group" aria-label={`${categoryLabel(rule.category)} · ${botName(rule.botId)}`}>
              <div className="settings-copy">
                <span>Grok Bot이 다음을 하려고 할 때:</span>
                <strong>{categoryLabel(rule.category)}</strong>
                <span>{botName(rule.botId)}</span>
              </div>
              <div className="permission-rule-actions">
                <select
                  aria-label="규칙의 동작 편집"
                  value={rule.action}
                  disabled={blocked}
                  onChange={(event) => {
                    const next = event.target.value as PermissionAction;
                    if (localOnlyBlocked(rule, next)) {
                      setError('묻지 않고 허용 규칙은 서버 컴퓨터의 localhost 연결에서만 추가하거나 완화할 수 있어요.');
                      return;
                    }
                    void mutate(`edit:${rule.id}`, `/api/permission-rules/${encodeURIComponent(rule.id)}`, 'PATCH', { action: next }, '규칙의 동작을 바꿨어요.');
                  }}
                >
                  {actions.map((item) => (
                    <option key={item.id} value={item.id} disabled={localOnlyBlocked(rule, item.id)}>{item.label}</option>
                  ))}
                </select>
                <IconButton
                  label="규칙 삭제"
                  disabled={blocked || localOnlyBlocked(rule, 'delete')}
                  onClick={() => {
                    if (!window.confirm(`'${categoryLabel(rule.category)} · ${botName(rule.botId)}' 규칙을 삭제할까요?`)) return;
                    void mutate(`delete:${rule.id}`, `/api/permission-rules/${encodeURIComponent(rule.id)}`, 'DELETE', {}, '규칙을 삭제했어요.');
                  }}
                >
                  <Trash2 size={14} />
                </IconButton>
              </div>
            </div>
          ))}
        </div>
        <button
          type="button"
          className="small-button permission-add"
          disabled={blocked || !!draft || rules.length >= MAX_PERMISSION_RULES || (action === 'allow' && canAutoAllow === false)}
          onClick={() => startDraft(action)}
        >
          <Plus size={13} />{heading} 추가
        </button>
      </section>
    );
  };

  return (
    <div className="permission-rules">
      <section className="settings-section">
        <h3>사용자 지정 규칙</h3>
        <div className="settings-card">
          <div className="settings-row">
            <div className="settings-copy">
              <strong><ShieldCheck size={15} /> Grok Bot이 위험할 수 있는 작업을 실행하기 전에 얼마나 꼼꼼히 확인할지 선택해요</strong>
              <span>
                {rules.length === 0
                  ? 'Grok Bot은 각 작업을 실행하기 전에 확인하고 필요할 때 먼저 물어봐요. 자동으로 실행할 수 있는 작업을 맞춤 설정하려면 규칙을 추가해 주세요.'
                  : 'Grok Bot이 자동으로 수행할 수 있는 작업에 대한 규칙을 설정해요. 규칙이 없는 작업은 항상 먼저 물어봐요.'}
              </span>
            </div>
            <span className="status-pill">규칙 최대 {MAX_PERMISSION_RULES}개</span>
          </div>
        </div>
        <p className="settings-note">
          이 규칙은 나에게만 적용돼요. 기본 제공 안전 검사는 항상 적용돼요. 규칙이 충돌하면 누가 작성했든 항상 묻기 규칙이 우선해요.
        </p>
        {canAutoAllow === false && (
          <p className="settings-note permission-lan-note" role="note">
            <ShieldAlert size={14} /> 이 기기는 서버 컴퓨터의 localhost 연결이 아니에요. 규칙을 보거나 더 엄격하게 바꿀 수는 있지만, 묻지 않고 허용 규칙을 추가하거나 완화하려면 서버 컴퓨터에서 열어 주세요.
          </p>
        )}
      </section>
      {draft && (
        <section className="settings-section">
          <h3>규칙 추가</h3>
          <div className="settings-card field-card permission-editor" role="group" aria-label="규칙 추가">
            <label className="settings-field">
              <span>Grok Bot이 다음을 하려고 할 때:</span>
              <select
                ref={categoryInput}
                aria-label="작업"
                value={draft.category}
                onChange={(event) => setDraft({ ...draft, category: event.target.value as PermissionCategory })}
                disabled={blocked}
              >
                {permissionCategories.map((item) => <option key={item.id} value={item.id}>{item.label}</option>)}
              </select>
              <small>{permissionCategories.find((item) => item.id === draft.category)?.description}</small>
            </label>
            <label className="settings-field">
              <span>적용할 Bot</span>
              <select
                aria-label="적용할 Bot"
                value={draft.botId}
                onChange={(event) => setDraft({ ...draft, botId: event.target.value })}
                disabled={blocked}
              >
                <option value="">모든 Bot</option>
                {data.bots.map((bot) => <option key={bot.id} value={bot.id}>{bot.name}</option>)}
              </select>
            </label>
            <fieldset className="settings-field permission-action-field" disabled={blocked}>
              <legend>규칙 동작</legend>
              {actions.map((item) => (
                <label key={item.id} className="permission-action-option">
                  <input
                    type="radio"
                    name={`${prefix}-action`}
                    value={item.id}
                    checked={draft.action === item.id}
                    disabled={localOnlyBlocked(null, item.id)}
                    onChange={() => setDraft({ ...draft, action: item.id })}
                  />
                  <span><strong>{item.label}</strong><small>{item.description}</small></span>
                </label>
              ))}
            </fieldset>
            {duplicate && (
              <p className="settings-note" role="status">같은 작업과 Bot에 대한 규칙이 이미 있어요. 목록에서 규칙의 동작을 편집해 주세요.</p>
            )}
            <div className="permission-editor-actions">
              <button type="button" className="small-button" disabled={pending !== null} onClick={() => { setDraft(null); setError(null); }}>
                규칙 수정 취소
              </button>
              <button type="button" className="primary-button" disabled={blocked || !!duplicate} onClick={() => void save()}>
                {pending === 'add' ? '저장 중…' : '규칙 저장'}
              </button>
            </div>
          </div>
        </section>
      )}
      {error && <ErrorNotice onDismiss={() => setError(null)}>{error}</ErrorNotice>}
      <span className="settings-copy-status" role="status">{status ?? ''}</span>
      {list('ask', '항상 묻기 규칙', '항상 묻기 규칙이 없어요. 규칙이 없는 작업은 기본으로 먼저 물어봐요.')}
      {list('allow', '묻지 않고 허용 규칙', '묻지 않고 허용 규칙이 없어요.')}
      <section className="settings-section">
        <h3>기본 제공 안전 검사</h3>
        <div className="settings-card">
          <div className="settings-row">
            <div className="settings-copy">
              <strong>관리형 브라우저</strong>
              <span>페이지 정보 공유와 브라우저 조작은 규칙과 관계없이 매번 서버의 localhost 연결에서 승인해요.</span>
            </div>
            <span className="status-pill success">항상 묻기</span>
          </div>
          <div className="settings-row">
            <div className="settings-copy">
              <strong>외부에서 시작한 대화</strong>
              <span>다른 Bot이 보낸 메시지로 시작한 대화와 웹훅으로 시작한 루틴 실행에서는 규칙을 적용하지 않고 먼저 물어봐요.</span>
            </div>
            <span className="status-pill success">항상 묻기</span>
          </div>
          <div className="settings-row">
            <div className="settings-copy">
              <strong>파일과 메시지 제한</strong>
              <span>자동으로 허용해도 파일 충돌 확인과 크기 제한, 작업 폴더 밖 접근 차단, Bot 메시지 4단계 제한은 그대로 적용돼요.</span>
            </div>
          </div>
          <div className="settings-row">
            <div className="settings-copy">
              <strong>자동 승인 기록</strong>
              <span>규칙으로 허용한 작업도 대화의 도구 활동에 “내 규칙으로 자동 승인됨”으로 표시돼요.</span>
            </div>
          </div>
        </div>
      </section>
    </div>
  );
}
