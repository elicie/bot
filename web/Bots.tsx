import { useState } from 'react';
import { Clock3, Plus, X } from 'lucide-react';
import type { AvatarShape, Bot, Conversation } from '../shared/types';
import { activeRun, conversationPreview, runLabels } from './state';
import { Avatar, avatarShapes, ErrorNotice, IconButton, Modal } from './ui';
import './bots.css';

const botPalette = [
  { color: '#ffffff', label: '흰색' },
  { color: '#a36c36', label: '갈색' },
  { color: '#ff2543', label: '빨강' },
  { color: '#ff6b00', label: '주황' },
  { color: '#ff9c00', label: '노랑' },
  { color: '#00c77b', label: '녹색' },
  { color: '#00b6a6', label: '청록색' },
  { color: '#198aff', label: '파랑' },
  { color: '#8b55ff', label: '보라색' },
  { color: '#ff269c', label: '마젠타' },
  { color: '#808080', label: '회색' },
];
export const botColors = botPalette.map((option) => option.color);

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
  onSave: (input: { name: string; instructions: string; color: string; shape?: AvatarShape }) => Promise<boolean>;
}) {
  const [name, setName] = useState(bot?.name ?? '');
  const [instructions, setInstructions] = useState(bot?.instructions ?? '');
  const [color, setColor] = useState(bot?.color ?? botColors[0]);
  const [shape, setShape] = useState<AvatarShape>(bot?.shape ?? 'blob');
  const dirty =
    name !== (bot?.name ?? '') ||
    instructions !== (bot?.instructions ?? '') ||
    color !== (bot?.color ?? botColors[0]) ||
    shape !== (bot?.shape ?? 'blob');
  const close = () => {
    if (busy || (dirty && !window.confirm('저장하지 않은 봇 설정을 버릴까요?'))) return;
    onClose();
  };
  return (
    <Modal title={bot ? '봇 편집' : '봇 추가'} onClose={close} className="compact-modal bot-dialog">
      <header className="modal-header">
        <h2>{bot ? '봇 편집' : '새 Bot 만들기'}</h2>
        <IconButton label="봇 설정 닫기" onClick={close} disabled={busy}>
          <X size={18} />
        </IconButton>
      </header>
      <form
        aria-busy={busy}
        onSubmit={async (event) => {
          event.preventDefault();
          if (!name.trim() || busy || !dirty) return;
          if (await onSave({ name: name.trim(), instructions, color, shape })) onClose();
        }}
      >
        <div className="bot-dialog-body">
          <div className="bot-dialog-preview" role="img" aria-label="봇 아바타 미리보기">
            <Avatar color={color} seed={bot?.id ?? name} size={80} shape={shape} />
          </div>
          <label className="bot-dialog-name">
            <span>봇 이름</span>
            <input
              data-autofocus
              aria-label="봇 이름"
              maxLength={80}
              required
              value={name}
              onChange={(event) => setName(event.target.value)}
              disabled={busy}
              placeholder="Bot 이름"
              autoComplete="off"
            />
          </label>
          <div className="bot-dialog-shapes" role="group" aria-label="봇 모양">
            {avatarShapes.map((option) => (
              <button
                type="button"
                key={option.value}
                className="bot-dialog-shape"
                aria-label={`${option.label} 캐릭터 모양`}
                aria-pressed={shape === option.value}
                title={`${option.label} 캐릭터 모양`}
                onClick={() => setShape(option.value)}
                disabled={busy}
              >
                <Avatar color={color} size={40} shape={option.value} />
              </button>
            ))}
          </div>
          <div className="bot-dialog-colors" role="group" aria-label="봇 색상">
            {botPalette.map((option) => (
              <button
                type="button"
                key={option.color}
                className="bot-dialog-color"
                style={{ backgroundColor: option.color }}
                aria-label={`${option.label} 캐릭터 색상`}
                aria-pressed={color.toLowerCase() === option.color}
                title={`${option.label} 캐릭터 색상`}
                onClick={() => setColor(option.color)}
                disabled={busy}
              />
            ))}
          </div>
          <label className="bot-dialog-instructions">
            <span>
              봇 지침 <span className="bot-dialog-optional">선택</span>
            </span>
            <textarea
              aria-label="봇 지침"
              rows={2}
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
            aria-label={bot ? '봇 저장' : '봇 추가'}
            disabled={busy || !name.trim() || !dirty}
          >
            {busy ? '저장 중…' : bot ? '봇 저장' : '만들기'}
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
