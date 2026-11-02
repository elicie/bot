import { useEffect, useId, useRef, useState, type Dispatch, type FormEvent } from 'react';
import { createPortal } from 'react-dom';
import { KeyRound, Plug, Plus, RefreshCw, Search, ShieldAlert, Terminal, Trash2, X } from 'lucide-react';
import type { AppState, Bot } from '../shared/types';
import type { McpConfigDocument, PublicMcpServer, PublicSecret, SecretInput, VaultAccess } from '../shared/vault';
import { ApiError, errorText, request } from './api';
import { type Action, runningConversation } from './state';
import { ErrorNotice, IconButton, Modal } from './ui';
import { McpServerForm } from './McpServerForm';
import './vault.css';

const SECRET_NAME = /^[A-Z][A-Z0-9_]{0,63}$/;
const MIN_SECRET = 8;
const EXAMPLE = `{
  "mcpServers": {
    "example": {
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer \${secret:API_TOKEN}" }
    }
  }
}`;
const LAN_NOTE = '이 브라우저는 서버 컴퓨터의 localhost 연결이 아니에요. 이름만 볼 수 있고, 추가·교체·삭제와 MCP 서버 편집·켜기는 서버 컴퓨터에서 해 주세요.';

/** `canEdit` from the server (loopback clients only); `null` while loading. */
export function useVaultAccess() {
  const [canEdit, setCanEdit] = useState<boolean | null>(null);
  useEffect(() => {
    const controller = new AbortController();
    request<VaultAccess>('/api/vault', { signal: controller.signal })
      .then((access) => setCanEdit(access.canEdit))
      .catch(() => { if (!controller.signal.aborted) setCanEdit(false); });
    return () => controller.abort();
  }, []);
  return canEdit;
}

const scopeLabel = (data: AppState, secret: PublicSecret) =>
  secret.botId ? data.bots.find((bot) => bot.id === secret.botId)?.name ?? '삭제된 Bot' : '모든 Bot';

/** "새 시크릿 추가" / replace form. `replace` set: only the value changes. */
function SecretForm({ data, botId, replace, onClose, onSaved }: {
  data: AppState; botId?: string; replace?: PublicSecret; onClose: () => void; onSaved: (state: AppState, message: string) => void;
}) {
  const [name, setName] = useState(replace?.name ?? '');
  const [value, setValue] = useState('');
  const [scope, setScope] = useState(replace?.botId ?? botId ?? '');
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const prefix = useId();
  const trimmedName = name.trim();
  const nameProblem = !replace && trimmedName && !SECRET_NAME.test(trimmedName) ? '문자로 시작하는 대문자 환경 변수 이름(A-Z, 0-9, _)을 사용해 주세요' : null;
  const valueProblem = value && value.trim().length < MIN_SECRET ? `시크릿 값은 ${MIN_SECRET}자 이상이어야 해요.` : null;
  const save = async (event: FormEvent) => {
    event.preventDefault();
    // React propagates portal events through the component tree, which includes the settings form.
    event.stopPropagation();
    setPending(true);
    setError(null);
    try {
      if (replace) {
        const state = await request<AppState>(`/api/secrets/${encodeURIComponent(replace.id)}`, { method: 'PUT', body: { value } });
        onSaved(state, `${replace.name} 시크릿을 교체했어요.`);
      } else {
        const input: SecretInput = { name: trimmedName, value, ...(scope ? { botId: scope } : {}) };
        const state = await request<AppState>('/api/secrets', { method: 'POST', body: input });
        onSaved(state, `${trimmedName} 시크릿을 저장했어요.`);
      }
    } catch (failure) {
      setError(`시크릿이 저장되지 않았어요. ${errorText(failure)}`);
      setPending(false);
    }
  };
  const close = () => { if (!pending) onClose(); };
  const title = replace ? `${replace.name} 시크릿 교체` : '새 시크릿 추가';
  // Portaled: the settings dialog is itself a form, and forms must not nest.
  return createPortal(<Modal title={title} onClose={close} className="compact-modal vault-modal">
    <header className="modal-header"><h2><KeyRound size={16} />{title}</h2><IconButton label="시크릿 창 닫기" onClick={close} disabled={pending}><X size={18} /></IconButton></header>
    <form onSubmit={(event) => void save(event)}>
      <div className="modal-body vault-modal-body">
        <p className="vault-help">값은 쓰기 전용이에요. 저장한 뒤에는 이 앱의 어느 화면에서도 다시 보여 주지 않고, 대화와 기록에 나타나면 [redacted]로 가려요.</p>
        {!replace && <label className="vault-field"><span>환경 변수 이름</span>
          <input id={`${prefix}-name`} aria-label="환경 변수 이름" value={name} onChange={(event) => setName(event.target.value.toUpperCase())} placeholder="API_TOKEN" maxLength={64} spellCheck={false} autoComplete="off" disabled={pending} data-autofocus aria-invalid={!!nameProblem} />
          {nameProblem && <small className="vault-invalid">{nameProblem}</small>}
        </label>}
        <label className="vault-field"><span>시크릿 값</span>
          <input type="password" aria-label="시크릿 값" value={value} onChange={(event) => setValue(event.target.value)} placeholder="시크릿 붙여넣기" autoComplete="new-password" spellCheck={false} disabled={pending} data-autofocus={replace ? true : undefined} aria-invalid={!!valueProblem} />
          {valueProblem && <small className="vault-invalid">{valueProblem}</small>}
        </label>
        {!replace && !botId && <label className="vault-field"><span>사용할 Bot</span>
          <select aria-label="사용할 Bot" value={scope} onChange={(event) => setScope(event.target.value)} disabled={pending}>
            <option value="">모든 Bot</option>
            {data.bots.map((bot) => <option key={bot.id} value={bot.id}>{bot.name}</option>)}
          </select>
        </label>}
        {!replace && botId && <p className="vault-help">이 Bot에만 쓰이는 시크릿이에요. 같은 이름의 모든 Bot용 시크릿보다 우선해요.</p>}
        {error && <ErrorNotice>{error}</ErrorNotice>}
      </div>
      <footer className="modal-footer">
        <button type="button" className="small-button" onClick={close} disabled={pending}>취소</button>
        <button type="submit" className="primary-button" disabled={pending || !value.trim() || !!valueProblem || (!replace && (!trimmedName || !!nameProblem))}>{pending ? '저장 중…' : '시크릿 저장'}</button>
      </footer>
    </form>
  </Modal>, document.body);
}

function SecretRows({ data, secrets, canEdit, onReplace, onDelete, busy }: {
  data: AppState; secrets: PublicSecret[]; canEdit: boolean; busy: boolean;
  onReplace: (secret: PublicSecret) => void; onDelete: (secret: PublicSecret) => void;
}) {
  return <ul className="vault-list" aria-label="시크릿 목록">
    {secrets.map((secret) => <li key={secret.id} className="vault-row" aria-label={`${secret.name} · ${scopeLabel(data, secret)}`}>
      <KeyRound size={14} aria-hidden="true" />
      <span className="vault-row-copy"><code>{secret.name}</code><small>{scopeLabel(data, secret)}</small></span>
      <span className="vault-masked" aria-label="값 숨김">••••••••</span>
      <button type="button" className="text-button" disabled={!canEdit || busy} onClick={() => onReplace(secret)} aria-label={`${secret.name} 교체`}>교체</button>
      <IconButton label={`${secret.name} 삭제`} disabled={!canEdit || busy} onClick={() => onDelete(secret)}><Trash2 size={13} /></IconButton>
    </li>)}
  </ul>;
}

/** Shared secret list state: dialogs, deletion, feedback. */
function useSecretActions(onState?: (state: AppState) => void) {
  const [dialog, setDialog] = useState<'add' | { replace: PublicSecret } | null>(null);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState<string | null>(null);
  const remove = async (secret: PublicSecret) => {
    if (!window.confirm(`${secret.name} 시크릿을 삭제할까요? 이 시크릿을 참조하는 MCP 서버는 다음 대화부터 시작되지 않아요.`)) return;
    setPending(true); setError(null); setStatus(null);
    try {
      const state = await request<AppState>(`/api/secrets/${encodeURIComponent(secret.id)}`, { method: 'DELETE', body: {} });
      onState?.(state);
      setStatus(`${secret.name} 시크릿을 삭제했어요.`);
    } catch (failure) { setError(`시크릿을 삭제하지 못했어요. ${errorText(failure)}`); } finally { setPending(false); }
  };
  const saved = (state: AppState, message: string) => { onState?.(state); setDialog(null); setStatus(message); setError(null); };
  return { dialog, setDialog, pending, error, setError, status, remove, saved };
}

/** The "보관함" settings tab: every secret by name and scope. */
export function VaultSettings({ data }: { data: AppState }) {
  const canEdit = useVaultAccess();
  const [query, setQuery] = useState('');
  const actions = useSecretActions();
  const secrets = data.secrets ?? [];
  const needle = query.trim().toLowerCase();
  const shown = secrets.filter((secret) => !needle || secret.name.toLowerCase().includes(needle) || scopeLabel(data, secret).toLowerCase().includes(needle));
  return <section className="settings-section vault-settings" aria-label="Grok Bot 보관함">
    <h3>Grok Bot 보관함 <span className="permission-count">{secrets.length}</span></h3>
    <p className="settings-note">사용자 지정 MCP 서버의 env 또는 headers 값에서 <code>{'${secret:이름}'}</code>으로 참조하는 자격 증명이에요. 값은 쓰기 전용이며 세션을 시작할 때 서버에서만 채워지고 모델에는 전달되지 않아요.</p>
    {canEdit === false && <p className="settings-note vault-lan-note" role="note"><ShieldAlert size={14} aria-hidden="true" />{LAN_NOTE}</p>}
    <div className="vault-toolbar">
      <label className="vault-search"><Search size={14} aria-hidden="true" /><input type="search" aria-label="보관함 검색" placeholder="보관함 검색" value={query} onChange={(event) => setQuery(event.target.value)} onKeyDown={(event) => { if (event.key === 'Enter') event.preventDefault(); }} /></label>
      <button type="button" className="small-button" disabled={!canEdit || actions.pending} onClick={() => actions.setDialog('add')}><Plus size={13} />새 시크릿 추가</button>
    </div>
    <div className="settings-card">
      {shown.length ? <SecretRows data={data} secrets={shown} canEdit={!!canEdit} busy={actions.pending} onReplace={(secret) => actions.setDialog({ replace: secret })} onDelete={(secret) => void actions.remove(secret)} />
        : <div className="settings-row"><span className="muted">{secrets.length ? '검색과 일치하는 시크릿이 없어요' : '아직 시크릿이 없습니다'}</span></div>}
    </div>
    {actions.error && <ErrorNotice onDismiss={() => actions.setError(null)}>{actions.error}</ErrorNotice>}
    {actions.status && <p className="vault-status" role="status">{actions.status}</p>}
    <p className="settings-note">시크릿 값은 API 키처럼 서버 데이터 폴더의 state.json(소유자만 읽고 쓸 수 있는 0600 파일)에 저장돼요. 운영체제 키체인과 연동하거나 따로 암호화하지 않으니 서버 컴퓨터의 계정을 보호해 주세요.</p>
    {actions.dialog === 'add' && <SecretForm data={data} onClose={() => actions.setDialog(null)} onSaved={actions.saved} />}
    {actions.dialog && typeof actions.dialog === 'object' && <SecretForm data={data} replace={actions.dialog.replace} onClose={() => actions.setDialog(null)} onSaved={actions.saved} />}
  </section>;
}

function McpEditor({ bot, onClose, onSaved }: { bot: Bot; onClose: () => void; onSaved: (state: AppState) => void }) {
  const [text, setText] = useState<string | null>(null);
  const [original, setOriginal] = useState('');
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    const controller = new AbortController();
    request<McpConfigDocument>(`/api/bots/${encodeURIComponent(bot.id)}/mcp-servers`, { signal: controller.signal })
      .then((document) => { setText(document.json); setOriginal(document.json); })
      .catch((failure) => { if (!controller.signal.aborted) { setError(`MCP 서버 JSON을 불러올 수 없어요. ${errorText(failure)}`); } });
    return () => controller.abort();
  }, [bot.id]);
  const empty = text !== null && /^\s*\{\s*"mcpServers"\s*:\s*\{\s*\}\s*\}\s*$/.test(text);
  const save = async () => {
    if (text === null) return;
    try { JSON.parse(text); } catch { setError('구성이 올바른 JSON이 아니에요.'); return; }
    setPending(true); setError(null);
    try {
      onSaved(await request<AppState>(`/api/bots/${encodeURIComponent(bot.id)}/mcp-servers`, { method: 'PUT', body: { json: text } }));
    } catch (failure) { setError(errorText(failure)); setPending(false); }
  };
  const close = () => { if (!pending && (text === null || text === original || window.confirm('저장하지 않은 MCP 서버 JSON을 버리고 닫을까요?'))) onClose(); };
  return <Modal title="MCP 서버 JSON" onClose={close} className="compact-modal vault-modal vault-mcp-modal">
    <header className="modal-header"><h2><Plug size={16} />MCP 서버 JSON</h2><IconButton label="MCP 서버 JSON 닫기" onClick={close} disabled={pending}><X size={18} /></IconButton></header>
    <div className="modal-body vault-modal-body">
      <p className="vault-warning" role="note"><ShieldAlert size={15} aria-hidden="true" /><span><strong>명령(stdio) 서버는 서버 컴퓨터에서 이 앱과 같은 사용자 권한으로 실행돼요.</strong> 신뢰하는 명령만 추가하세요. 추가하거나 바꾼 서버는 꺼진 상태로 저장되며, 켠 뒤에도 도구 호출마다 승인을 요청해요.</span></p>
      <p className="vault-help">{bot.name}의 대화에서 쓸 서버를 <code>{'{"mcpServers": {"이름": {...}}}'}</code> 형식으로 적어 주세요. 명령 서버는 command·args·env, HTTP 서버는 url·headers를 써요. 토큰은 값에 직접 넣지 말고 보관함에 저장한 뒤 <code>{'${secret:API_TOKEN}'}</code>처럼 참조하세요.</p>
      <textarea aria-label="MCP 서버 JSON" rows={14} spellCheck={false} value={text ?? ''} onChange={(event) => setText(event.target.value)} disabled={pending || text === null} data-autofocus />
      {empty && <button type="button" className="small-button" onClick={() => setText(EXAMPLE)} disabled={pending}>예시 넣기</button>}
      {error && <ErrorNotice>{error}</ErrorNotice>}
    </div>
    <footer className="modal-footer">
      <button type="button" className="small-button" onClick={close} disabled={pending}>취소</button>
      <button type="button" className="primary-button" onClick={() => void save()} disabled={pending || text === null}>{pending ? '저장 중…' : '저장'}</button>
    </footer>
  </Modal>;
}

type McpOAuthStatus = {
  status: 'not_connected' | 'awaiting_login' | 'connected' | 'login_required' | 'error' | 'unavailable';
  canLogin: boolean;
};
const OAUTH_LABELS: Record<McpOAuthStatus['status'], string> = {
  not_connected: '로그인 안 됨', awaiting_login: '로그인 대기 중', connected: '연결됨',
  login_required: '로그인 필요', error: '인증 오류', unavailable: '로그인 지원 안 됨',
};
const OAUTH_POLL_MS = 90_000;
const OAUTH_TIMEOUT = '로그인 확인 시간이 지났어요. 상태를 새로고침하거나 다시 로그인해 주세요.';

function readOAuthStatus(value: unknown): McpOAuthStatus {
  if (value && typeof value === 'object' && 'status' in value && typeof value.status === 'string'
    && Object.hasOwn(OAUTH_LABELS, value.status) && 'canLogin' in value && typeof value.canLogin === 'boolean') {
    // Deliberately discard backend diagnostics: they must never expose an authorization URL here.
    return { status: value.status as McpOAuthStatus['status'], canLogin: value.canLogin };
  }
  throw new Error('Invalid MCP OAuth status');
}

function closeOAuthPopup(popup: Window | null) {
  try { popup?.close(); } catch { /* The browser may already have discarded the blank window. */ }
}

/* THESIS: Add account connection to HTTP rows without changing server configuration or enablement.
 * OWN-WORLD: Inherit the vault's compact status text, text actions, labelled fields and native dialog.
 * STORY: Read auth state, optionally supply public client metadata, then finish login in a new window.
 * FIRST VIEWPORT: Auth state and Login/Logout sit below each HTTP server's existing name and switch.
 * FORM: Local Operate extension; JSON, server creation and write-only secret controls stay intact. */
function McpOAuthConnect({ name, pending, running, canLogin, error, onClose, onLogin }: {
  name: string; pending: boolean; running: boolean; canLogin: boolean; error: string | null;
  onClose: () => void; onLogin: (input: { clientId?: string; resource?: string }) => void;
}) {
  const [clientId, setClientId] = useState('');
  const [resource, setResource] = useState('');
  const prefix = useId();
  const submit = (event: FormEvent) => {
    event.preventDefault(); event.stopPropagation();
    if (!pending && !running && canLogin) onLogin({
      ...(clientId.trim() ? { clientId: clientId.trim() } : {}),
      ...(resource.trim() ? { resource: resource.trim() } : {}),
    });
  };
  return createPortal(<Modal title={`${name} 로그인`} onClose={onClose} className="compact-modal vault-oauth-modal">
    <header className="modal-header"><h2><Plug size={16} />{name} 로그인</h2><IconButton label="MCP 로그인 닫기" onClick={onClose}><X size={18} /></IconButton></header>
    <form onSubmit={submit}>
      <div className="modal-body vault-modal-body">
        <p className="mcp-form-note">새 브라우저 창에서 로그인해요. 로그인해도 이 서버의 사용 스위치는 바뀌지 않아요.</p>
        <label className="vault-field"><span>클라이언트 ID (선택)</span>
          <input aria-label="클라이언트 ID (선택)" value={clientId} onChange={event => setClientId(event.target.value)} placeholder="자동 등록" maxLength={1024} spellCheck={false} autoComplete="off" disabled={pending || running || !canLogin} data-autofocus aria-describedby={`${prefix}-client-help`} />
          <small id={`${prefix}-client-help`} className="mcp-form-note">서버가 제공한 ID가 없다면 비워 두세요. 자동 등록을 시도해요. 수동 ID는 issuer 응답을 지원하는 공개 클라이언트만 사용할 수 있어요.</small>
        </label>
        <label className="vault-field"><span>리소스 식별자 (선택)</span>
          <input aria-label="리소스 식별자 (선택)" value={resource} onChange={event => setResource(event.target.value)} placeholder="MCP 서버 주소 사용" maxLength={2048} spellCheck={false} autoComplete="off" inputMode="url" disabled={pending || running || !canLogin} aria-describedby={`${prefix}-resource-help`} />
          <small id={`${prefix}-resource-help`} className="mcp-form-note">비워 두면 MCP 서버 주소를 사용해요.</small>
        </label>
        <details className="vault-oauth-resource-help"><summary>리소스 식별자가 필요한 경우</summary><p>서버가 연결 주소와 다른 식별자를 요구할 때만 입력하세요. 예를 들어 Notion의 <code>/mcp</code> 연결에는 <code>https://mcp.notion.com</code>이 필요할 수 있어요.</p></details>
        {running && <p className="mcp-form-note" role="note">실행 중인 작업을 마친 뒤 로그인해 주세요. 입력은 유지돼요.</p>}
        {!canLogin && <p className="mcp-form-note" role="note">지금은 이 서버에 로그인할 수 없어요. 창을 닫고 인증 상태를 새로고침해 주세요.</p>}
        {error && <ErrorNotice>{error}</ErrorNotice>}
      </div>
      <footer className="modal-footer"><button type="button" className="small-button" onClick={onClose}>취소</button><button type="submit" className="primary-button" disabled={pending || running || !canLogin}>{pending ? '로그인 준비 중…' : '브라우저에서 로그인'}</button></footer>
    </form>
  </Modal>, document.body);
}

/** Keyed by bot/server/config identity at the row: no response can acquire a different server label. */
function McpOAuthControls({ server, canEdit, running }: { server: PublicMcpServer; canEdit: boolean | null; running: boolean }) {
  const [oauth, setOAuth] = useState<McpOAuthStatus | null>(null);
  const [checking, setChecking] = useState(false);
  const [polling, setPolling] = useState(false);
  const [timedOut, setTimedOut] = useState(false);
  const [cycle, setCycle] = useState({ generation: 0, poll: false });
  const [pending, setPending] = useState<'login' | 'logout' | null>(null);
  const [connecting, setConnecting] = useState(false);
  const [statusError, setStatusError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const statusRequest = useRef<AbortController | null>(null);
  const mutation = useRef<AbortController | null>(null);
  // This ref holds only an unused about:blank window, never a URL or the provider's opened page.
  const blankPopup = useRef<Window | null>(null);
  const latest = useRef({ canEdit, running });
  latest.current = { canEdit, running };
  const path = `/api/bots/${encodeURIComponent(server.botId)}/mcp-servers/${encodeURIComponent(server.name)}/oauth`;
  useEffect(() => () => {
    const controller = mutation.current;
    mutation.current = null;
    controller?.abort();
    closeOAuthPopup(blankPopup.current);
    blankPopup.current = null;
  }, []);
  useEffect(() => {
    setChecking(false); setPolling(false);
    // Even the status route is sensitive. Never request it from LAN or before access is known.
    if (canEdit !== true || pending) return;
    const controller = new AbortController();
    statusRequest.current = controller;
    let timer: number | undefined;
    let attempts = 0;
    setChecking(true); setPolling(cycle.poll); setTimedOut(false); setStatusError(null);
    const deadline = window.setTimeout(() => {
      if (controller.signal.aborted) return;
      controller.abort(); setChecking(false); setPolling(false);
      setTimedOut(cycle.poll);
      setStatusError(cycle.poll ? OAUTH_TIMEOUT : '인증 상태를 확인하지 못했어요. 잠시 후 상태를 새로고침해 주세요.');
      window.clearTimeout(timer);
    }, cycle.poll ? OAUTH_POLL_MS : 15_000);
    const load = async () => {
      if (controller.signal.aborted) return;
      try {
        const next = readOAuthStatus(await request<unknown>(path, { signal: controller.signal }));
        if (controller.signal.aborted) return;
        attempts++;
        setOAuth(next); setChecking(false);
        const waiting = next.status === 'awaiting_login' || next.status === 'not_connected';
        if (cycle.poll && next.canLogin && waiting && attempts < 45) {
          timer = window.setTimeout(() => void load(), 2_000);
        } else {
          window.clearTimeout(deadline); setPolling(false);
          if (cycle.poll && next.canLogin && waiting) { setTimedOut(true); setStatusError(OAUTH_TIMEOUT); }
          else if (next.status === 'error') setStatusError('서버의 인증 상태에 오류가 있어요. 상태를 새로고침하거나 다시 로그인해 주세요.');
        }
      } catch {
        if (controller.signal.aborted) return;
        window.clearTimeout(deadline); setChecking(false); setPolling(false);
        setStatusError('인증 상태를 확인하지 못했어요. 잠시 후 상태를 새로고침해 주세요.');
      }
    };
    void load();
    return () => {
      controller.abort(); window.clearTimeout(timer); window.clearTimeout(deadline);
      if (statusRequest.current === controller) statusRequest.current = null;
    };
  }, [canEdit, path, pending, cycle, running]);
  const refresh = () => {
    if (canEdit !== true || mutation.current) return;
    setActionError(null);
    setCycle(current => ({ generation: current.generation + 1, poll: false }));
  };
  const closeConnect = () => {
    const controller = mutation.current;
    mutation.current = null;
    controller?.abort();
    closeOAuthPopup(blankPopup.current); blankPopup.current = null;
    setPending(null); setConnecting(false); setActionError(null);
  };
  const login = async (input: { clientId?: string; resource?: string }) => {
    if (canEdit !== true || running || !oauth?.canLogin || oauth.status === 'connected' || mutation.current || polling) return;
    let popup: Window | null = null;
    try {
      // `noopener` in window.open can discard the handle. Detach and verify it synchronously on a
      // same-origin blank window, before requesting any URL or loading untrusted provider content.
      popup = window.open('about:blank', '_blank');
      if (!popup || popup.closed) throw new Error('Popup blocked');
      popup.opener = null;
      if (popup.opener !== null) throw new Error('Cannot detach opener');
      popup.document.title = 'MCP 로그인 준비 중';
      popup.document.documentElement.lang = 'ko';
      const referrer = popup.document.createElement('meta');
      referrer.name = 'referrer'; referrer.content = 'no-referrer';
      popup.document.head.append(referrer);
      popup.document.body.textContent = '로그인 페이지를 준비하고 있어요. 취소하면 이 창이 닫혀요.';
    } catch {
      closeOAuthPopup(popup);
      setActionError('로그인 창을 열 수 없어요. 이 사이트의 팝업을 허용한 뒤 다시 시도해 주세요.');
      return;
    }
    blankPopup.current = popup;
    statusRequest.current?.abort();
    const controller = new AbortController();
    mutation.current = controller;
    setPending('login'); setActionError(null);
    setCycle(current => ({ generation: current.generation + 1, poll: false }));
    const timeout = window.setTimeout(() => controller.abort(), 30_000);
    try {
      const result = await request<{ authorizationUrl: string }>(`${path}/login`, { method: 'POST', body: input, signal: controller.signal });
      if (mutation.current !== controller) return;
      if (controller.signal.aborted) throw new Error('MCP login timed out');
      if (!latest.current.canEdit || latest.current.running) {
        setActionError('지금은 로그인할 수 없어요. 실행 중인 작업을 마친 뒤 서버 컴퓨터에서 다시 시도해 주세요.');
        return;
      }
      if (popup.closed) { setActionError('로그인 창이 닫혔어요. 다시 로그인해 주세요.'); return; }
      // The authorization URL exists only in this call frame, never in React/shared state or errors.
      const url = new URL(result.authorizationUrl);
      if (url.protocol !== 'https:' || url.username || url.password || url.hash || result.authorizationUrl.length > 16_384) throw new Error('Invalid authorization URL');
      popup.opener = null;
      if (popup.opener !== null) throw new Error('Cannot detach opener');
      popup.location.replace(url.href);
      blankPopup.current = null;
      setConnecting(false);
      setCycle(current => ({ generation: current.generation + 1, poll: true }));
    } catch (failure) {
      if (mutation.current !== controller) return;
      setActionError(failure instanceof ApiError && failure.status === 409
        ? '지금은 로그인할 수 없어요. 실행 중인 작업을 마친 뒤 다시 시도해 주세요.'
        : '로그인을 시작하지 못했어요. 서버 연결과 클라이언트 ID·리소스 식별자를 확인한 뒤 다시 시도해 주세요.');
    } finally {
      window.clearTimeout(timeout);
      if (blankPopup.current === popup) { closeOAuthPopup(popup); blankPopup.current = null; }
      if (mutation.current === controller) { mutation.current = null; setPending(null); }
    }
  };
  const logout = async () => {
    if (canEdit !== true || running || mutation.current) return;
    statusRequest.current?.abort();
    const controller = new AbortController();
    mutation.current = controller;
    setPending('logout'); setActionError(null); setConnecting(false);
    setCycle(current => ({ generation: current.generation + 1, poll: false }));
    const timeout = window.setTimeout(() => controller.abort(), 25_000);
    try {
      const next = readOAuthStatus(await request<unknown>(`${path}/logout`, { method: 'POST', body: {}, signal: controller.signal }));
      if (mutation.current !== controller) return;
      if (controller.signal.aborted) throw new Error('MCP logout timed out');
      setOAuth(next);
    } catch (failure) {
      if (mutation.current !== controller) return;
      setActionError(failure instanceof ApiError && failure.status === 409
        ? '로그아웃하지 못했어요. 실행 중인 작업을 마친 뒤 다시 시도해 주세요.'
        : '로그아웃하지 못했어요. 연결 상태를 확인한 뒤 다시 시도해 주세요.');
    } finally {
      window.clearTimeout(timeout);
      if (mutation.current === controller) { mutation.current = null; setPending(null); }
    }
  };
  const label = canEdit === null ? '접근 확인 중…' : canEdit === false ? '로컬에서만 로그인'
    : pending === 'logout' ? '로그아웃 중…' : statusError ? timedOut ? '로그인 확인 시간 초과' : '인증 확인 실패'
      : checking ? '인증 확인 중…' : oauth ? OAUTH_LABELS[oauth.status] : '인증 확인 중…';
  const loginLabel = timedOut || oauth?.status === 'login_required' || oauth?.status === 'error' ? '다시 로그인' : '로그인';
  const canLogout = oauth && ['connected', 'awaiting_login', 'login_required'].includes(oauth.status);
  return <div className="vault-oauth">
    <div className="vault-oauth-bar">
      <span className={`vault-oauth-state${!statusError && oauth?.status === 'connected' ? ' connected' : ''}`} role="status" aria-label={`${server.name} 인증 상태`}>{label}</span>
      <span className="vault-oauth-actions">
        {canEdit === true && <IconButton label={`${server.name} 인증 상태 새로고침`} onClick={refresh} disabled={checking || polling || !!pending}><RefreshCw size={12} /></IconButton>}
        {oauth?.status !== 'connected' && <button type="button" className="text-button" aria-label={`${server.name} ${loginLabel}`} disabled={canEdit !== true || !oauth?.canLogin || running || !!pending || checking || polling} onClick={() => { setActionError(null); setConnecting(true); }}>{loginLabel}</button>}
        {canLogout && <button type="button" className="text-button" aria-label={`${server.name} 로그아웃`} disabled={canEdit !== true || running || !!pending} onClick={() => void logout()}>로그아웃</button>}
      </span>
    </div>
    {polling && <p className="vault-oauth-note">새 창에서 로그인을 마쳐 주세요. 최대 90초 동안 연결 상태를 확인해요.</p>}
    {running && <p className="vault-oauth-note">실행 중인 작업을 마친 뒤 로그인·로그아웃할 수 있어요.</p>}
    {canEdit && oauth?.canLogin === false && oauth.status !== 'connected' && <p className="vault-oauth-note">이 서버 구성에서는 OAuth 로그인을 사용할 수 없어요.</p>}
    {statusError && <ErrorNotice>{statusError}</ErrorNotice>}
    {actionError && !connecting && <ErrorNotice>{actionError}</ErrorNotice>}
    {connecting && <McpOAuthConnect name={server.name} pending={pending === 'login'} running={running} canLogin={canEdit === true && !!oauth?.canLogin && oauth.status !== 'connected'} error={actionError} onClose={closeConnect} onLogin={input => void login(input)} />}
  </div>;
}

/** Details-pane sections: the bot's custom MCP servers and the secrets it can use. */
export function BotIntegrations({ bot, data, dispatch }: { bot: Bot; data: AppState; dispatch: Dispatch<Action> }) {
  const canEdit = useVaultAccess();
  const prefix = useId();
  const [editing, setEditing] = useState<'form' | 'json' | null>(null);
  const [toggling, setToggling] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState<string | null>(null);
  const lock = useRef(false);
  const snapshot = (state: AppState) => dispatch({ type: 'snapshot', data: state });
  const secretActions = useSecretActions(snapshot);
  useEffect(() => { setError(null); setStatus(null); setEditing(null); }, [bot.id]);
  const servers = (data.mcpServers ?? []).filter((server) => server.botId === bot.id);
  const secrets = (data.secrets ?? []).filter((secret) => !secret.botId || secret.botId === bot.id);
  const available = new Set(secrets.map((secret) => secret.name));
  const running = !!runningConversation(data);
  const toggle = async (server: PublicMcpServer, enabled: boolean) => {
    if (lock.current) return;
    if (enabled && server.transport === 'stdio' && !window.confirm(`${server.name} 서버를 켜면 이 Bot의 대화가 시작될 때 서버 컴퓨터에서 명령을 실행해요. 켤까요?`)) return;
    lock.current = true;
    setToggling(server.name); setError(null); setStatus(null);
    try {
      snapshot(await request<AppState>(`/api/bots/${encodeURIComponent(bot.id)}/mcp-servers/${encodeURIComponent(server.name)}`, { method: 'PATCH', body: { enabled } }));
      setStatus(`${server.name} 서버를 ${enabled ? '켰어요' : '껐어요'}.`);
    } catch (failure) { setError(errorText(failure)); } finally { lock.current = false; setToggling(null); }
  };
  return <>
    <section className="details-section vault-section" aria-labelledby={`${prefix}-mcp`}>
      <header><h3 id={`${prefix}-mcp`}>MCP 서버</h3><button type="button" className="text-button" disabled={!canEdit || running} onClick={() => setEditing('json')}>MCP 서버 JSON</button></header>
      <p className="vault-help">이 Bot을 위한 사용자 지정 MCP 서버를 추가하세요. 도구는 켠 서버만 쓰고, 호출마다 승인을 요청해요.</p>
      {canEdit === false && <p className="vault-help vault-lan-note" role="note">{LAN_NOTE}</p>}
      {servers.length ? <ul className="vault-list" aria-label={`${bot.name}의 MCP 서버`}>
        {servers.map((server) => {
          const missing = (server.secretRefs ?? []).filter((name) => !available.has(name));
          return <li key={server.id} className={`vault-row${server.transport === 'http' ? ' vault-mcp-row' : ''}`} aria-label={`MCP 서버 ${server.name}`}>
            {server.transport === 'stdio' ? <Terminal size={14} aria-hidden="true" /> : <Plug size={14} aria-hidden="true" />}
            <span className="vault-row-copy"><code>{server.name}</code><small>{server.transport === 'stdio' ? 'STDIO · 서버 컴퓨터에서 명령 실행' : 'HTTP'}{missing.length ? ` · 보관함에 없는 시크릿: ${missing.join(', ')}` : ''}</small></span>
            <label className="vault-switch">
              <input type="checkbox" role="switch" aria-label={`${server.name} 사용`} checked={server.enabled} disabled={toggling !== null || (!server.enabled && !canEdit)} onChange={(event) => void toggle(server, event.target.checked)} />
              <span>{server.enabled ? '켜짐' : '꺼짐'}</span>
            </label>
            {server.transport === 'http' && <McpOAuthControls key={JSON.stringify([bot.id, server.id, server.name, server.updatedAt])} server={server} canEdit={canEdit} running={running} />}
          </li>;
        })}
      </ul> : <p className="detail-empty">아직 사용자 지정 MCP 서버가 없어요</p>}
      <button type="button" className="small-button" disabled={!canEdit || running} onClick={() => setEditing('form')}><Plus size={13} />사용자 지정 MCP 추가</button>
      {error && <ErrorNotice onDismiss={() => setError(null)}>{error}</ErrorNotice>}
      {status && <p className="vault-status" role="status">{status}</p>}
    </section>
    <section className="details-section vault-section" aria-labelledby={`${prefix}-secrets`}>
      <header><h3 id={`${prefix}-secrets`}>시크릿</h3><button type="button" className="text-button" disabled={!canEdit || secretActions.pending} onClick={() => secretActions.setDialog('add')}>시크릿 추가</button></header>
      <p className="vault-help">이 Bot에 필요한 시크릿을 추가하세요. 값은 서버에 저장되고 다시 표시되지 않아요.</p>
      {secrets.length ? <SecretRows data={data} secrets={secrets} canEdit={!!canEdit} busy={secretActions.pending} onReplace={(secret) => secretActions.setDialog({ replace: secret })} onDelete={(secret) => void secretActions.remove(secret)} />
        : <p className="detail-empty">아직 시크릿이 없습니다</p>}
      {secretActions.error && <ErrorNotice onDismiss={() => secretActions.setError(null)}>{secretActions.error}</ErrorNotice>}
      {secretActions.status && <p className="vault-status" role="status">{secretActions.status}</p>}
    </section>
    {editing === 'json' && <McpEditor bot={bot} onClose={() => setEditing(null)} onSaved={(state) => { snapshot(state); setEditing(null); setStatus('MCP 서버를 저장했어요. 새로 추가하거나 바꾼 서버는 꺼져 있어요.'); }} />}
    {editing === 'form' && <McpServerForm bot={bot} data={data} canEdit={!!canEdit} running={running} onClose={() => setEditing(null)} onSaved={(state) => { snapshot(state); setEditing(null); setStatus('MCP 서버를 추가했어요. 목록에서 켜면 다음 대화부터 사용할 수 있어요.'); }} />}
    {secretActions.dialog === 'add' && <SecretForm data={data} botId={bot.id} onClose={() => secretActions.setDialog(null)} onSaved={secretActions.saved} />}
    {secretActions.dialog && typeof secretActions.dialog === 'object' && <SecretForm data={data} replace={secretActions.dialog.replace} onClose={() => secretActions.setDialog(null)} onSaved={secretActions.saved} />}
  </>;
}
