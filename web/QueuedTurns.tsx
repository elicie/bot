import { Clock3, Paperclip, X } from 'lucide-react';
import type { AppState, QueuedWork } from '../shared/types';
import { queuedWorkForConversation } from './state';
import { IconButton } from './ui';

function description(data: AppState, work: QueuedWork): string {
  switch (work.kind) {
    case 'user': return work.text || '첨부 파일 메시지';
    case 'routine': return data.routineRuns.find(run => run.id === work.runId)?.prompt ?? '루틴 실행';
    case 'delivery': return data.botDeliveries?.find(delivery => delivery.id === work.deliveryId)?.text ?? 'Bot 메시지';
    case 'group': return `${data.bots.find(bot => bot.id === work.botId)?.name ?? 'Bot'}의 그룹 답변`;
    case 'report': return 'Bot 답변 보고';
  }
}

/** Accepted requests stay separate from live messages until the server starts their turns. */
export function QueuedTurns({ data, conversationId, disabled = false, onCancel }: {
  data: AppState;
  conversationId: string | null | undefined;
  disabled?: boolean;
  onCancel: (turnId: string) => void;
}) {
  const work = queuedWorkForConversation(data, conversationId);
  if (!work.length) return null;
  return <section className="queued-turns" aria-label="대기 중인 메시지">
    <header><Clock3 size={13} aria-hidden="true" /><span role="status" aria-live="polite" aria-atomic="true">대기 중 · {work.length}개</span></header>
    <ol>
      {work.map(turn => <li key={turn.id} data-queued-turn-id={turn.id}>
        <div className="queued-turn-copy">
          <span className="queued-turn-status">대기 중</span>
          <p>{description(data, turn)}</p>
          {turn.kind === 'user' && !!turn.attachments.length && <ul className="queued-turn-attachments" aria-label="대기 메시지 첨부 파일">
            {turn.attachments.map(attachment => <li key={attachment.id}><Paperclip size={12} aria-hidden="true" />{attachment.name}</li>)}
          </ul>}
          {turn.kind === 'user' && !!turn.speakerBotIds?.length && <small className="queued-turn-speakers">답변할 Bot: {turn.speakerBotIds.map(id => data.bots.find(bot => bot.id === id)?.name ?? '삭제된 Bot').join(', ')}</small>}
        </div>
        {turn.kind === 'user' && <IconButton label="대기 메시지 취소" title="이 대기 메시지만 취소" disabled={disabled} onClick={() => onCancel(turn.id)}><X size={14} /></IconButton>}
      </li>)}
    </ol>
  </section>;
}
