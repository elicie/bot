import { useEffect, useRef, useState, type ReactNode } from 'react';
import { Blocks, Eye, EyeOff, MoreHorizontal, PanelLeftClose, PanelLeftOpen, Pin, Plus, Repeat2, Search, Settings2, X } from 'lucide-react';
import type { AppState, Bot } from '../shared/types';
import { activeRun, conversationPreview, conversationsFor, type Selection } from './state';
import { Avatar, IconButton, Spinner } from './ui';
import './workspace.css';

export function Sidebar({ data, selection, lastConversation, chat, search, open, mobile, busy, onSearch, onSelect, onClose, onNew, onSettings, onRoutines, onCapabilities, onPatch, menuExtras }: {
  data: AppState | null; selection: Selection; lastConversation: Record<string, string | null>; chat: boolean;
  search: string; open: boolean; mobile: boolean; busy: boolean;
  onSearch: (value: string) => void; onSelect: (selection: Selection) => void; onClose: () => void;
  onNew: () => void; onSettings: () => void; onRoutines: () => void; onCapabilities: () => void;
  onPatch: (id: string, patch: Partial<Pick<Bot, 'pinned' | 'hidden'>>) => void;
  /** Extra context-menu items (read state, notifications, delete). */
  menuExtras?: (bot: Bot, close: () => void) => ReactNode;
}) {
  const [compact, setCompact] = useState(() => localStorage.getItem('grok-bot-sidebar-compact') === 'true');
  const [hidden, showHidden] = useState(false);
  const [searchOpen, setSearchOpen] = useState(false);
  const [menu, setMenu] = useState<string | null>(null);
  const [width, setWidth] = useState(() => Math.min(400, Math.max(240, Number(localStorage.getItem('grok-bot-sidebar-width')) || 280)));
  const aside = useRef<HTMLElement>(null);
  const searchInput = useRef<HTMLInputElement>(null);
  useEffect(() => { localStorage.setItem('grok-bot-sidebar-compact', String(compact)); }, [compact]);
  useEffect(() => { localStorage.setItem('grok-bot-sidebar-width', String(width)); }, [width]);
  useEffect(() => {
    if (!menu) return;
    const close = (event: MouseEvent) => { if (!(event.target as Element).closest('.bot-actions')) setMenu(null); };
    const escape = (event: KeyboardEvent) => { if (event.key === 'Escape') setMenu(null); };
    document.addEventListener('click', close); document.addEventListener('keydown', escape);
    return () => { document.removeEventListener('click', close); document.removeEventListener('keydown', escape); };
  }, [menu]);
  const bots = (data?.bots ?? []).filter(bot => (hidden ? bot.hidden : !bot.hidden) && bot.name.toLocaleLowerCase().includes(search.toLocaleLowerCase()));
  const pinned = bots.filter(bot => bot.pinned ?? bot.id === data?.bots[0]?.id);
  const normal = bots.filter(bot => !pinned.includes(bot));
  const closeSearch = () => { onSearch(''); setSearchOpen(false); aside.current?.querySelector<HTMLButtonElement>('button[aria-label="검색"]')?.focus(); };
  const selectBot = (bot: Bot) => onSelect({ botId: bot.id, conversationId: selection.botId === bot.id ? selection.conversationId : Object.hasOwn(lastConversation, bot.id) ? lastConversation[bot.id] : conversationsFor(data, bot.id)[0]?.id ?? null });
  const row = (bot: Bot, tile: boolean) => {
    const latest = conversationsFor(data, bot.id)[0];
    const running = data?.conversations.some(conversation => conversation.botId === bot.id && activeRun(conversation.run.status));
    const unread = !!bot.unread && !(chat && bot.id === selection.botId);
    return <div className={`bot-item ${tile ? 'pinned-item' : ''}`} key={bot.id}>
      {unread && <span id={`unread-${bot.id}`} hidden>읽지 않은 활동</span>}
      <button type="button" className={`bot-row ${tile ? 'bot-tile' : ''} ${chat && bot.id === selection.botId ? 'selected' : ''} ${unread ? 'is-unread' : ''}`} aria-current={chat && bot.id === selection.botId ? 'true' : undefined} title={bot.name}
        aria-describedby={unread ? `unread-${bot.id}` : undefined} data-unread={unread ? 'true' : undefined}
        onClick={() => selectBot(bot)} onContextMenu={event => { event.preventDefault(); setMenu(bot.id); }}>
        <span className="bot-avatar"><Avatar color={bot.color} seed={bot.id} shape={bot.shape} size={tile ? 64 : 36} />{running && <span className="running-dot" />}{unread && !running && <span className="unread-dot" aria-hidden="true" />}</span>
        <span className="bot-copy"><span>{bot.name}</span>{!tile && <small>{conversationPreview(latest)}</small>}</span>
      </button>
      <div className="bot-actions">
        <IconButton label={`추가 작업: ${bot.name}`} onClick={() => setMenu(menu === bot.id ? null : bot.id)}><MoreHorizontal size={15} /></IconButton>
        {menu === bot.id && <div className="bot-context" role="menu" aria-label={`${bot.name} 작업`}>
          <button role="menuitem" disabled={busy} onClick={() => { onPatch(bot.id, { pinned: !(bot.pinned ?? bot.id === data?.bots[0]?.id) }); setMenu(null); }}><Pin size={14} />{(bot.pinned ?? bot.id === data?.bots[0]?.id) ? '고정 해제' : '고정'}</button>
          <button role="menuitem" disabled={busy} onClick={() => { onPatch(bot.id, { hidden: !bot.hidden }); setMenu(null); }}>{bot.hidden ? <Eye size={14} /> : <EyeOff size={14} />}{bot.hidden ? '사이드바에 표시' : '사이드바에서 숨기기'}</button>
          <button role="menuitem" onClick={() => { void navigator.clipboard?.writeText(bot.id); setMenu(null); }}>Bot ID 복사</button>
          {menuExtras?.(bot, () => setMenu(null))}
        </div>}
      </div>
    </div>;
  };
  return <aside ref={aside} className={`sidebar reference-sidebar ${open ? 'is-open' : ''} ${compact && !mobile ? 'is-compact' : ''} ${searchOpen || search ? 'search-is-open' : ''}`} style={{ width: compact && !mobile ? 88 : width, flexBasis: compact && !mobile ? 88 : width }} aria-label="봇 목록" inert={mobile && !open}>
    <header className="sidebar-header">
      <span className="app-title">Grok Bot</span>
      <div>
        <IconButton label="컴팩트 사이드바 전환" className="desktop-only" onClick={() => setCompact(!compact)}>{compact ? <PanelLeftOpen size={17} /> : <PanelLeftClose size={17} />}</IconButton>
        <IconButton label="검색" aria-expanded={searchOpen || !!search} onClick={() => { setCompact(false); setSearchOpen(true); setTimeout(() => searchInput.current?.focus(), 0); }}><Search size={18} /></IconButton>
        <IconButton label="봇 추가" onClick={onNew} disabled={!data || busy}><Plus size={20} /></IconButton>
        <IconButton label="사이드바 닫기" className="mobile-only" onClick={onClose}><X size={18} /></IconButton>
      </div>
    </header>
    <label className="search-box"><Search size={14} /><input ref={searchInput} aria-label="봇 검색" placeholder="Bot 검색" value={search} onChange={event => onSearch(event.target.value)} onKeyDown={event => { if (event.key === 'Escape') closeSearch(); }} />{search && <IconButton label="검색 지우기" onClick={closeSearch}><X size={12} /></IconButton>}</label>
    <div className="bot-list">
      {!data ? <Spinner /> : <>
        {hidden && <div className="sidebar-section-heading">숨긴 Bot<button onClick={() => showHidden(false)}>돌아가기</button></div>}
        {!!pinned.length && <section className="pinned-bots" aria-label="고정된 봇">{pinned.map(bot => row(bot, true))}</section>}
        <div className="ordinary-bots">{normal.map(bot => row(bot, false))}</div>
        {!bots.length && <p className="sidebar-empty">{search ? '검색 결과가 없습니다.' : hidden ? '숨긴 Bot이 없습니다.' : '봇을 추가해 대화를 시작하세요.'}</p>}
      </>}
    </div>
    <footer className="sidebar-footer">
      <button className="footer-button" onClick={onCapabilities} disabled={!data} title="앱 연결" aria-label="앱 연결"><Blocks size={17} /><span>앱 연결</span></button>
      <button className="footer-button" onClick={onRoutines} title="루틴" aria-label="루틴"><Repeat2 size={17} /><span>루틴</span></button>
      <button className="footer-button" onClick={() => showHidden(!hidden)} title="숨긴 봇" aria-label="숨긴 봇"><EyeOff size={17} /><span>숨긴 Bot</span></button>
      <button className="footer-button" onClick={onSettings} disabled={!data} title="설정" aria-label="설정"><Settings2 size={17} /><span>설정</span></button>
      <button className="footer-button profile-button" onClick={onSettings} disabled={!data}><span className="profile-avatar">L</span><span>로컬 사용자<small>이 컴퓨터에서 실행</small></span><MoreHorizontal size={16} /></button>
    </footer>
    {!mobile && !compact && <div className="sidebar-resizer" role="separator" aria-label="사이드바 크기 조정" aria-orientation="vertical" aria-valuemin={240} aria-valuemax={400} aria-valuenow={width} tabIndex={0} onKeyDown={event => { if (event.key === 'ArrowLeft' || event.key === 'ArrowRight') { event.preventDefault(); setWidth(Math.min(400, Math.max(240, width + (event.key === 'ArrowRight' ? 10 : -10)))); } }} onPointerDown={event => {
      event.preventDefault(); const start = event.clientX; const initial = width; const target = event.currentTarget; target.setPointerCapture(event.pointerId);
      const move = (moveEvent: PointerEvent) => setWidth(Math.min(400, Math.max(240, initial + moveEvent.clientX - start)));
      const stop = () => { target.removeEventListener('pointermove', move); target.removeEventListener('pointerup', stop); target.removeEventListener('pointercancel', stop); };
      target.addEventListener('pointermove', move); target.addEventListener('pointerup', stop); target.addEventListener('pointercancel', stop);
    }} />}
  </aside>;
}
