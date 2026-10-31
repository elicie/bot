import { useState } from 'react';
import { Clock3, Pencil, Plus, X } from 'lucide-react';
import type { Bot, Conversation } from '../shared/types';
import { activeRun, conversationPreview, runLabels } from './state';
import { Avatar, ErrorNotice, IconButton, Modal } from './ui';

export const botColors = ['#75a9c0', '#d58b4b', '#77b899', '#b89bc9', '#d4bc77', '#bf7884'];
export function BotDialog({
  bot,
  busy,
  error,
  onClose,
  onSave,
}: {
  bot?: Bot;
  busy: boolean;
  error: string | null;
  onClose: () => void;
  onSave: (input: { name: string; instructions: string; color: string }) => Promise<boolean>;
}) {
  const [name, setName] = useState(bot?.name ?? '');
  const [instructions, setInstructions] = useState(bot?.instructions ?? '');
  const [color, setColor] = useState(bot?.color ?? botColors[0]);
  const dirty =
    name !== (bot?.name ?? '') ||
    instructions !== (bot?.instructions ?? '') ||
    color !== (bot?.color ?? botColors[0]);
  const close = () => {
    if (busy || (dirty && !window.confirm('저장하지 않은 봇 설정을 버릴까요?'))) return;
    onClose();
  };
  return (
    <Modal title={bot ? '봇 편집' : '봇 추가'} onClose={close} className="compact-modal">
      <header className="modal-header">
        <h2>
          {bot ? <Pencil size={17} /> : <Plus size={18} />}
          {bot ? '봇 편집' : '새 봇'}
        </h2>
        <IconButton label="봇 설정 닫기" onClick={close} disabled={busy}>
          <X size={18} />
        </IconButton>
      </header>
      <form
        onSubmit={async (event) => {
          event.preventDefault();
          if (!name.trim() || busy) return;
          if (await onSave({ name: name.trim(), instructions, color })) onClose();
        }}
      >
        <div className="modal-body bot-form">
          <div className="avatar-picker">
            <Avatar color={color} seed={bot?.id ?? name} size={58} />
            <div className="color-options" role="group" aria-label="봇 색상">
              {botColors.map((option, index) => (
                <button
                  type="button"
                  key={option}
                  className="color-option"
                  style={{ backgroundColor: option }}
                  aria-label={`봇 색상 ${index + 1}`}
                  aria-pressed={color === option}
                  onClick={() => setColor(option)}
                  disabled={busy}
                />
              ))}
            </div>
          </div>
          <label className="settings-field">
            <span>봇 이름</span>
            <input
              data-autofocus
              aria-label="봇 이름"
              maxLength={80}
              required
              value={name}
              onChange={(event) => setName(event.target.value)}
              disabled={busy}
              placeholder="봇 이름을 입력하세요"
              autoComplete="off"
            />
          </label>
          <label className="settings-field">
            <span>
              봇 지침 <span className="muted">선택</span>
            </span>
            <textarea
              aria-label="봇 지침"
              rows={5}
              maxLength={20000}
              value={instructions}
              onChange={(event) => setInstructions(event.target.value)}
              disabled={busy}
              placeholder="어떤 일을 도울지, 어떻게 응답할지 알려주세요."
            />
            <small>새 작업에 적용되는 에이전트 지침입니다.</small>
          </label>
          {error && <ErrorNotice>{error}</ErrorNotice>}
        </div>
        <footer className="modal-footer">
          <button type="button" className="small-button" onClick={close} disabled={busy}>
            취소
          </button>
          <button
            type="submit"
            className="primary-button"
            disabled={busy || !name.trim() || !dirty}
          >
            {busy ? '저장 중…' : bot ? '봇 저장' : '봇 추가'}
          </button>
        </footer>
      </form>
    </Modal>
  );
}

export function History({
  bot,
  conversations,
  selectedId,
  busy,
  onSelect,
  onNew,
  onClose,
}: {
  bot: Bot;
  conversations: Conversation[];
  selectedId: string | null;
  busy: boolean;
  onSelect: (conversationId: string) => void;
  onNew: () => void;
  onClose: () => void;
}) {
  return (
    <Modal title="대화 기록" onClose={onClose} className="compact-modal history-modal">
      <header className="modal-header">
        <h2>
          <Clock3 size={18} />
          {bot.name}의 대화
        </h2>
        <IconButton label="대화 기록 닫기" onClick={onClose}>
          <X size={18} />
        </IconButton>
      </header>
      <div className="history-actions">
        <button type="button" className="small-button" onClick={onNew} disabled={busy}>
          <Plus size={15} />새 대화
        </button>
      </div>
      <div className="history-list">
        {conversations.length === 0 ? (
          <div className="history-empty">
            <Clock3 size={25} />
            <p>아직 대화가 없습니다.</p>
          </div>
        ) : (
          conversations.map((conversation) => (
            <button
              type="button"
              key={conversation.id}
              className={`history-item ${conversation.id === selectedId ? 'selected' : ''}`}
              aria-current={conversation.id === selectedId ? 'true' : undefined}
              onClick={() => onSelect(conversation.id)}
            >
              <span className="history-item-top">
                <strong>{conversation.title || '새 대화'}</strong>
                <time dateTime={conversation.updatedAt}>
                  {new Date(conversation.updatedAt).toLocaleDateString('ko-KR', {
                    month: 'numeric',
                    day: 'numeric',
                  })}
                </time>
              </span>
              <span>{conversationPreview(conversation)}</span>
              {activeRun(conversation.run.status) && (
                <small className="blue-text">{runLabels[conversation.run.status]}</small>
              )}
            </button>
          ))
        )}
      </div>
    </Modal>
  );
}
