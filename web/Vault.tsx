import { useEffect, useId, useRef, useState, type Dispatch, type FormEvent } from 'react';
import { createPortal } from 'react-dom';
import { KeyRound, Plug, Plus, Search, ShieldAlert, Terminal, Trash2, X } from 'lucide-react';
import type { AppState, Bot } from '../shared/types';
import type { McpConfigDocument, PublicMcpServer, PublicSecret, SecretInput, VaultAccess } from '../shared/vault';
import { errorText, request } from './api';
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
          return <li key={server.id} className="vault-row" aria-label={`MCP 서버 ${server.name}`}>
            {server.transport === 'stdio' ? <Terminal size={14} aria-hidden="true" /> : <Plug size={14} aria-hidden="true" />}
            <span className="vault-row-copy"><code>{server.name}</code><small>{server.transport === 'stdio' ? 'STDIO · 서버 컴퓨터에서 명령 실행' : 'HTTP'}{missing.length ? ` · 보관함에 없는 시크릿: ${missing.join(', ')}` : ''}</small></span>
            <label className="vault-switch">
              <input type="checkbox" role="switch" aria-label={`${server.name} 사용`} checked={server.enabled} disabled={toggling !== null || (!server.enabled && !canEdit)} onChange={(event) => void toggle(server, event.target.checked)} />
              <span>{server.enabled ? '켜짐' : '꺼짐'}</span>
            </label>
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
