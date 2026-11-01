import { useEffect, useRef, useState } from 'react';
import { ArrowDown, ArrowRight, ArrowUp, Expand, Globe2, LoaderCircle, Monitor, Play, RefreshCw, Square, X } from 'lucide-react';
import type { ComputerAction, ComputerObservation, ComputerStatus } from '../shared/computer';
import { ApiError, errorText, request } from './api';
import { Avatar, ErrorNotice, IconButton } from './ui';
import './computer.css';

export function Computer({ ownerId, canControl }: { ownerId: string; canControl: boolean }) {
  const [status, setStatus] = useState<ComputerStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [localAccess, setLocalAccess] = useState(true);
  const [busy, setBusy] = useState(false);
  const [address, setAddress] = useState('https://example.com');
  const [origin, setOrigin] = useState('https://example.com');
  const [text, setText] = useState('');
  const [expanded, setExpanded] = useState(false);
  const [loadedPreview, setLoadedPreview] = useState<string | null>(null);
  const preview = useRef<HTMLImageElement>(null);
  const mounted = useRef(true);
  const currentOwner = useRef(ownerId);
  const operationVersion = useRef(0);
  const operationPending = useRef(false);
  currentOwner.current = ownerId;
  const base = '/api/computer';

  useEffect(() => {
    mounted.current = true;
    operationVersion.current++;
    operationPending.current = false;
    setStatus(null); setError(null); setBusy(false); setLocalAccess(true); setText(''); setExpanded(false); setLoadedPreview(null);
    let active = true;
    let loading = false;
    const controller = new AbortController();
    const load = async () => {
      if (loading || operationPending.current) return;
      loading = true;
      const version = operationVersion.current;
      try {
        const next = await request<ComputerStatus>(`${base}?owner=${encodeURIComponent(ownerId)}`, { signal: controller.signal });
        if (active && version === operationVersion.current) setStatus(next);
      } catch (failure) {
        if (active && !controller.signal.aborted && version === operationVersion.current) {
          setError(errorText(failure));
          if (failure instanceof ApiError && failure.status === 403) setLocalAccess(false);
        }
      } finally { loading = false; }
    };
    void load();
    const timer = setInterval(() => void load(), 2500);
    return () => { active = false; mounted.current = false; controller.abort(); clearInterval(timer); };
  }, [ownerId]);
  useEffect(() => {
    if (!expanded) return;
    const close = (event: KeyboardEvent) => { if (event.key === 'Escape') setExpanded(false); };
    window.addEventListener('keydown', close);
    return () => window.removeEventListener('keydown', close);
  }, [expanded]);

  const update = async (operation: () => Promise<ComputerStatus | ComputerObservation>, interrupt = false): Promise<boolean> => {
    if (operationPending.current && !interrupt) return false;
    const owner = ownerId;
    const version = ++operationVersion.current;
    operationPending.current = true;
    setBusy(true); setError(null);
    try {
      const value = await operation();
      if (mounted.current && owner === currentOwner.current && version === operationVersion.current) {
        setStatus('status' in value ? value.status : value);
        return true;
      }
    } catch (failure) {
      if (mounted.current && owner === currentOwner.current && version === operationVersion.current) {
        setError(errorText(failure));
        const latest = await request<ComputerStatus>(`${base}?owner=${encodeURIComponent(owner)}`).catch(() => null);
        if (latest && mounted.current && owner === currentOwner.current && version === operationVersion.current) setStatus(latest);
      }
    } finally {
      if (mounted.current && owner === currentOwner.current && version === operationVersion.current) {
        operationPending.current = false;
        setBusy(false);
      }
    }
    return false;
  };
  const perform = (action: ComputerAction) => {
    if (!status || status.state !== 'running' || !canControl) return Promise.resolve(false);
    return update(() => request<ComputerObservation>(`${base}/actions`, { method: 'POST', body: { ownerId, requestId: crypto.randomUUID(), epoch: status.epoch, frameId: status.frameId, action } }));
  };
  const start = () => {
    let allowed: string[];
    try {
      allowed = origin.split(/[\s,]+/).filter(Boolean).map(value => {
        const url = new URL(value);
        if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.pathname !== '/' || url.search || url.hash) throw Error('Invalid origin');
        return url.origin;
      });
      if (!allowed.length) throw Error('Missing origin');
    } catch { setError('https://example.com 형태로 경로·로그인 정보가 없는 사이트 주소를 입력하세요.'); return; }
    void update(() => request<ComputerStatus>(base, { method: 'POST', body: { ownerId, requestId: crypto.randomUUID(), userInitiated: true, origins: allowed } }));
  };
  const stop = () => { void update(() => request<ComputerStatus>(base, { method: 'DELETE', body: { ownerId } }), true); };
  const running = status?.state === 'running';
  const previewIdentity = status?.frameId ? `${status.epoch}:${status.frameId}` : null;
  const image = `${base}/preview?owner=${encodeURIComponent(ownerId)}&epoch=${encodeURIComponent(status?.epoch ?? '')}&frame=${encodeURIComponent(status?.frameId ?? '')}`;
  const stopped = status?.state === 'stopped';

  return <section className={`managed-computer ${expanded ? 'computer-expanded' : ''}`} aria-label="관리형 브라우저">
    <header hidden={!running && !expanded}><span><Globe2 size={14} />관리형 브라우저</span><div>{running && <IconButton label={expanded ? '브라우저 확대 닫기' : '브라우저 확대'} onClick={() => setExpanded(!expanded)}>{expanded ? <X size={15} /> : <Expand size={15} />}</IconButton>}</div></header>
    {!running ? <div className="computer-start">
      <div className="computer-preview-empty"><span className="computer-sleep-icon"><Avatar shape="blob" color="#929292" size={36} /><span>zᶻ</span></span><span>{status?.state === 'starting' || busy ? '브라우저 시작 중…' : !localAccess ? '이 서버의 로컬 연결에서 사용할 수 있어요' : '컴퓨터가 잠들어 있어요'}</span></div>
      <label><span>허용할 웹사이트</span><input aria-label="컴퓨터 허용 사이트" value={origin} onChange={event => setOrigin(event.target.value)} placeholder="https://example.com" disabled={busy || !localAccess} /></label>
      <button className="small-button" type="button" onClick={start} disabled={busy || !canControl || !localAccess || !stopped}>{busy ? <LoaderCircle className="spin" size={14} /> : <Play size={14} />}브라우저 시작</button>
      {(busy || status?.state === 'starting') && <button className="small-button" type="button" onClick={stop}><Square size={13} />브라우저 중지</button>}
      <p>서버에서 새 Chrome을 엽니다. 허용한 공개 사이트에 접근하며 로그인·다운로드·파일 업로드는 차단합니다. 서버의 localhost 연결에서 제어할 수 있습니다.</p>
    </div> : <>
      <form className="computer-address" onSubmit={event => { event.preventDefault(); void perform({ type: 'navigate', url: address }); }}><input aria-label="컴퓨터 웹 주소" value={address} onChange={event => setAddress(event.target.value)} disabled={busy || !canControl} /><button className="icon-button" title="주소로 이동" aria-label="주소로 이동" disabled={busy || !canControl}><ArrowRight size={15} /></button></form>
      <button type="button" className="computer-live-preview" aria-label="브라우저 화면 클릭" disabled={busy || !canControl || !status.frameId || loadedPreview !== previewIdentity} onClick={event => {
        const bounds = preview.current?.getBoundingClientRect();
        if (!bounds || !status || event.detail === 0) return;
        const scale = Math.min(bounds.width / status.viewport.width, bounds.height / status.viewport.height);
        const x = (event.clientX - bounds.left - (bounds.width - status.viewport.width * scale) / 2) / scale;
        const y = (event.clientY - bounds.top - (bounds.height - status.viewport.height * scale) / 2) / scale;
        if (x >= 0 && y >= 0 && x < status.viewport.width && y < status.viewport.height) void perform({ type: 'click', x: Math.floor(x), y: Math.floor(y) });
      }}>{status.frameId ? <img key={image} ref={preview} src={image} alt="서버 관리형 브라우저의 현재 화면" onLoad={() => setLoadedPreview(previewIdentity)} onError={() => { setLoadedPreview(null); setError('화면을 불러오지 못했습니다. 새로고침해 주세요.'); }} /> : <span className="computer-preview-empty">화면 새로고침으로 현재 페이지를 확인하세요.</span>}</button>
      <p className="computer-current-url" title={status.url ?? ''}>{status.url || '빈 페이지'}</p>
      <div className="computer-toolbar"><IconButton label="화면 새로고침" disabled={busy || !canControl} onClick={() => void perform({ type: 'observe' })}><RefreshCw size={14} /></IconButton><IconButton label="위로 스크롤" disabled={busy || !canControl} onClick={() => void perform({ type: 'scroll', deltaY: -400 })}><ArrowUp size={14} /></IconButton><IconButton label="아래로 스크롤" disabled={busy || !canControl} onClick={() => void perform({ type: 'scroll', deltaY: 400 })}><ArrowDown size={14} /></IconButton><button className="text-button" disabled={busy || !canControl} onClick={() => void perform({ type: 'key', key: 'Tab' })}>Tab</button><button className="text-button" disabled={busy || !canControl} onClick={() => void perform({ type: 'key', key: 'Enter' })}>Enter</button><IconButton label="브라우저 중지" onClick={stop}><Square size={13} /></IconButton></div>
      <form className="computer-type" onSubmit={event => { event.preventDefault(); const value = text; if (value) void perform({ type: 'type', text: value }).then(ok => { if (ok) setText(current => current === value ? '' : current); }); }}><input aria-label="컴퓨터 입력 텍스트" maxLength={2000} value={text} onChange={event => setText(event.target.value)} placeholder="선택한 입력란에 쓸 내용" disabled={busy || !canControl} /><button className="small-button" disabled={busy || !text || !canControl}>입력</button></form>
      {!canControl && <p className="computer-control-note">봇이 작업 중입니다. 수동 조작은 작업 중단 후 사용할 수 있습니다.</p>}
    </>}
    {error && <ErrorNotice onDismiss={() => setError(null)}>{error}</ErrorNotice>}
    {status?.error && !error && <ErrorNotice>{status.error}</ErrorNotice>}
    <small className="computer-privacy">개인 데스크톱과 분리된 브라우저입니다. 중지하면 쿠키와 브라우저 저장소를 닫습니다. 봇에게 페이지의 조작 요소와 주소를 전달할 때마다 승인받습니다.</small>
  </section>;
}
