// "메인 Bot 선택": choose the main bot (server rules in `grok_bot_store::main_bot`). The main bot
// opens when no bot is selected, comes first among the pinned bots in the sidebar, and carries the
// "메인 Bot" label there.

import { useState, type FormEvent } from 'react';
import { Star, X } from 'lucide-react';
import type { AppState } from '../shared/types';
import { request } from './api';
import { mainBotId } from './state';
import { Avatar, ErrorNotice, IconButton, Modal } from './ui';
import type { Run } from './Groups';
import './chat-features.css';

export function MainBotMenuItem({ busy, onChoose, close }: { busy: boolean; onChoose: () => void; close: () => void }) {
  return <button role="menuitem" disabled={busy} onClick={() => { close(); onChoose(); }}>
    <Star size={14} />메인 Bot 선택
  </button>;
}

export function MainBotDialog({ data, initial, busy, running, error: serverError, run, onSnapshot, onClose }: {
  data: AppState;
  /** Preselected bot (the bot whose menu opened the dialog). */
  initial?: string;
  busy: boolean;
  /** A run is active: bot changes wait until it ends. */
  running: boolean;
  error: string | null;
  run: Run;
  onSnapshot: (state: AppState) => void;
  onClose: () => void;
}) {
  const current = mainBotId(data);
  const [choice, setChoice] = useState(initial && data.bots.some((bot) => bot.id === initial) ? initial : current ?? '');
  const [attempted, setAttempted] = useState(false);
  const error = attempted ? serverError : null;
  const close = () => { if (!busy) onClose(); };
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!choice || busy || running) return;
    if (choice === current) { onClose(); return; }
    setAttempted(true);
    const saved = await run('main-bot', async () => {
      onSnapshot(await request<AppState>(`/api/bots/${encodeURIComponent(choice)}/main`, { method: 'POST', body: {} }));
      return true;
    });
    if (saved) onClose();
  };
  return <Modal title="메인 Bot 선택" onClose={close} className="compact-modal main-bot-dialog">
    <form onSubmit={(event) => void submit(event)}>
      <header className="modal-header">
        <h2>메인 Bot 선택</h2>
        <IconButton label="메인 Bot 선택 닫기" onClick={close} disabled={busy}><X size={18} /></IconButton>
      </header>
      <div className="main-bot-body">
        <p>메인 Bot은 사이드바 맨 위에 고정되고, Bot을 고르지 않았을 때 먼저 열려요.</p>
        <div className="main-bot-options" role="radiogroup" aria-label="메인 Bot">
          {data.bots.map((bot) => <label key={bot.id} className={`main-bot-option ${choice === bot.id ? 'is-selected' : ''}`}>
            <input type="radio" name="main-bot" value={bot.id} checked={choice === bot.id} disabled={busy}
              onChange={() => setChoice(bot.id)} data-autofocus={choice === bot.id ? true : undefined} />
            <Avatar color={bot.color} seed={bot.id} shape={bot.shape} size={26} />
            <span className="main-bot-option-name">{bot.name}</span>
            {bot.id === current && <small>현재 메인 Bot</small>}
            {bot.hidden && bot.id !== current && <small>숨김</small>}
          </label>)}
        </div>
        {running && <p className="main-bot-note" role="status">진행 중인 작업이 끝난 뒤 메인 Bot을 바꿀 수 있어요.</p>}
        {error && <ErrorNotice>메인 Bot을 바꿀 수 없어요. {error}</ErrorNotice>}
      </div>
      <footer className="modal-footer">
        <button type="button" className="small-button" onClick={close} disabled={busy}>취소</button>
        <button type="submit" className="primary-button" disabled={busy || running || !choice}>{busy ? '저장 중...' : '메인 Bot으로 설정'}</button>
      </footer>
    </form>
  </Modal>;
}
