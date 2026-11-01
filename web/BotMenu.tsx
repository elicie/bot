// Sidebar bot menu additions: read state, update notifications, and deletion with confirmation.

import { Bell, BellOff, CircleDot, MailOpen, Trash2, X } from 'lucide-react';
import type { AppState, Bot } from '../shared/types';
import { activeRun, mainBotId } from './state';
import { ErrorNotice, IconButton, Modal } from './ui';
import { MainBotMenuItem } from './MainBot';
import './chat-features.css';

export function BotMenuExtras({ bot, busy, onUnread, onNotify, onDelete, onChooseMain, close }: {
  bot: Bot; busy: boolean;
  onUnread: (bot: Bot, unread: boolean) => void;
  onNotify: (bot: Bot, enabled: boolean) => void;
  onDelete: (bot: Bot) => void;
  /** Open "메인 Bot 선택" with this bot preselected. */
  onChooseMain?: (bot: Bot) => void;
  close: () => void;
}) {
  return <>
    <button role="menuitem" onClick={() => { onUnread(bot, !bot.unread); close(); }}>
      {bot.unread ? <MailOpen size={14} /> : <CircleDot size={14} />}{bot.unread ? '읽음으로 표시' : '읽지 않음으로 표시'}
    </button>
    <button role="menuitemcheckbox" aria-checked={!!bot.notifyOnUpdates} title="이 Bot의 작업이 끝나거나 응답을 기다릴 때 알림 받기"
      onClick={() => { onNotify(bot, !bot.notifyOnUpdates); close(); }}>
      {bot.notifyOnUpdates ? <BellOff size={14} /> : <Bell size={14} />}{bot.notifyOnUpdates ? '알림 끄기' : '알림 받기'}
    </button>
    {onChooseMain && <MainBotMenuItem busy={busy} close={close} onChoose={() => onChooseMain(bot)} />}
    <button role="menuitem" className="danger-menuitem" disabled={busy} onClick={() => { onDelete(bot); close(); }}>
      <Trash2 size={14} />Bot 삭제
    </button>
  </>;
}

export function BotDeleteDialog({ bot, data, busy, error, onCancel, onConfirm }: {
  bot: Bot; data: AppState; busy: boolean; error: string | null;
  onCancel: () => void; onConfirm: () => void;
}) {
  const conversations = data.conversations.filter((conversation) => conversation.botId === bot.id);
  const routines = data.routines.filter((routine) => routine.botId === bot.id).length;
  const running = conversations.some((conversation) => activeRun(conversation.run.status));
  const last = data.bots.length <= 1;
  const blocked = last ? '마지막 Bot은 삭제할 수 없어요. 다른 Bot을 먼저 추가하세요.' : running ? '이 Bot의 작업이 끝난 뒤 삭제할 수 있어요.' : null;
  // Deleting the main bot makes the first remaining bot the main bot (`grok_bot_store::main_bot`).
  const successor = !last && mainBotId(data) === bot.id ? data.bots.find((item) => item.id !== bot.id) : undefined;
  const close = () => { if (!busy) onCancel(); };
  return <Modal title={`${bot.name} 삭제`} onClose={close} className="compact-modal bot-delete-dialog">
    <header className="modal-header">
      <h2>“{bot.name}”을(를) 삭제할까요?</h2>
      <IconButton label="삭제 취소" onClick={close} disabled={busy}><X size={18} /></IconButton>
    </header>
    <div className="bot-delete-body">
      <p>이 Bot과 채팅 기록이 영구적으로 삭제돼요. 되돌릴 수 없어요.</p>
      <ul>
        <li>대화 {conversations.length}개와 첨부 파일</li>
        <li>루틴 {routines}개와 실행 기록</li>
        <li>이 Bot의 비공개 스킬과 다른 Bot과 주고받은 메시지</li>
      </ul>
      {successor && <p className="bot-delete-main">이 Bot은 메인 Bot이에요. 삭제하면 “{successor.name}”이(가) 메인 Bot이 돼요.</p>}
      {blocked && <p className="bot-delete-blocked" role="status">{blocked}</p>}
      {error && <ErrorNotice>{error}</ErrorNotice>}
    </div>
    <footer className="modal-footer">
      <button type="button" className="small-button" onClick={close} disabled={busy} data-autofocus>취소</button>
      <button type="button" className="danger-button" disabled={busy || !!blocked}
        onClick={onConfirm}>{busy ? '삭제 중...' : '삭제'}</button>
    </footer>
  </Modal>;
}
