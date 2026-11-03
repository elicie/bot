import { useEffect, useId, useRef, useState, type Dispatch, type FormEvent } from 'react';
import { Brain, Download, FileText, Plus, Trash2, Upload, X } from 'lucide-react';
import type { AppState, Bot } from '../shared/types';
import type { BotTemplate, KnowledgeDocument, KnowledgeFile, MemoryPreviewItem, TemplateImportResult } from '../shared/bot-data';
import { errorText, request } from './api';
import { type Action, isBotRunning, runningConversation } from './state';
import { ErrorNotice, IconButton, Modal, Spinner } from './ui';
import './bot-data.css';

const MAX_MEMORY_CHARACTERS = 500;
const MAX_KNOWLEDGE_BYTES = 512 * 1024;
const MAX_TEMPLATE_BYTES = 2 * 1024 * 1024;
const IMPORT_PROMPT = '지금까지 나에 대해 기억하는 사실과 선호를 한 줄에 하나씩 목록으로 정리해 줘. 설명 없이 목록만 보여 줘.';

const botPath = (bot: Bot) => `/api/bots/${encodeURIComponent(bot.id)}`;
const formatBytes = (bytes: number) => (bytes < 1024 ? `${bytes} B` : bytes < 1024 * 1024 ? `${(bytes / 1024).toFixed(1)} KB` : `${(bytes / 1024 / 1024).toFixed(1)} MB`);

async function readUtf8(file: File, limit: number) {
  if (file.size > limit) throw new Error(`${formatBytes(limit)}보다 작은 파일을 선택하세요.`);
  try {
    return new TextDecoder('utf-8', { fatal: true }).decode(await file.arrayBuffer()).replace(/^﻿/, '');
  } catch {
    throw new Error('UTF-8 텍스트 파일만 추가할 수 있어요.');
  }
}

/** Memory, knowledge, and template sections of the bot details pane. */
export function BotDataSections({ bot, data, dispatch, onBotCreated }: {
  bot: Bot; data: AppState; dispatch: Dispatch<Action>; onBotCreated: (botId: string) => void;
}) {
  const [pending, setPending] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState<string | null>(null);
  const [memoryDraft, setMemoryDraft] = useState('');
  const [dialog, setDialog] = useState<'import' | 'knowledge' | { preview: KnowledgeFile } | null>(null);
  const [includeMemories, setIncludeMemories] = useState(false);
  const lock = useRef(false);
  const templateInput = useRef<HTMLInputElement>(null);
  const prefix = useId();
  useEffect(() => { setError(null); setStatus(null); setMemoryDraft(''); setDialog(null); }, [bot.id]);
  const ownerRunning = isBotRunning(data, bot.id);
  const blocked = ownerRunning || pending !== null;
  const templateBlocked = !!runningConversation(data) || pending !== null;
  const memories = bot.memories ?? [];
  const files = bot.knowledge ?? [];

  const run = async <T,>(name: string, operation: () => Promise<T>) => {
    if (lock.current) return undefined;
    lock.current = true;
    setPending(name);
    setError(null);
    setStatus(null);
    try {
      return await operation();
    } catch (failure) {
      setError(errorText(failure));
      return undefined;
    } finally {
      lock.current = false;
      setPending(null);
    }
  };
  const addMemory = async (event: FormEvent) => {
    event.preventDefault();
    const text = memoryDraft.trim();
    if (!text || blocked) return;
    await run('memory', async () => {
      const result = await request<{ added: unknown[]; state: AppState }>(`${botPath(bot)}/memories`, { method: 'POST', body: { texts: [text] } });
      dispatch({ type: 'snapshot', data: result.state });
      setMemoryDraft('');
      setStatus(result.added.length ? '메모리를 추가했어요.' : '이미 기억하고 있는 내용이에요.');
    });
  };
  const forget = async (id: string, text: string) => {
    if (blocked || !window.confirm(`이 메모리를 삭제할까요?\n\n${text}`)) return;
    await run('forget', async () => {
      dispatch({ type: 'snapshot', data: await request<AppState>(`${botPath(bot)}/memories/${encodeURIComponent(id)}`, { method: 'DELETE', body: {} }) });
      setStatus('메모리를 삭제했어요.');
    });
  };
  const removeFile = async (file: KnowledgeFile) => {
    if (blocked || !window.confirm(`‘${file.name}’ 파일을 지식에서 삭제할까요?`)) return;
    await run('knowledge-delete', async () => {
      dispatch({ type: 'snapshot', data: await request<AppState>(`${botPath(bot)}/knowledge/${encodeURIComponent(file.id)}`, { method: 'DELETE', body: {} }) });
      setStatus('파일을 삭제했어요.');
    });
  };
  const exportTemplate = () => run('export', async () => {
    const template = await request<BotTemplate>(`${botPath(bot)}/template${includeMemories ? '?memories=true' : ''}`);
    const blob = new Blob([`${JSON.stringify(template, null, 2)}\n`], { type: 'application/json' });
    const link = document.createElement('a');
    link.href = URL.createObjectURL(blob);
    link.download = `${bot.name.replace(/[\\/:*?"<>|\s]+/g, '-') || 'bot'}.grok-bot-template.json`;
    link.click();
    setTimeout(() => URL.revokeObjectURL(link.href), 0);
    setStatus('템플릿을 내보냈어요.');
  });
  const importTemplate = (file: File) => templateBlocked ? undefined : run('template', async () => {
    const text = await readUtf8(file, MAX_TEMPLATE_BYTES);
    let template: unknown;
    try { template = JSON.parse(text); } catch { throw new Error('템플릿 JSON을 읽을 수 없어요. Grok Bot 템플릿 파일을 선택하세요.'); }
    const result = await request<TemplateImportResult>('/api/bot-templates', { method: 'POST', body: { template } });
    dispatch({ type: 'snapshot', data: result.state });
    onBotCreated(result.bot.id);
  });

  return <>
    {error && <div className="bot-data-feedback"><ErrorNotice onDismiss={() => setError(null)}>{error}</ErrorNotice></div>}
    {status && <p className="bot-data-feedback bot-data-status" role="status">{status}</p>}
    <section className="details-section bot-data-section" aria-labelledby={`${prefix}-memory`}>
      <header><h3 id={`${prefix}-memory`}>메모리</h3><button type="button" className="text-button" disabled={blocked} onClick={() => setDialog('import')}>가져오기</button></header>
      <p className="bot-data-help">이 Bot이 기억하는 내용</p>
      {memories.length ? <ul className="bot-data-list" aria-label={`${bot.name}의 메모리`}>
        {memories.map(memory => <li key={memory.id} className="bot-data-row">
          <Brain size={14} aria-hidden="true" /><span>{memory.text}</span>
          <IconButton label={`메모리 삭제: ${memory.text}`} disabled={blocked} onClick={() => void forget(memory.id, memory.text)}><Trash2 size={13} /></IconButton>
        </li>)}
      </ul> : <p className="detail-empty">아직 메모리가 없어요</p>}
      <form className="bot-data-add" onSubmit={event => void addMemory(event)}>
        <input aria-label="새 메모리" placeholder="기억할 내용" value={memoryDraft} maxLength={MAX_MEMORY_CHARACTERS} onChange={event => setMemoryDraft(event.target.value)} disabled={blocked} />
        <button type="submit" className="small-button" disabled={blocked || !memoryDraft.trim()}><Plus size={13} />{pending === 'memory' ? '추가 중…' : '메모리 추가'}</button>
      </form>
    </section>
    <section className="details-section bot-data-section" aria-labelledby={`${prefix}-knowledge`}>
      <header><h3 id={`${prefix}-knowledge`}>지식</h3><button type="button" className="text-button" disabled={blocked} onClick={() => setDialog('knowledge')}>파일 추가</button></header>
      <p className="bot-data-help">파일은 Bot에 공유 컨텍스트를 제공하고 팀의 지식을 활용해 답변하도록 돕습니다</p>
      {files.length ? <ul className="bot-data-list" aria-label={`${bot.name}의 지식 파일`}>
        {files.map(file => <li key={file.id} className="bot-data-row">
          <FileText size={14} aria-hidden="true" />
          <button type="button" className="bot-data-file" onClick={() => setDialog({ preview: file })} aria-label={`${file.name} 내용 보기`}>{file.name}<small>{formatBytes(file.bytes)}</small></button>
          <IconButton label={`파일 삭제: ${file.name}`} disabled={blocked} onClick={() => void removeFile(file)}><Trash2 size={13} /></IconButton>
        </li>)}
      </ul> : <p className="detail-empty">추가한 파일이 여기에 표시됩니다. UTF-8 텍스트 파일을 최대 512 KB까지 추가할 수 있어요.</p>}
    </section>
    <section className="details-section bot-data-section" aria-labelledby={`${prefix}-template`}>
      <header><h3 id={`${prefix}-template`}>Bot 템플릿</h3></header>
      <p className="bot-data-help">이름, 모양, 지침과 스킬을 JSON 파일로 내보내거나 템플릿 파일로 새 Bot을 만들어요. 지식 파일, 대화, 루틴은 포함되지 않아요.</p>
      <label className="bot-data-check"><input type="checkbox" checked={includeMemories} onChange={event => setIncludeMemories(event.target.checked)} />메모리 포함</label>
      <div className="bot-data-actions">
        <button type="button" className="small-button" disabled={pending !== null} onClick={() => void exportTemplate()}><Download size={13} />{pending === 'export' ? '내보내는 중…' : '템플릿 내보내기'}</button>
        <button type="button" className="small-button" disabled={templateBlocked} onClick={() => templateInput.current?.click()}><Upload size={13} />{pending === 'template' ? '가져오는 중…' : 'Bot 가져오기'}</button>
        <input ref={templateInput} type="file" hidden disabled={templateBlocked} accept=".json,application/json" aria-label="Bot 템플릿 파일 선택" onChange={event => { const file = event.currentTarget.files?.[0]; event.currentTarget.value = ''; if (file) void importTemplate(file); }} />
      </div>
    </section>
    {dialog === 'import' && <MemoryImport bot={bot} ownerRunning={ownerRunning} onClose={() => setDialog(null)} onSaved={(state, count) => { dispatch({ type: 'snapshot', data: state }); setDialog(null); setStatus(`메모리 ${count}개를 추가했어요.`); }} />}
    {dialog === 'knowledge' && <KnowledgeAdd bot={bot} ownerRunning={ownerRunning} onClose={() => setDialog(null)} onSaved={state => { dispatch({ type: 'snapshot', data: state }); setDialog(null); setStatus('파일을 지식에 추가했어요.'); }} />}
    {dialog && typeof dialog === 'object' && <KnowledgePreview botId={bot.id} file={dialog.preview} onClose={() => setDialog(null)} />}
  </>;
}

function MemoryImport({ bot, ownerRunning, onClose, onSaved }: { bot: Bot; ownerRunning: boolean; onClose: () => void; onSaved: (state: AppState, count: number) => void }) {
  const [text, setText] = useState('');
  const [items, setItems] = useState<(MemoryPreviewItem & { checked: boolean })[] | null>(null);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const selected = items?.filter(item => item.checked) ?? [];
  const preview = async () => {
    if (pending || ownerRunning) return;
    setPending(true); setError(null);
    try {
      const result = await request<{ memories: MemoryPreviewItem[] }>(`${botPath(bot)}/memories/preview`, { method: 'POST', body: { text } });
      setItems(result.memories.map(item => ({ ...item, checked: !item.duplicate && !item.error })));
    } catch (failure) { setError(errorText(failure)); } finally { setPending(false); }
  };
  const save = async () => {
    if (pending || ownerRunning) return;
    setPending(true); setError(null);
    try {
      const result = await request<{ added: unknown[]; state: AppState }>(`${botPath(bot)}/memories`, { method: 'POST', body: { texts: selected.map(item => item.text), imported: true } });
      onSaved(result.state, result.added.length);
    } catch (failure) { setError(errorText(failure)); setPending(false); }
  };
  const close = () => { if (!pending && (!text.trim() || window.confirm('붙여넣은 메모리를 버리고 닫을까요?'))) onClose(); };
  return <Modal title="Grok Bot으로 메모리 가져오기" onClose={close} className="compact-modal bot-data-modal">
    <header className="modal-header"><h2>Grok Bot으로 메모리 가져오기</h2><IconButton label="메모리 가져오기 닫기" onClick={close} disabled={pending}><X size={18} /></IconButton></header>
    <div className="modal-body bot-data-modal-body">
      {items === null ? <>
        <p className="bot-data-step">1. 다른 AI 어시스턴트에 이 요청을 보내세요.</p>
        <div className="bot-data-prompt"><span>{IMPORT_PROMPT}</span><button type="button" className="small-button" onClick={() => { void navigator.clipboard?.writeText(IMPORT_PROMPT); setCopied(true); }}>{copied ? '복사됨' : '복사'}</button></div>
        <p className="bot-data-step">2. 결과를 아래에 붙여넣어 Grok Bot의 메모리에 추가하세요</p>
        <textarea aria-label="가져올 메모리" placeholder="메모리 내용을 여기에 붙여넣으세요" rows={9} value={text} onChange={event => setText(event.target.value)} data-autofocus disabled={pending || ownerRunning} />
        <small className="bot-data-help">한 줄이나 목록 항목 하나가 메모리 하나가 됩니다. 저장하기 전에 항목을 확인할 수 있어요.</small>
      </> : items.length === 0 ? <p className="detail-empty">추가할 항목이 없어요</p> : <>
        <p className="bot-data-step">{bot.name}에 추가할 항목을 선택하세요.</p>
        <ul className="bot-data-review" aria-label="가져올 메모리 항목">
          {items.map((item, index) => <li key={index}>
            <label>
              <input type="checkbox" checked={item.checked} disabled={!!item.error || pending || ownerRunning} onChange={event => setItems(items.map((other, position) => position === index ? { ...other, checked: event.target.checked } : other))} />
              <span>{item.text}{item.duplicate && <small>이미 있음</small>}{item.error && <small className="bot-data-invalid">{item.text.length > MAX_MEMORY_CHARACTERS ? `${MAX_MEMORY_CHARACTERS}자를 넘어 추가할 수 없어요` : '추가할 수 없는 문자가 있어요'}</small>}</span>
            </label>
          </li>)}
        </ul>
      </>}
      {error && <ErrorNotice>{error}</ErrorNotice>}
    </div>
    <footer className="modal-footer">
      {items === null ? <>
        <button type="button" className="small-button" onClick={close} disabled={pending}>취소</button>
        <button type="button" className="primary-button" onClick={() => void preview()} disabled={pending || ownerRunning || !text.trim()}>{pending ? '확인 중…' : '항목 확인'}</button>
      </> : <>
        <button type="button" className="small-button" onClick={() => setItems(null)} disabled={pending}>뒤로</button>
        <button type="button" className="primary-button" onClick={() => void save()} disabled={pending || ownerRunning || !selected.length}>{pending ? '추가 중…' : `메모리에 추가 (${selected.length})`}</button>
      </>}
    </footer>
  </Modal>;
}

function KnowledgeAdd({ bot, ownerRunning, onClose, onSaved }: { bot: Bot; ownerRunning: boolean; onClose: () => void; onSaved: (state: AppState) => void }) {
  const [name, setName] = useState('');
  const [content, setContent] = useState('');
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  const bytes = new TextEncoder().encode(content).length;
  const choose = async (file: File) => {
    if (pending || ownerRunning) return;
    setError(null);
    try {
      setContent(await readUtf8(file, MAX_KNOWLEDGE_BYTES));
      setName(file.name);
    } catch (failure) { setError(errorText(failure)); }
  };
  const save = async (event: FormEvent) => {
    event.preventDefault();
    if (pending || ownerRunning) return;
    setPending(true); setError(null);
    try {
      onSaved(await request<AppState>(`${botPath(bot)}/knowledge`, { method: 'POST', body: { name: name.trim(), content } }));
    } catch (failure) { setError(errorText(failure)); setPending(false); }
  };
  const close = () => { if (!pending && (!content.trim() || window.confirm('추가하지 않은 파일 내용을 버리고 닫을까요?'))) onClose(); };
  return <Modal title="지식 파일 추가" onClose={close} className="compact-modal bot-data-modal">
    <header className="modal-header"><h2>파일 추가</h2><IconButton label="파일 추가 닫기" onClick={close} disabled={pending}><X size={18} /></IconButton></header>
    <form onSubmit={event => void save(event)}>
      <div className="modal-body bot-data-modal-body">
        <p className="bot-data-help">파일은 Bot에 공유 컨텍스트를 제공하고 팀의 지식을 활용해 답변하도록 돕습니다. 이 Bot의 대화에서만 읽을 수 있어요.</p>
        <button type="button" className="small-button" onClick={() => fileInput.current?.click()} disabled={pending || ownerRunning}><Upload size={13} />파일 업로드</button>
        <input ref={fileInput} type="file" hidden disabled={pending || ownerRunning} accept=".txt,.md,.markdown,.csv,.json,.yaml,.yml,.html,.xml,text/*" aria-label="지식 파일 선택" onChange={event => { const file = event.currentTarget.files?.[0]; event.currentTarget.value = ''; if (file) void choose(file); }} />
        <label className="bot-data-field"><span>파일 이름</span><input aria-label="파일 이름" value={name} maxLength={200} onChange={event => setName(event.target.value)} disabled={pending || ownerRunning} data-autofocus /></label>
        <label className="bot-data-field"><span>내용</span><textarea aria-label="파일 내용" rows={10} value={content} onChange={event => setContent(event.target.value)} disabled={pending || ownerRunning} placeholder="텍스트를 붙여넣거나 파일을 업로드하세요" spellCheck={false} /></label>
        <small className={`bot-data-help ${bytes > MAX_KNOWLEDGE_BYTES ? 'bot-data-invalid' : ''}`}>{formatBytes(bytes)} / 512 KB · UTF-8 텍스트</small>
        {error && <ErrorNotice>{error}</ErrorNotice>}
      </div>
      <footer className="modal-footer">
        <button type="button" className="small-button" onClick={close} disabled={pending}>취소</button>
        <button type="submit" className="primary-button" disabled={pending || ownerRunning || !name.trim() || !content.trim() || bytes > MAX_KNOWLEDGE_BYTES}>{pending ? '추가 중…' : '추가'}</button>
      </footer>
    </form>
  </Modal>;
}

function KnowledgePreview({ botId, file, onClose }: { botId: string; file: KnowledgeFile; onClose: () => void }) {
  const [document, setDocument] = useState<KnowledgeDocument | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    const controller = new AbortController();
    request<KnowledgeDocument>(`/api/bots/${encodeURIComponent(botId)}/knowledge/${encodeURIComponent(file.id)}`, { signal: controller.signal })
      .then(setDocument)
      .catch(failure => { if (!controller.signal.aborted) setError(errorText(failure)); });
    return () => controller.abort();
  }, [botId, file.id]);
  return <Modal title={`${file.name} 내용`} onClose={onClose} className="compact-modal bot-data-modal">
    <header className="modal-header"><h2><FileText size={16} />{file.name}</h2><IconButton label="파일 내용 닫기" onClick={onClose}><X size={18} /></IconButton></header>
    <div className="modal-body bot-data-modal-body">
      <small className="bot-data-help">{formatBytes(file.bytes)} · 읽기 전용</small>
      {error ? <ErrorNotice>{error}</ErrorNotice> : document ? <pre className="bot-data-content">{document.content}</pre> : <Spinner />}
    </div>
  </Modal>;
}
