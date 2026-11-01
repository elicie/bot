import { ArrowLeftRight, Clock3, X } from 'lucide-react';
import type { AppState, BotExchange, Message } from '../shared/types';
import { Avatar } from './ui';

export function BotMessageLinks({ message, data, onOpen }: { message: Message; data: AppState; onOpen: (id: string) => void }) {
  const exchanges = (message.botExchangeIds ?? []).map(id => data.botExchanges?.find(exchange => exchange.id === id)).filter((item): item is BotExchange => Boolean(item));
  return <>{exchanges.map(exchange => {
    const otherId = exchange.botIds.find(id => id !== data.conversations.find(conversation => conversation.messages.some(entry => entry.id === message.id))?.botId) ?? exchange.botIds[1];
    const other = data.bots.find(bot => bot.id === otherId);
    return <div className="bot-exchange-summary" key={exchange.id}><ArrowLeftRight size={13} /><span>메시지 {exchange.messages.length}개 ·</span><button type="button" onClick={() => onOpen(exchange.id)} aria-label={`${other?.name ?? 'Bot'}와의 대화 열기`}>{other && <Avatar color={other.color} seed={other.id} shape={other.shape} size={18} />}{other?.name ?? 'Bot'}</button></div>;
  })}</>;
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
      </>}
    </div></div>
    <footer className="pair-close"><button type="button" className="small-button" onClick={onClose}><X size={14} />채팅 닫기</button></footer>
  </>;
}
