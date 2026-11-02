import { ArrowLeftRight, Clock3, CornerDownRight, X } from 'lucide-react';
import type { AppState, BotDelivery, BotExchange, Message } from '../shared/types';
import { Avatar } from './ui';

const exchangeStatus: Record<BotDelivery['status'], string> = {
  queued: '대기 중',
  running: '답변 중',
  completed: '답변 도착',
  error: '답변 실패',
  cancelled: '중단됨',
  interrupted: '중단됨',
};

const reportLabels: Record<NonNullable<BotDelivery['report']>['status'], string> = {
  queued: '보낸 Bot이 이 답변을 받아 이어서 답할 차례를 기다리고 있습니다.',
  running: '보낸 Bot이 이 답변을 받아 이어서 답하고 있습니다.',
  completed: '보낸 Bot이 이 답변을 받아 이어서 답했습니다.',
  cancelled: '이어서 답하기를 중단했습니다.',
  interrupted: '이어서 답하기가 중단됐습니다.',
  error: '이 답변을 받아 이어서 답하지 못했습니다.',
  skipped: '이 대화 흐름은 이어서 답하기 한도(2회)에 도달해 답변을 다시 전달하지 않았습니다.',
};

export function BotMessageLinks({ message, data, onOpen }: { message: Message; data: AppState; onOpen: (id: string) => void }) {
  const exchanges = (message.botExchangeIds ?? []).map(id => data.botExchanges?.find(exchange => exchange.id === id)).filter((item): item is BotExchange => Boolean(item));
  return <>{exchanges.map(exchange => {
    const otherId = exchange.botIds.find(id => id !== data.conversations.find(conversation => conversation.messages.some(entry => entry.id === message.id))?.botId) ?? exchange.botIds[1];
    const other = data.bots.find(bot => bot.id === otherId);
    const delivery = data.botDeliveries?.find(item => item.exchangeId === exchange.id);
    return <div className="bot-exchange-summary" key={exchange.id}><ArrowLeftRight size={13} /><span>메시지 {exchange.messages.length}개 ·</span><button type="button" onClick={() => onOpen(exchange.id)} aria-label={`${other?.name ?? 'Bot'}와의 대화 열기`}>{other && <Avatar color={other.color} seed={other.id} shape={other.shape} size={18} />}{other?.name ?? 'Bot'}</button>{delivery && <span className="bot-exchange-status" data-status={delivery.status} aria-label={`${other?.name ?? 'Bot'} 답변 상태`}>{exchangeStatus[delivery.status]}</span>}</div>;
  })}</>;
}

/** Shown instead of the hidden input of an automatic follow-up turn, right above the bot's reply. */
export function BotReportMarker({ message, data, onOpen }: { message: Message; data?: AppState; onOpen?: (id: string) => void }) {
  const deliveries = (message.reportDeliveryIds ?? []).map(id => data?.botDeliveries?.find(delivery => delivery.id === id)).filter((item): item is BotDelivery => Boolean(item));
  const recipients = [...new Set(deliveries.map(delivery => delivery.toBotId))].map(id => ({ id, bot: data?.bots.find(bot => bot.id === id), exchangeId: deliveries.find(delivery => delivery.toBotId === id)!.exchangeId }));
  const label = `${recipients.map(item => item.bot?.name ?? '삭제된 Bot').join(', ') || '다른 Bot'}의 답변을 받아 이어서 답함`;
  return <div className="bot-report-marker" role="note" aria-label="이어서 답한 Bot 답변">
    <CornerDownRight size={13} aria-hidden="true" />
    <span>{label}</span>
    {onOpen && recipients.map(item => <button type="button" className="text-button" key={item.id} onClick={() => onOpen(item.exchangeId)} aria-label={`${item.bot?.name ?? 'Bot'}의 답변 보기`}>답변 보기</button>)}
  </div>;
}

export function BotChat({ data, exchangeId, onClose, onExecution }: { data: AppState; exchangeId: string; onClose: () => void; onExecution: (botId: string, conversationId: string) => void }) {
  const exchange = data.botExchanges?.find(item => item.id === exchangeId);
  const delivery = data.botDeliveries?.find(item => item.exchangeId === exchangeId);
  const bots = exchange?.botIds.map(id => data.bots.find(bot => bot.id === id));
  return <>
    <header className="pair-header" aria-label="봇 간 대화">{bots?.map((bot, index) => bot && <span key={bot.id}>{index === 1 && <ArrowLeftRight size={14} />}<Avatar color={bot.color} seed={bot.id} shape={bot.shape} size={28} />{bot.name}</span>)}</header>
    <div className="pair-transcript" role="log" aria-label="봇 간 대화 메시지"><div>
      {!exchange ? <p className="pair-empty">이 봇 대화를 찾을 수 없습니다.</p> : <>
        <p className="date-label">{new Date(exchange.updatedAt).toLocaleDateString('ko-KR', { month: 'long', day: 'numeric', weekday: 'short' })}</p>
        {exchange.messages.map(message => { const bot = data.bots.find(item => item.id === message.botId); return <article className="pair-message" key={message.id} aria-label={`${bot?.name ?? 'Bot'} 메시지`}><Avatar color={bot?.color} seed={message.botId} shape={bot?.shape} size={28} /><div><strong>{bot?.name ?? 'Bot'}</strong><p>{message.text}</p><small>{new Date(message.createdAt).toLocaleTimeString('ko-KR', { hour: 'numeric', minute: '2-digit' })}</small></div></article>; })}
        {delivery && ['queued', 'running'].includes(delivery.status) && <div className="pair-delivery" role="status"><Clock3 size={14} /><span>{delivery.status === 'queued' ? 'Bot의 응답을 기다리고 있습니다.' : 'Bot이 작업 중입니다.'}</span><button className="text-button" onClick={() => onExecution(delivery.toBotId, delivery.recipientConversationId)}>실행 대화 열기</button></div>}
        {delivery && ['error', 'cancelled', 'interrupted'].includes(delivery.status) && <p className="terminal-note">{delivery.error || (delivery.status === 'cancelled' ? 'Bot 작업이 취소됐습니다.' : 'Bot 작업이 중단됐습니다.')}<button className="text-button" onClick={() => onExecution(delivery.toBotId, delivery.recipientConversationId)}>실행 대화 확인</button></p>}
        {delivery?.report && <p className="pair-report" role="status"><CornerDownRight size={13} aria-hidden="true" />{delivery.report.status === 'error' ? delivery.report.error || reportLabels.error : reportLabels[delivery.report.status]}</p>}
      </>}
    </div></div>
    <footer className="pair-close"><button type="button" className="small-button" onClick={onClose}><X size={14} />채팅 닫기</button></footer>
  </>;
}
