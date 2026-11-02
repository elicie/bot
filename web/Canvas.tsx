// Operate extension: conversation-owned text beside the existing chat, not another workspace file.
// Inherit the app's neutral surfaces, compact controls and mobile full-screen side-panel pattern.
// Keep the saved revision separate from local edits; a remote save never silently replaces a draft.
import { useEffect, useRef, useState, type ReactNode } from 'react';
import { Download, Eye, FileCode2, FolderOutput, History, MessageSquare, Save, Undo2, X } from 'lucide-react';
import Markdown from 'react-markdown';
import type { AppState, CanvasVersion, Conversation } from '../shared/types';
import { ApiError, errorText, request } from './api';
import { canvasDirty, canvasDraft, canvasExportRequest, canvasFilename, canvasStale, reconcileCanvas } from './canvas-state';
import { ErrorNotice, IconButton } from './ui';
import './canvas.css';

export interface CanvasDraftStatus { dirty: boolean; saving: boolean }
export function CanvasButton({ open, onClick, disabled = false }: { open: boolean; onClick: () => void; disabled?: boolean }) {
  return <IconButton label={open ? '캔버스 닫기' : '캔버스 열기'} aria-expanded={open} className={open ? 'active' : ''} onClick={onClick} disabled={disabled}><FileCode2 size={17} /></IconButton>;
}
const languages = [['markdown', '마크다운'], ['text', '일반 텍스트'], ['javascript', 'JavaScript'], ['typescript', 'TypeScript'], ['python', 'Python'], ['rust', 'Rust'], ['html', 'HTML'], ['css', 'CSS'], ['json', 'JSON']] as const;
const sources = { user: '내 편집', bot: 'Bot 수정', restore: '되돌리기' };
const byteLength = (text: string) => new TextEncoder().encode(text).length;

/** A key per conversation makes switching ownership explicit and leaves unrelated SSE updates alone. */
export function CanvasPanel({ conversation, active, online, onSnapshot, onClose, onViewChat, onDraftStatus, onExportRequest }: {
  conversation: Conversation; active: boolean; online: boolean; onSnapshot: (snapshot: AppState) => void;
  onClose: () => void; onViewChat: () => void; onDraftStatus: (status: CanvasDraftStatus) => void; onExportRequest: (text: string) => void;
}) {
  const latest = conversation.canvas;
  const [draft, setDraft] = useState(() => canvasDraft(latest));
  const [saving, setSaving] = useState(false);
  const savingRef = useRef(false);
  const closeRef = useRef(onClose);
  closeRef.current = onClose;
  const [error, setError] = useState<string | null>(null);
  const [preview, setPreview] = useState(false);
  const [history, setHistory] = useState(false);
  const [version, setVersion] = useState<number | null>(null);
  const editor = useRef<HTMLTextAreaElement>(null);
  const panel = useRef<HTMLElement>(null);
  const dirty = canvasDirty(draft);
  const conflict = canvasStale(draft, latest);
  const readOnly = !!latest?.redacted;
  const historical = version === null ? undefined : latest?.history.find(item => item.revision === version);
  const display = historical ?? (readOnly && latest ? latest : draft);
  const bytes = byteLength(display.content);
  const valid = draft.title.trim().length > 0 && bytes <= 128 * 1024 && !draft.content.includes('\0');
  const locked = saving || !online || readOnly;
  useEffect(() => {
    if (!saving) setDraft(current => reconcileCanvas(current, latest));
  }, [latest, saving]);
  useEffect(() => { onDraftStatus({ dirty, saving }); }, [dirty, saving, onDraftStatus]);
  useEffect(() => () => onDraftStatus({ dirty: false, saving: false }), [onDraftStatus]);
  useEffect(() => {
    if (!active) return;
    const previous = document.activeElement;
    editor.current?.focus();
    return () => { if (previous instanceof HTMLElement && previous.isConnected) previous.focus(); };
  }, [active]);
  useEffect(() => {
    if (!active) return;
    const keyboard = (event: KeyboardEvent) => {
      if (event.key === 'Escape' && !savingRef.current && !document.querySelector('dialog[open]')) {
        event.preventDefault(); closeRef.current();
      }
    };
    document.addEventListener('keydown', keyboard);
    return () => document.removeEventListener('keydown', keyboard);
  }, [active]);
  const reload = () => {
    if (savingRef.current || (dirty && !window.confirm('내 초안을 버리고 최신 캔버스를 다시 열까요?'))) return;
    setDraft(canvasDraft(latest)); setVersion(null); setError(null);
  };
  const mutate = async (body: unknown, restore = false) => {
    if (savingRef.current || !online || readOnly) return;
    savingRef.current = true; setSaving(true); setError(null);
    try {
      const result = await request<AppState>(`/api/conversations/${encodeURIComponent(conversation.id)}/canvas${restore ? '/restore' : ''}`, { method: restore ? 'POST' : 'PUT', body });
      const saved = result.conversations.find(item => item.id === conversation.id)?.canvas;
      if (!saved) throw new Error('저장한 캔버스를 확인하지 못했습니다. 초안은 유지됩니다.');
      onSnapshot(result); setDraft(canvasDraft(saved)); setVersion(null);
    } catch (cause) {
      setError(cause instanceof ApiError && cause.status === 409
        ? '캔버스가 다른 곳에서 변경되어 저장하지 않았습니다. 내 초안은 유지됩니다. 내용을 복사한 뒤 최신 버전을 다시 열어 주세요.'
        : errorText(cause));
    } finally { savingRef.current = false; setSaving(false); }
  };
  const save = () => {
    if (!valid || conflict || locked || historical || (latest && !dirty)) return;
    void mutate({ revision: draft.base.revision, title: draft.title, language: draft.language, content: draft.content });
  };
  const restore = (target: CanvasVersion) => {
    if (!latest || dirty || locked || conflict || target.redacted) return;
    void mutate({ revision: latest.revision, targetRevision: target.revision }, true);
  };
  const download = () => {
    if (!latest || dirty || conflict || saving || readOnly) return;
    const url = URL.createObjectURL(new Blob([latest.content], { type: 'text/plain;charset=utf-8' }));
    const anchor = document.createElement('a'); anchor.href = url; anchor.download = canvasFilename(latest);
    anchor.click(); setTimeout(() => URL.revokeObjectURL(url), 1000);
  };
  const close = () => { if (!savingRef.current) onClose(); };
  const last = latest?.history.at(-1);
  const note: ReactNode = historical ? `버전 ${historical.revision} · ${sources[historical.source]} · 읽기 전용`
    : saving ? '저장 중…' : dirty ? '저장하지 않은 변경 사항' : latest ? `버전 ${latest.revision} · 저장됨` : '저장하면 이 대화에 보관됩니다';
  return <aside ref={panel} className="canvas-panel" aria-label="대화 캔버스" aria-busy={saving} onKeyDown={event => {
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 's') { event.preventDefault(); save(); }
  }}>
    <header className="canvas-header">
      <span><FileCode2 size={17} />캔버스</span>
      <IconButton label="대화로 돌아가기" onClick={onViewChat} disabled={saving}><MessageSquare size={17} /></IconButton>
      <IconButton label="캔버스 닫기" onClick={close} disabled={saving}><X size={18} /></IconButton>
    </header>
    <div className="canvas-metadata">
      <label><span>문서 제목</span><input aria-label="캔버스 제목" maxLength={100} value={display.title} onChange={event => setDraft(current => ({ ...current, title: event.target.value }))} readOnly={!!historical || readOnly || saving} /></label>
      <label><span>형식</span><select aria-label="캔버스 형식" value={display.language} disabled={saving || !!historical || readOnly} onChange={event => { setDraft(current => ({ ...current, language: event.target.value })); setPreview(false); }}>
        {!languages.some(([language]) => language === (display.language)) && <option value={display.language}>{display.language}</option>}
        {languages.map(([value, label]) => <option key={value} value={value}>{label}</option>)}
      </select></label>
    </div>
    <div className="canvas-toolbar" role="group" aria-label="캔버스 작업">
      <button type="button" className="small-button" aria-label="캔버스 저장" onClick={save} disabled={locked || !valid || conflict || !!historical || (!!latest && !dirty)}><Save size={14} />저장</button>
      <IconButton label="이전 버전으로 되돌리기" disabled={!last || locked || dirty || conflict || !!last.redacted} onClick={() => last && restore(last)}><Undo2 size={15} /></IconButton>
      <IconButton label="캔버스 버전 기록" aria-expanded={history} className={history ? 'active' : ''} disabled={!latest} onClick={() => setHistory(value => !value)}><History size={15} /></IconButton>
      <IconButton label="마크다운 미리보기" aria-pressed={preview} className={preview ? 'active' : ''} disabled={(display.language) !== 'markdown'} onClick={() => setPreview(value => !value)}><Eye size={15} /></IconButton>
      <span className="canvas-toolbar-spacer" />
      <IconButton label="캔버스 파일로 다운로드" onClick={download} disabled={!latest || dirty || conflict || saving || readOnly}><Download size={15} /></IconButton>
      <IconButton label="작업 폴더에 저장 요청" onClick={() => latest && onExportRequest(canvasExportRequest(latest))} disabled={!latest || dirty || conflict || locked}><FolderOutput size={15} /></IconButton>
    </div>
    {history && latest && <div className="canvas-history">
      <label>버전 기록<select aria-label="캔버스 버전" value={historical?.revision ?? ''} onChange={event => setVersion(event.target.value ? Number(event.target.value) : null)}>
        <option value="">현재 · 버전 {latest.revision}</option>
        {[...latest.history].reverse().map(item => <option value={item.revision} key={item.revision}>버전 {item.revision} · {sources[item.source]} · {new Date(item.updatedAt).toLocaleString('ko-KR')}</option>)}
      </select></label>
      {historical && <button type="button" className="small-button" disabled={locked || dirty || conflict || historical.redacted} onClick={() => restore(historical)}>이 버전 복원</button>}
      <small>최근 이전 버전 20개를 보관합니다. 복원하면 새 버전으로 저장됩니다.</small>
    </div>}
    {error && <ErrorNotice>{error}</ErrorNotice>}
    {conflict && <div className="canvas-conflict" role="status"><p>저장된 내용이나 보호 설정이 변경되었습니다. 내 초안은 유지됩니다.</p><button type="button" className="small-button" onClick={reload} disabled={saving}>최신 버전 다시 열기</button></div>}
    {readOnly && <p className="canvas-notice" role="status">비밀 값이 가려진 문서는 안전을 위해 읽기 전용입니다.</p>}
    {!online && <p className="canvas-notice" role="status">서버에 다시 연결한 뒤 저장할 수 있습니다. 초안은 유지됩니다.</p>}
    {conversation.run.permission && <button type="button" className="canvas-approval-link" onClick={onViewChat}>대화의 승인 요청 확인하기</button>}
    {preview && (display.language) === 'markdown'
      ? <div className="canvas-preview markdown" aria-label="캔버스 미리보기"><Markdown skipHtml components={{ img: ({ alt }) => <span>[이미지{alt ? ` · ${alt}` : ''}]</span>, a: ({ href, children }) => <a href={href} target="_blank" rel="noopener noreferrer">{children}</a> }}>{display.content}</Markdown></div>
      : <textarea ref={editor} className={`canvas-editor ${((display.language) === 'markdown' || (display.language) === 'text') ? 'canvas-prose' : ''}`} aria-label="캔버스 내용" placeholder="문서나 코드를 작성하세요. 저장한 내용은 이 대화의 Bot도 읽고 수정할 수 있습니다." value={display.content} spellCheck={(display.language) === 'markdown'} readOnly={!!historical || readOnly || saving} onChange={event => setDraft(current => ({ ...current, content: event.target.value }))} />}
    <footer className="canvas-footer"><span role="status">{note}</span><span className={bytes > 128 * 1024 ? 'canvas-over-limit' : ''}>{Math.ceil(bytes / 1024)} / 128 KiB</span></footer>
    <p className="canvas-footnote">Bot 수정은 한 번씩 승인합니다. 작업 폴더 저장은 대화에서 요청하며 기존 파일 승인을 사용합니다.</p>
  </aside>;
}
