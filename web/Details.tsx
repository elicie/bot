import { useEffect, useId, useRef, useState, type Dispatch, type ReactNode } from 'react';
import { ChevronRight, FileText, FolderOpen, Monitor, Pencil, Repeat2, X } from 'lucide-react';
import type { AppState, Bot, Conversation } from '../shared/types';
import { errorText, readDocument } from './api';
import { type Action, type FilesState, isDirty } from './state';
import { Avatar, IconButton } from './ui';
import { Files } from './Files';
import { BotDataSections } from './BotData';
import './workspace.css';

export type DetailsTab = 'details' | 'library' | 'computer';
export function Details({ bot, conversation, data, files, dispatch, tab, onTab, onClose, onEdit, onSettings, onRoutines, onBotCreated, computer }: {
  bot: Bot; conversation?: Conversation; data: AppState; files: FilesState; dispatch: Dispatch<Action>;
  tab: DetailsTab; onTab: (tab: DetailsTab) => void; onClose: () => void; onEdit: () => void;
  onSettings: () => void; onRoutines: () => void; onBotCreated: (botId: string) => void; computer?: ReactNode;
}) {
  const [width, setWidth] = useState(320);
  const requestPrefix = useId();
  const requestSequence = useRef(0);
  const workspaceRef = useRef(data.settings.workspace);
  workspaceRef.current = data.settings.workspace;
  useEffect(() => () => { requestSequence.current++; }, []);
  const routines = data.routines.filter(routine => routine.botId === bot.id);
  const paths = [...new Set(conversation?.messages.flatMap(message => message.tools.flatMap(tool => tool.paths ?? [])) ?? [])];
  const changeTab = (next: DetailsTab) => {
    if (next === tab) return;
    if (files.editor.kind === 'ready' && files.editor.save === 'saving') return;
    if (isDirty(files.editor) && !window.confirm('저장하지 않은 파일 변경 사항을 버리고 이동할까요?')) return;
    if (isDirty(files.editor)) dispatch({ type: 'close-file' });
    onTab(next);
  };
  const openPath = async (path: string) => {
    if (files.editor.kind === 'ready' && files.editor.save === 'saving') return;
    if (isDirty(files.editor) && !window.confirm('저장하지 않은 파일 변경 사항을 버리고 파일을 열까요?')) return;
    const root = data.settings.workspace;
    const sequence = ++requestSequence.current;
    const requestId = `${requestPrefix}-${sequence}`;
    onTab('computer');
    dispatch({ type: 'directory', path: path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : '' });
    dispatch({ type: 'open-file', path, requestId });
    try {
      const document = await readDocument(path);
      if (workspaceRef.current === root && requestSequence.current === sequence) dispatch({ type: 'file-loaded', document, requestId });
    } catch (error) {
      if (workspaceRef.current === root && requestSequence.current === sequence) dispatch({ type: 'file-error', path, error: errorText(error), requestId });
    }
  };
  return <aside className="details-panel" aria-label="대화 세부 정보" style={{ width, flexBasis: width }}>
    <div className="details-resizer desktop-only" role="separator" tabIndex={0} aria-label="세부 정보 크기 조정" aria-orientation="vertical" aria-valuenow={width} aria-valuemin={280} aria-valuemax={480}
      onKeyDown={event => { if (event.key === 'ArrowLeft' || event.key === 'ArrowRight') { event.preventDefault(); setWidth(Math.max(280, Math.min(480, width + (event.key === 'ArrowLeft' ? 10 : -10)))); } }}
      onPointerDown={event => { event.preventDefault(); const start = event.clientX; const original = width; const target = event.currentTarget; target.setPointerCapture(event.pointerId); const move = (next: PointerEvent) => setWidth(Math.max(280, Math.min(480, original + start - next.clientX))); const end = () => { target.removeEventListener('pointermove', move); target.removeEventListener('pointerup', end); target.removeEventListener('pointercancel', end); }; target.addEventListener('pointermove', move); target.addEventListener('pointerup', end); target.addEventListener('pointercancel', end); }} />
    <header className="details-header"><span>대화 세부 정보</span><IconButton label="컴퓨터 닫기" onClick={onClose}><X size={18} /></IconButton></header>
    <div className="details-profile"><button className="profile-edit-avatar" onClick={onEdit} aria-label="Bot 아바타 편집"><Avatar color={bot.color} seed={bot.id} shape={bot.shape} size={64} /></button><button className="details-name" onClick={onEdit} aria-label="봇 편집">{bot.name}<Pencil size={12} /></button></div>
    <div className="details-tabs" role="tablist" aria-label="대화 세부 정보 탭">
      {([{ id: 'details', label: '세부 정보' }, { id: 'library', label: '라이브러리' }, { id: 'computer', label: '컴퓨터' }] as const).map(item => <button key={item.id} role="tab" type="button" aria-selected={tab === item.id} aria-controls={`details-${item.id}`} id={`tab-${item.id}`} onClick={() => changeTab(item.id)}>{item.label}</button>)}
    </div>
    <div className={`details-content details-${tab}`} role="tabpanel" id={`details-${tab}`} aria-labelledby={`tab-${tab}`}>
      {tab === 'details' && <>
        {bot.instructions && <section className="details-section"><h3>Bot 지침</h3><p className="bot-instructions">{bot.instructions}</p><button className="text-button" onClick={onEdit}>지침 편집</button></section>}
        <section className="details-section"><header><h3>루틴</h3><button className="text-button" onClick={onRoutines}>관리<ChevronRight size={12} /></button></header>
          {!routines.length ? <p className="detail-empty">루틴은 이 Bot이 일정에 따라 실행하는 반복 작업입니다. 루틴을 추가해 작업을 예약해 보세요.</p> : routines.map(routine => <button className="detail-list-row" key={routine.id} onClick={onRoutines}><Repeat2 size={15} /><span>{routine.name}<small>{routine.enabled ? '예약됨' : '일시정지'}</small></span><ChevronRight size={14} /></button>)}
          <button className="small-button routine-add" onClick={onRoutines}><Repeat2 size={14} />루틴 추가</button>
        </section>
        <BotDataSections bot={bot} data={data} dispatch={dispatch} onBotCreated={onBotCreated} />
        <section className="details-section"><h3>대화</h3><dl className="details-values"><div><dt>모델</dt><dd>{data.settings.model || '미설정'}</dd></div><div><dt>파일 접근</dt><dd>{data.settings.workspace ? '작업 폴더 연결됨' : '폴더 미연결'}</dd></div></dl></section>
      </>}
      {tab === 'library' && <section className="details-section"><header><h3>대화에서 사용한 파일</h3><button className="text-button" onClick={() => changeTab('computer')}>파일 열기</button></header>
        {paths.length ? <div className="library-file-list">{paths.map(path => <button className="detail-list-row" key={path} onClick={() => void openPath(path)}><FileText size={17} /><span>{path}</span><ChevronRight size={13} /></button>)}</div> : <div className="detail-empty"><FileText size={24} /><p>이 대화에서 사용한 파일이 여기에 표시됩니다.</p></div>}
        <button className="small-button" onClick={() => changeTab('computer')}><FolderOpen size={14} />작업 폴더 탐색</button>
      </section>}
      {tab === 'computer' && <>
        {computer}
        <div className="computer-scope"><Monitor size={13} /><span>서버 컴퓨터 · {window.location.host}</span></div>
        <Files embedded workspace={data.settings.workspace} files={files} dispatch={dispatch} onClose={onClose} onSettings={onSettings} />
      </>}
    </div>
  </aside>;
}
