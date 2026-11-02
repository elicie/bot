import { useId, useRef, useState, type FormEvent } from 'react';
import { KeyRound, Plug, Plus, ShieldAlert, Trash2, X } from 'lucide-react';
import type { AppState, Bot } from '../shared/types';
import type { McpConfigDocument, McpConfigInput, McpConnectionTestInput, McpConnectionTestResult } from '../shared/vault';
import { errorText, request } from './api';
import { ErrorNotice, IconButton, Modal } from './ui';
import { appendMcpDraft, availableMcpSecrets, buildMcpDraft, credentialRows, emptyMcpDraft, mcpDraftProblems, type McpCredential, type McpDraft } from './mcp-form';

/* THESIS: Add one server without editing a document or changing the existing details pane.
 * OWN-WORLD: Inherit the dark/light vault dialog, compact labelled fields and blue action.
 * STORY: Choose transport, supply connection and write-only credentials, test, then add disabled.
 * FIRST VIEWPORT: Safety note, transport and name, connection fields; actions stay at the foot.
 * FORM: Local Operate extension. JSON remains the advanced editor and storage source of truth. */
export function McpServerForm({ bot, data, canEdit, running, onClose, onSaved }: {
  bot: Bot; data: AppState; canEdit: boolean; running: boolean; onClose: () => void;
  onSaved: (state: AppState) => void;
}) {
  const [draft, setDraft] = useState(emptyMcpDraft);
  const [pending, setPending] = useState<'test' | 'save' | null>(null);
  const [attempted, setAttempted] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [tested, setTested] = useState<number | null>(null);
  const lock = useRef(false);
  const form = useRef<HTMLFormElement>(null);
  const prefix = useId();
  const secrets = availableMcpSecrets(data.secrets ?? [], bot.id);
  const servers = (data.mcpServers ?? []).filter(server => server.botId === bot.id);
  const problems = attempted ? mcpDraftProblems(draft, servers, secrets) : {};
  const blocked = !!pending || !canEdit || running;
  const rows = credentialRows(draft);
  const pairKind = draft.transport === 'stdio' ? '환경 변수' : '헤더';
  const change = (next: McpDraft) => { setDraft(next); setTested(null); setError(null); };
  const changeRows = (next: McpCredential[]) => change({ ...draft, [draft.transport === 'stdio' ? 'env' : 'headers']: next });
  const changeRow = (id: string, fields: Partial<McpCredential>) => changeRows(rows.map(row => row.id === id ? { ...row, ...fields } : row));
  const addRow = () => {
    const used = new Set([...secrets.map(secret => secret.name), ...draft.env.map(row => row.secret), ...draft.headers.map(row => row.secret)]);
    const stem = `MCP_${draft.name.toUpperCase().replace(/[^A-Z0-9_]/g, '_') || 'SERVER'}_TOKEN`.slice(0, 59);
    let secret = stem;
    for (let number = 2; used.has(secret); number++) secret = `${stem}_${number}`;
    changeRows([...rows, { id: crypto.randomUUID(), name: '', source: secrets.length ? 'vault' : 'new', secret: secrets.length ? '' : secret, value: '', prefix: '' }]);
  };
  const validate = () => {
    setAttempted(true);
    if (Object.keys(mcpDraftProblems(draft, servers, secrets)).length) {
      setError('입력 내용을 확인해 주세요. 잘못된 항목 아래에 수정 방법을 표시했어요.');
      requestAnimationFrame(() => form.current?.querySelector<HTMLElement>('[aria-invalid="true"]')?.focus());
      return false;
    }
    return true;
  };
  const testConnection = async () => {
    if (lock.current || blocked || !validate()) return;
    if (draft.transport === 'stdio' && !window.confirm('연결 테스트는 서버 컴퓨터에서 입력한 명령을 실제로 실행해요. 신뢰하는 명령인지 확인했나요?')) return;
    lock.current = true; setPending('test'); setError(null); setTested(null);
    try {
      const input = buildMcpDraft(draft, bot.id);
      const body: McpConnectionTestInput = { json: input.json, secrets: input.secrets };
      const result = await request<McpConnectionTestResult>(`/api/bots/${encodeURIComponent(bot.id)}/mcp-servers/test`, { method: 'POST', body });
      setTested(result.toolCount);
    } catch (failure) { setError(`연결 테스트에 실패했어요. ${errorText(failure)}`); }
    finally { lock.current = false; setPending(null); }
  };
  const save = async (event: FormEvent) => {
    event.preventDefault(); event.stopPropagation();
    if (lock.current || blocked || !validate()) return;
    lock.current = true; setPending('save'); setError(null);
    try {
      const input = buildMcpDraft(draft, bot.id);
      const path = `/api/bots/${encodeURIComponent(bot.id)}/mcp-servers`;
      // One existing PUT saves the new secrets and merged server document together, or neither.
      const latest = await request<McpConfigDocument>(path);
      const json = appendMcpDraft(latest.json, input);
      const body: McpConfigInput = { json, secrets: input.secrets };
      onSaved(await request<AppState>(path, { method: 'PUT', body }));
    } catch (failure) {
      setError(`서버가 추가되지 않았어요. 입력을 유지했으니 확인 후 다시 추가해 주세요. ${errorText(failure)}`);
    } finally { lock.current = false; setPending(null); }
  };
  const dirty = JSON.stringify(draft) !== JSON.stringify(emptyMcpDraft());
  const close = () => { if (!pending && (!dirty || window.confirm('입력 중인 MCP 서버를 버리고 닫을까요?'))) onClose(); };
  const problem = (field: string) => problems[field] ? <small id={`${prefix}-${field}-error`} className="vault-invalid">{problems[field]}</small> : null;
  const described = (field: string) => problems[field] ? `${prefix}-${field}-error` : undefined;
  const pairDescription = (row: McpCredential, field: string) => problems[`pair-${row.id}-${field}`] ? described(`pair-${row.id}`) : undefined;

  return <Modal title="MCP 서버 추가" onClose={close} className="compact-modal vault-modal mcp-form-modal">
    <header className="modal-header"><h2><Plug size={16} />MCP 서버 추가</h2><IconButton label="MCP 서버 추가 닫기" onClick={close} disabled={!!pending}><X size={18} /></IconButton></header>
    <form ref={form} onSubmit={event => void save(event)} noValidate>
      <div className="modal-body mcp-form-body">
        <p className="vault-warning" role="note"><ShieldAlert size={15} aria-hidden="true" /><span>로컬 명령은 서버 컴퓨터에서 이 앱과 같은 권한으로 실행돼요. 신뢰하는 서버만 추가하세요. <strong>새 서버는 꺼진 상태로 저장돼요.</strong></span></p>
        <p className="mcp-form-note">{bot.name}의 다음 대화부터 사용할 서버예요. 연결 테스트는 도구 목록만 확인하고 도구를 호출하지 않아요.</p>
        <fieldset disabled={blocked} className="mcp-form-fields">
          <div className="mcp-form-grid">
            <label className="vault-field"><span>종류</span><select aria-label="종류" value={draft.transport} onChange={event => change({ ...draft, transport: event.target.value as McpDraft['transport'] })}><option value="stdio">로컬 명령 (STDIO)</option><option value="http">원격 URL (HTTP)</option></select></label>
            <label className="vault-field"><span>서버 이름</span><input aria-label="서버 이름" value={draft.name} onChange={event => change({ ...draft, name: event.target.value })} placeholder="my-server" maxLength={32} autoComplete="off" spellCheck={false} data-autofocus aria-invalid={!!problems.name} aria-describedby={described('name')} />{problem('name')}</label>
          </div>
          {draft.transport === 'stdio' ? <>
            <label className="vault-field"><span>명령</span><input aria-label="명령" value={draft.command} onChange={event => change({ ...draft, command: event.target.value })} placeholder="node 또는 /절대/경로/명령" maxLength={1024} autoComplete="off" spellCheck={false} aria-invalid={!!problems.command} aria-describedby={described('command')} />{problem('command')}</label>
            <section className="mcp-form-section" aria-label="명령 인자">
              <header><h3>인자</h3><button type="button" className="text-button" disabled={draft.args.length >= 64} onClick={() => change({ ...draft, args: [...draft.args, ''] })}><Plus size={13} />인자 추가</button></header>
              <p className="mcp-form-note">인자 하나를 한 칸에 입력하세요. 공백이나 따옴표로 나누지 않아요. 빈 칸은 삭제해 주세요.</p>
              {draft.args.map((arg, index) => <div className="mcp-arg-row" key={index}><label className="vault-field"><span>인자 {index + 1}</span><input aria-label={`인자 ${index + 1}`} value={arg} onChange={event => change({ ...draft, args: draft.args.map((value, i) => i === index ? event.target.value : value) })} maxLength={4096} spellCheck={false} aria-invalid={!!problems[`arg-${index}`]} aria-describedby={problems[`arg-${index}`] ? `${prefix}-arg-${index}-error` : undefined} />{problem(`arg-${index}`)}</label><IconButton label={`인자 ${index + 1} 삭제`} onClick={() => change({ ...draft, args: draft.args.filter((_, i) => i !== index) })}><Trash2 size={14} /></IconButton></div>)}
              {draft.args.length > 64 && problem('args')}
            </section>
          </> : <label className="vault-field"><span>서버 URL</span><input aria-label="서버 URL" type="url" value={draft.url} onChange={event => change({ ...draft, url: event.target.value })} placeholder="https://example.com/mcp" maxLength={2048} autoComplete="off" spellCheck={false} aria-invalid={!!problems.url} aria-describedby={described('url')} />{problem('url')}<small className="mcp-form-note">Streamable HTTP 주소를 입력하세요. 인증 정보는 아래 헤더에서 설정해요.</small></label>}
          <section className="mcp-form-section" aria-label={pairKind}>
            <header><h3>{pairKind}</h3><button type="button" className="text-button" onClick={addRow} disabled={rows.length >= 64}><Plus size={13} />{pairKind} 추가</button></header>
            <p className="mcp-form-note">보관함의 값을 선택하거나 새 시크릿을 입력하세요. 새 값은 서버 추가 시 이 Bot의 보관함에 저장되고, 연결 테스트 중에는 저장되지 않아요.</p>
            {rows.map((row, index) => <div className="mcp-credential" key={row.id} role="group" aria-label={`${pairKind} ${index + 1}`} aria-describedby={described(`pair-${row.id}`)}>
              <div className="mcp-credential-heading"><span><KeyRound size={13} aria-hidden="true" />{pairKind} {index + 1}</span><IconButton label={`${pairKind} ${index + 1} 삭제`} onClick={() => changeRows(rows.filter(item => item.id !== row.id))}><Trash2 size={14} /></IconButton></div>
              <div className="mcp-form-grid">
                <label className="vault-field"><span>{pairKind} 이름</span><input aria-label={`${pairKind} 이름`} value={row.name} onChange={event => changeRow(row.id, { name: event.target.value })} placeholder={draft.transport === 'stdio' ? 'API_TOKEN' : 'Authorization'} maxLength={128} spellCheck={false} autoComplete="off" aria-invalid={!!problems[`pair-${row.id}-name`]} aria-describedby={pairDescription(row, 'name')} /></label>
                <label className="vault-field"><span>값 선택</span><select aria-label="값 선택" aria-invalid={row.source === 'vault' && !!problems[`pair-${row.id}-secret`]} aria-describedby={row.source === 'vault' ? pairDescription(row, 'secret') : undefined} value={row.source === 'new' ? '@new' : row.secret} onChange={event => changeRow(row.id, event.target.value === '@new' ? { source: 'new', secret: '', value: '' } : { source: 'vault', secret: event.target.value, value: '' })}><option value="">보관함에서 선택</option>{secrets.map(secret => <option key={secret.id} value={secret.name}>{secret.name} · {secret.botId ? '이 Bot' : '모든 Bot'}</option>)}<option value="@new">직접 입력 → 새 시크릿</option></select></label>
              </div>
              {row.source === 'new' && <div className="mcp-form-grid">
                <label className="vault-field"><span>새 시크릿 이름</span><input aria-label="새 시크릿 이름" aria-invalid={!!problems[`pair-${row.id}-secret`]} aria-describedby={pairDescription(row, 'secret')} value={row.secret} onChange={event => changeRow(row.id, { secret: event.target.value.toUpperCase() })} placeholder="MCP_API_TOKEN" maxLength={64} autoComplete="off" spellCheck={false} /></label>
                <label className="vault-field"><span>새 시크릿 값</span><input aria-label="새 시크릿 값" aria-invalid={!!problems[`pair-${row.id}-value`]} aria-describedby={pairDescription(row, 'value')} type="password" value={row.value} onChange={event => changeRow(row.id, { value: event.target.value })} placeholder="8자 이상 · 저장 후 표시 안 됨" maxLength={8192} autoComplete="new-password" spellCheck={false} /></label>
              </div>}
              {draft.transport === 'http' && <label className="vault-field"><span>접두사 (선택)</span><input aria-label="접두사 (선택)" aria-invalid={!!problems[`pair-${row.id}-prefix`]} aria-describedby={pairDescription(row, 'prefix')} value={row.prefix} onChange={event => changeRow(row.id, { prefix: event.target.value })} placeholder="예: Bearer 뒤에 공백 한 칸" maxLength={8000} autoComplete="off" spellCheck={false} /><small className="mcp-form-note">시크릿 앞에 그대로 붙여요. Bearer 인증은 끝에 공백도 입력하세요.</small></label>}
              {problem(`pair-${row.id}`)}
            </div>)}
            {problem('pairs')}
          </section>
        </fieldset>
        {(running || !canEdit) && <p className="mcp-form-note" role="note">{running ? '실행 중인 작업을 마친 뒤 연결을 테스트하거나 서버를 추가해 주세요. 입력은 유지돼요.' : '서버 컴퓨터의 localhost 연결에서만 서버를 추가하고 테스트할 수 있어요.'}</p>}
      </div>
      {(error || tested !== null) && <div className="mcp-form-feedback">
        {error && <ErrorNotice>{error}</ErrorNotice>}
        {tested !== null && <p className="mcp-test-result" role="status">연결됐어요. 도구 {tested}개를 확인했어요. 서버는 아직 추가되지 않았어요.</p>}
      </div>}
      <footer className="modal-footer mcp-form-footer"><button type="button" className="small-button" onClick={() => void testConnection()} disabled={blocked}>{pending === 'test' ? '연결 확인 중…' : '연결 테스트'}</button><span /><button type="button" className="small-button" onClick={close} disabled={!!pending}>취소</button><button type="submit" className="primary-button" disabled={blocked}>{pending === 'save' ? '추가 중…' : '서버 추가'}</button></footer>
    </form>
  </Modal>;
}
