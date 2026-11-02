// Operate / local extension: inherit the existing visualization chrome and approval language.
// Only a saved server/tool resource opts into this bridge. Ordinary Markdown frames stay inert.
import { useCallback, useEffect, useRef, useState } from 'react';
import { Code2, Eye, LoaderCircle, Maximize2, ShieldQuestion } from 'lucide-react';
import type { McpApp } from '../shared/types';
import { errorText, request } from './api';
import { PreviewModal } from './Previews';
import { VisualizationFrame } from './Visualization';
import { sourceProblem, sourceTitle } from './visualization-document';
import { MCP_APP_PROTOCOL_VERSION, readAppRequest, record, rpcError, rpcResult, type RpcId } from './mcp-app-protocol';
import { IconButton } from './ui';
import './mcp-app.css';

type Respond = (message: unknown) => void;
interface PendingCall {
  id: RpcId;
  name: string;
  arguments: Record<string, unknown>;
  respond: Respond;
  generation: number;
  requestId?: string;
  deciding?: boolean;
}
const denied = { content: [{ type: 'text', text: '사용자가 MCP 앱의 도구 호출을 거절했습니다.' }], isError: true };

function McpAppSession({ app, path, fill, onEscape }: { app: McpApp; path: string; fill?: boolean; onEscape?: () => void }) {
  const [pending, setPending] = useState<PendingCall | null>(null);
  const [error, setError] = useState('');
  const current = useRef<PendingCall | null>(null);
  const generation = useRef(0);
  const initialized = useRef(false);
  const initializing = useRef(false);
  const usedIds = useRef(new Set<string>());
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const discard = useCallback((call: PendingCall) => {
    if (call.requestId && !call.deciding) void request(`${path}/requests/${encodeURIComponent(call.requestId)}`, { method: 'POST', body: { allow: false } }).catch(() => {});
  }, [path]);
  const reset = useCallback(() => {
    generation.current++;
    initialized.current = false;
    initializing.current = false;
    usedIds.current.clear();
    if (timer.current) clearTimeout(timer.current);
    if (current.current) discard(current.current);
    current.current = null;
    setPending(null);
  }, [discard]);
  useEffect(() => () => reset(), [reset]);

  const receive = useCallback((data: unknown, respond: Respond) => {
    const message = readAppRequest(data);
    if (!message) return;
    const { id, method, params } = message;
    if (method === 'ui/initialize' && id !== undefined) {
      if (initializing.current || params.protocolVersion !== MCP_APP_PROTOCOL_VERSION) {
        respond(rpcError(id, -32602, '지원하지 않는 앱 초기화 요청입니다.'));
        return;
      }
      initializing.current = true;
      respond(rpcResult(id, {
        protocolVersion: MCP_APP_PROTOCOL_VERSION,
        hostInfo: { name: 'grok-bot-local', version: '0.1.0' },
        hostCapabilities: { serverTools: {}, sandbox: { permissions: {}, csp: { connectDomains: [], resourceDomains: [], frameDomains: [] } } },
        hostContext: { theme: document.documentElement.dataset.theme ?? 'dark', displayMode: fill ? 'fullscreen' : 'inline', availableDisplayModes: ['inline', 'fullscreen'], locale: 'ko-KR', platform: 'web' },
      }));
      return;
    }
    if (method === 'ui/notifications/initialized' && id === undefined && initializing.current && !initialized.current) {
      initialized.current = true;
      let arguments_: unknown = {};
      try { arguments_ = JSON.parse(app.input); } catch { /* Old/invalid input never becomes a tool call. */ }
      respond({ jsonrpc: '2.0', method: 'ui/notifications/tool-input', params: { arguments: arguments_ } });
      respond({ jsonrpc: '2.0', method: 'ui/notifications/tool-result', params: { content: [{ type: 'text', text: app.output }] } });
      return;
    }
    if (method === 'notifications/cancelled' && id === undefined) {
      const call = current.current;
      if (call && call.id === params.requestId && !call.deciding) {
        if (timer.current) clearTimeout(timer.current);
        discard(call);
        current.current = null;
        setPending(null);
      }
      return;
    }
    if (id === undefined) return;
    if (method === 'ping') { respond(rpcResult(id, {})); return; }
    if (!initialized.current) { respond(rpcError(id, -32002, '앱 초기화를 먼저 마쳐 주세요.')); return; }
    if (method !== 'tools/call') { respond(rpcError(id, -32601, '이 앱 요청은 지원하지 않습니다.')); return; }
    if (typeof params.name !== 'string' || params.name.length === 0 || params.name.length > 128 || (params.arguments !== undefined && !record(params.arguments))) {
      respond(rpcError(id, -32602, '올바른 도구 이름과 인수가 필요합니다.'));
      return;
    }
    const key = `${typeof id}:${id}`;
    // Repeated IDs cannot create another approval or execute a previous request again.
    if (usedIds.current.has(key)) return;
    if (usedIds.current.size >= 128) { respond(rpcError(id, -32000, '이 앱 연결의 요청 한도에 도달했습니다. 코드 보기를 연 뒤 미리보기로 돌아와 다시 연결해 주세요.')); return; }
    if (current.current) { respond(rpcError(id, -32000, '이미 승인 또는 실행을 기다리는 요청이 있습니다.')); return; }
    usedIds.current.add(key);
    const call: PendingCall = { id, name: params.name, arguments: (params.arguments ?? {}) as Record<string, unknown>, respond, generation: generation.current };
    current.current = call;
    setPending(call);
    setError('');
    void request<{ requestId: string }>(`${path}/requests`, { method: 'POST', body: { name: call.name, arguments: call.arguments } }).then(({ requestId }) => {
      call.requestId = requestId;
      if (current.current !== call || generation.current !== call.generation) { discard(call); return; }
      setPending({ ...call });
      timer.current = setTimeout(() => {
        if (current.current !== call || call.deciding) return;
        discard(call);
        call.respond(rpcError(call.id, -32000, '승인 시간이 만료되었습니다. 앱에서 다시 요청해 주세요.'));
        current.current = null;
        setPending(null);
        setError('승인 시간이 만료되었어요. 앱에서 다시 요청해 주세요.');
      }, 115_000);
    }).catch((cause) => {
      if (current.current !== call || generation.current !== call.generation) return;
      call.respond(rpcError(id, -32000, errorText(cause)));
      current.current = null;
      setPending(null);
      setError(errorText(cause));
    });
  }, [app.input, app.output, discard, fill, path]);

  const choose = async (allow: boolean) => {
    const call = current.current;
    if (!call?.requestId || call.deciding) return;
    call.deciding = true;
    if (timer.current) clearTimeout(timer.current);
    setPending({ ...call });
    try {
      const result = await request<unknown>(`${path}/requests/${encodeURIComponent(call.requestId)}`, { method: 'POST', body: { allow } });
      if (generation.current === call.generation) call.respond(rpcResult(call.id, allow ? result : denied));
    } catch (cause) {
      if (generation.current === call.generation) {
        call.respond(rpcError(call.id, -32000, errorText(cause)));
        setError(errorText(cause));
      }
    } finally {
      if (current.current === call) { current.current = null; setPending(null); }
    }
  };
  return <div className={`mcp-app-session ${fill ? 'is-fill' : ''}`}>
    <span className="mcp-app-announcement" role="status" aria-atomic="true">{pending ? (pending.deciding ? '서버에서 처리 중' : '도구 호출을 검토하고 이번 호출만 승인하거나 거절해 주세요.') : ''}</span>
    <VisualizationFrame source={app.html} kind="html" title={sourceTitle(app.html) ?? 'MCP 앱'} fill={fill} onEscape={onEscape} onAppMessage={receive} onAppReset={reset} />
    {pending && <section className="permission-card mcp-app-permission" aria-label="MCP 앱 도구 승인 요청">
      <header><ShieldQuestion size={19} /><div><h3>MCP 앱 · {app.server}: {pending.name}</h3><small>{pending.deciding ? '서버에서 처리 중' : '이번 호출만 승인'}</small></div></header>
      <pre className="permission-description">{JSON.stringify(pending.arguments, null, 2)}</pre>
      <p className="permission-hint">앱이 요청한 도구와 인수를 확인하세요. 기존 자동 승인 규칙과 관계없이 매번 선택합니다.</p>
      <div className="permission-actions">
        <button type="button" className="primary-button" disabled={!pending.requestId || pending.deciding} onClick={() => void choose(true)}>{pending.deciding ? <><LoaderCircle size={14} className="spin" />처리 중…</> : '한 번 실행'}</button>
        <button type="button" className="small-button" disabled={!pending.requestId || pending.deciding} onClick={() => void choose(false)}>거절</button>
      </div>
    </section>}
    {error && <p className="mcp-app-error" role="alert">{error}</p>}
  </div>;
}

export function McpAppCard({ app, conversationId, toolId }: { app: McpApp; conversationId: string; toolId: string }) {
  const [code, setCode] = useState(false);
  const [expanded, setExpanded] = useState(false);
  const title = sourceTitle(app.html) ?? 'MCP 앱';
  const path = `/api/conversations/${encodeURIComponent(conversationId)}/mcp-apps/${encodeURIComponent(toolId)}`;
  const problem = sourceProblem(app.html);
  return <figure className="visualization-card mcp-app-card" aria-label={`${title} · MCP 앱`}>
    <figcaption className="visualization-header"><span className="visualization-title" title={title}>{title}</span><div className="visualization-actions">
      <button type="button" className="visualization-toggle" aria-pressed={code} onClick={() => setCode(!code)}>{code ? <><Eye size={13} />미리보기</> : <><Code2 size={13} />코드 보기</>}</button>
      <IconButton label="MCP 앱 확대" disabled={!!problem} onClick={() => setExpanded(true)}><Maximize2 size={14} /></IconButton>
    </div></figcaption>
    <p className="mcp-app-policy">MCP 앱 · {app.server} · 외부 연결 차단 · 도구 호출은 매번 승인</p>
    {problem ? <div className="visualization-failure" role="alert">저장된 앱의 HTML을 표시할 수 없어요.</div> : code ? <pre className="visualization-code"><code>{app.html}</code></pre> : !expanded && <McpAppSession app={app} path={path} />}
    {expanded && <PreviewModal title="MCP 앱 미리보기" closeLabel="MCP 앱 미리보기 닫기" heading={title} className="visualization-viewer" onClose={() => setExpanded(false)}>
      {(close) => <McpAppSession app={app} path={path} fill onEscape={close} />}
    </PreviewModal>}
  </figure>;
}
