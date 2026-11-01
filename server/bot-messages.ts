import { z } from 'zod';
import type { BotDelivery, BotExchange, Conversation } from '../shared/types.js';
import { HttpError } from './files.js';

export const MAX_BOT_HOPS = 4;
export const MAX_BOT_DELIVERIES = 10000;
export const MAX_BOT_MESSAGE_BYTES = 160 * 1024;
export const MAX_BOT_REPLY_CHARACTERS = 200000;
const timestamp = z.iso.datetime({ offset: true });
export const botMessageInputSchema = z.object({
  toBotId: z.string().uuid(), text: z.string().trim().min(1).max(100000),
  requestId: z.string().trim().min(1).max(128).regex(/^[\w.-]+$/).optional(),
}).strict();
export type BotMessageInput = z.infer<typeof botMessageInputSchema>;
export const botExchangeSchema = z.object({
  id: z.string().uuid(), botIds: z.tuple([z.string().uuid(), z.string().uuid()]),
  messages: z.array(z.object({
    id: z.string().uuid(), botId: z.string().uuid(), text: z.string().max(MAX_BOT_REPLY_CHARACTERS), createdAt: timestamp,
  }).strict()).min(1).max(2),
  sourceConversationId: z.string().uuid(), sourceMessageId: z.string().uuid(), updatedAt: timestamp,
}).strict().refine(exchange => exchange.botIds[0] !== exchange.botIds[1]
  && exchange.messages.every((message, index) => message.botId === exchange.botIds[index])
  && new Set(exchange.messages.map(message => message.id)).size === exchange.messages.length,
'An exchange must contain distinct bots and ordered, unique pair messages.') satisfies z.ZodType<BotExchange>;
export const botDeliverySchema = z.object({
  id: z.string().uuid(), exchangeId: z.string().uuid(), sourceConversationId: z.string().uuid(), sourceMessageId: z.string().uuid(),
  recipientConversationId: z.string().uuid(), fromBotId: z.string().uuid(), toBotId: z.string().uuid(),
  text: botMessageInputSchema.shape.text, chainId: z.string().uuid(), hop: z.number().int().min(1).max(MAX_BOT_HOPS), createdAt: timestamp,
  status: z.enum(['queued', 'running', 'completed', 'cancelled', 'error', 'interrupted']), requestId: botMessageInputSchema.shape.requestId,
  executionRunId: z.string().uuid().optional(), startedAt: timestamp.optional(), finishedAt: timestamp.optional(), error: z.string().max(1000).optional(),
}).strict().refine(delivery => {
  if (delivery.fromBotId === delivery.toBotId || Buffer.byteLength(delivery.text, 'utf8') > MAX_BOT_MESSAGE_BYTES) return false;
  if (delivery.status === 'queued') return !delivery.executionRunId && !delivery.startedAt && !delivery.finishedAt && !delivery.error;
  if (delivery.status === 'running') return Boolean(delivery.executionRunId && delivery.startedAt && !delivery.finishedAt && !delivery.error);
  return Boolean(delivery.finishedAt && (delivery.status !== 'completed' || (delivery.executionRunId && delivery.startedAt)));
}, 'Invalid bot delivery lifecycle.') satisfies z.ZodType<BotDelivery>;

export function existingBotDelivery(deliveries: BotDelivery[], sourceMessageId: string, input: BotMessageInput): BotDelivery | undefined {
  const existing = deliveries.find(delivery => delivery.sourceMessageId === sourceMessageId);
  if (!existing) return undefined;
  if (existing.toBotId === input.toBotId && existing.text === input.text) return existing;
  throw new HttpError(409, 'A run may send only one distinct bot message. Repeating the same request returns its existing receipt.');
}

export function botDeliveryReceipt(delivery: BotDelivery) {
  return { deliveryId: delivery.id, exchangeId: delivery.exchangeId, recipientConversationId: delivery.recipientConversationId, status: delivery.status };
}

export function reconcileBotDeliveries(deliveries: BotDelivery[], exchanges: BotExchange[], conversations: Conversation[], now: Date): void {
  for (const delivery of deliveries) {
    if (delivery.status !== 'queued' && delivery.status !== 'running') continue;
    const conversation = conversations.find(item => item.id === delivery.recipientConversationId)!;
    const execution = conversation.run;
    if (execution.status === 'idle' || (delivery.executionRunId && delivery.executionRunId !== execution.id)) continue;
    if (execution.id) delivery.executionRunId = execution.id;
    if (execution.startedAt) delivery.startedAt = execution.startedAt;
    if (execution.status === 'starting' || execution.status === 'running' || execution.status === 'awaiting_permission') {
      delivery.status = 'running';
      continue;
    }
    delivery.status = execution.status;
    delivery.finishedAt = execution.finishedAt ?? now.toISOString();
    if (execution.error) delivery.error = execution.error.slice(0, 1000);
    if (delivery.status !== 'completed') continue;
    const answer = conversation.messages.findLast(message => message.role === 'assistant');
    const exchange = exchanges.find(item => item.id === delivery.exchangeId)!;
    if (answer && !exchange.messages.some(message => message.id === answer.id)) {
      exchange.messages.push({ id: answer.id, botId: delivery.toBotId, text: answer.text, createdAt: answer.createdAt });
      exchange.updatedAt = delivery.finishedAt;
    }
  }
}

export function validBotMessageReferences(
  deliveries: BotDelivery[], exchanges: BotExchange[], conversations: Conversation[], botIds: Set<string>, routineConversationIds: Set<string>,
): boolean {
  const recipients = new Set<string>();
  const sources = new Set<string>();
  const exchangeIds = new Set<string>();
  for (const delivery of deliveries) {
    const exchange = exchanges.find(item => item.id === delivery.exchangeId);
    const source = conversations.find(item => item.id === delivery.sourceConversationId);
    const sourceMessage = source?.messages.find(message => message.id === delivery.sourceMessageId);
    const recipient = conversations.find(item => item.id === delivery.recipientConversationId);
    const parent = deliveries.find(item => item.recipientConversationId === delivery.sourceConversationId);
    if (!exchange || !source || !sourceMessage || sourceMessage.role !== 'assistant' || !recipient
      || !botIds.has(delivery.fromBotId) || !botIds.has(delivery.toBotId)
      || source.botId !== delivery.fromBotId || recipient.botId !== delivery.toBotId
      || source.id === recipient.id || recipients.has(recipient.id) || sources.has(sourceMessage.id) || exchangeIds.has(exchange.id)
      || routineConversationIds.has(recipient.id) || !sourceMessage.botExchangeIds?.includes(exchange.id)
      || exchange.sourceConversationId !== source.id || exchange.sourceMessageId !== sourceMessage.id
      || exchange.botIds[0] !== delivery.fromBotId || exchange.botIds[1] !== delivery.toBotId || exchange.messages[0].text !== delivery.text
      || (parent ? delivery.chainId !== parent.chainId || delivery.hop !== parent.hop + 1 : delivery.chainId !== delivery.id || delivery.hop !== 1)) return false;
    if (delivery.status === 'queued' && (recipient.run.status !== 'idle' || recipient.run.id || recipient.messages.length)) return false;
    if (delivery.executionRunId) {
      if (delivery.executionRunId !== recipient.run.id || delivery.startedAt !== recipient.run.startedAt || recipient.messages.length !== 2
        || recipient.messages[0].role !== 'user' || recipient.messages[0].sourceBotId !== delivery.fromBotId || recipient.messages[0].text !== delivery.text
        || recipient.messages[1].role !== 'assistant') return false;
    } else if (recipient.run.id || recipient.messages.length || delivery.startedAt) return false;
    if (delivery.status === 'running' && !['starting', 'running', 'awaiting_permission'].includes(recipient.run.status)) return false;
    if (!['queued', 'running'].includes(delivery.status) && (delivery.status !== recipient.run.status || delivery.finishedAt !== recipient.run.finishedAt)) return false;
    const answer = recipient.messages.findLast(message => message.role === 'assistant');
    if (delivery.status === 'completed') {
      if (!answer || exchange.messages.length !== 2 || exchange.messages[1].id !== answer.id || exchange.messages[1].text !== answer.text) return false;
    } else if (exchange.messages.length !== 1) return false;
    recipients.add(recipient.id);
    sources.add(sourceMessage.id);
    exchangeIds.add(exchange.id);
  }
  if (exchanges.some(exchange => !exchangeIds.has(exchange.id))) return false;
  return conversations.every(conversation => conversation.messages.every(message => {
    if (message.sourceBotId && !deliveries.some(delivery => delivery.recipientConversationId === conversation.id
      && delivery.fromBotId === message.sourceBotId && conversation.messages[0] === message)) return false;
    const ids = message.botExchangeIds ?? [];
    return new Set(ids).size === ids.length && ids.every(id => exchanges.some(exchange => exchange.id === id
      && exchange.sourceConversationId === conversation.id && exchange.sourceMessageId === message.id));
  }));
}
