import { useRef, useState, type ReactNode } from 'react';
import { MessageSquareReply, MessagesSquare, X } from 'lucide-react';
import type { AppState, Bot, Conversation, Message } from '../shared/types';
import { request } from './api';
import { Composer, Transcript } from './Conversation';
import { groupAsBot, roundPending, speakerLookup, type Run } from './Groups';
import { activeRun, runningConversation } from './state';
import { Avatar, IconButton } from './ui';

const threadFor = (data: AppState, messageId: string) => data.conversations.find(conversation => conversation.thread?.parentMessageId === messageId);

/** "스레드에서 답장" under a message, plus the reply count of an existing thread. */
export function ThreadLinks({ data, conversation, message, busy, run, onSnapshot, onOpen }: {
  data: AppState; conversation: Conversation; message: Message; busy: boolean; run: Run;
  onSnapshot: (state: AppState) => void; onOpen: (threadId: string) => void;
}) {
  if (conversation.thread || !message.text.trim()) return null;
  const thread = threadFor(data, message.id);
  const replies = thread?.messages.filter(item => item.text.trim()).length ?? 0;
  const open = async () => {
    if (thread) { onOpen(thread.id); return; }
    await run('thread', async () => {
      const result = await request<{ conversation: Conversation; state: AppState }>(`/api/conversations/${encodeURIComponent(conversation.id)}/threads`, { method: 'POST', body: { messageId: message.id } });
      onSnapshot(result.state);
      onOpen(result.conversation.id);
    });
  };
  return <div className={`thread-links ${message.role}`}>
    {replies > 0 && <button type="button" className="thread-count" onClick={() => void open()} aria-label="답글 스레드 열기"><MessagesSquare size={13} />답글 {replies}개</button>}
    <button type="button" className="thread-reply" onClick={() => void open()} disabled={busy} aria-label={thread ? '스레드 열기' : '스레드에서 답장'} title={thread ? '메인 채팅 옆에 스레드 열기' : '스레드 만들기'}><MessageSquareReply size={13} /><span>스레드에서 답장</span></button>
  </div>;
}

/** The thread side panel next to the main chat (full screen on narrow windows). */
export function ThreadPanel({ data, threadId, busy, online, run, onSnapshot, onClose, onSettings, canvasControl, draft: controlledDraft, onDraft }: {
  data: AppState; threadId: string; busy: boolean; online: boolean; run: Run;
  onSnapshot: (state: AppState) => void; onClose: () => void; onSettings: () => void;
  canvasControl?: ReactNode; draft?: string; onDraft?: (draft: string) => void;
}) {
  const [localDraft, setLocalDraft] = useState('');
  const draft = controlledDraft ?? localDraft;
  const setDraft = onDraft ?? setLocalDraft;
  const draftRef = useRef(draft);
  draftRef.current = draft;
  const thread = data.conversations.find(conversation => conversation.id === threadId && conversation.thread);
  const parent = thread && data.conversations.find(conversation => conversation.id === thread.thread!.parentConversationId);
  const parentMessage = parent?.messages.find(message => message.id === thread!.thread!.parentMessageId);
  const group = thread?.groupId ? data.groups?.find(item => item.id === thread.groupId) : undefined;
  const bot: Bot | undefined = group ? groupAsBot(group) : data.bots.find(item => item.id === thread?.botId);
  const speakerFor = group ? speakerLookup(data) : undefined;
  const author = parentMessage && (parentMessage.role === 'user' ? undefined : speakerFor?.(parentMessage) ?? bot);
  const globalRun = runningConversation(data);
  const running = !!thread && (activeRun(thread.run.status) || roundPending(thread));
  const send = async () => {
    const text = draft.trim();
    if (!thread || !text || running || globalRun) return;
    if (!data.settings.baseUrl || !data.settings.model) { onSettings(); return; }
    await run('thread-message', async () => {
      onSnapshot(await request<AppState>(`/api/conversations/${encodeURIComponent(thread.id)}/messages`, { method: 'POST', body: { text } }));
      if (draftRef.current === draft) setDraft('');
    });
  };
  const cancel = async () => {
    if (!thread || !running) return;
    await run('cancel', async () => onSnapshot(await request<AppState>(`/api/conversations/${encodeURIComponent(thread.id)}/cancel`, { method: 'POST', body: {} })));
  };
  const permission = async (optionId: string) => {
    const pending = thread?.run.permission;
    if (!thread || !pending) return;
    await run('permission', async () => onSnapshot(await request<AppState>(`/api/conversations/${encodeURIComponent(thread.id)}/permissions/${encodeURIComponent(pending.id)}`, { method: 'POST', body: { optionId } })));
  };
  return <aside className="thread-panel reference-chat" aria-label="스레드">
    <header className="thread-header">
      <span><MessagesSquare size={16} />스레드{bot && <small>{bot.name}</small>}</span>
      {canvasControl}
      <IconButton label="스레드 닫기" onClick={onClose}><X size={18} /></IconButton>
    </header>
    {!thread || !bot ? <p className="pair-empty">이 스레드를 찾을 수 없습니다.</p> : <>
      <blockquote className="thread-parent" aria-label="원본 메시지">
        {parentMessage ? <>
          <span className="thread-parent-author">{author ? <><Avatar color={author.color} seed={author.id} shape={author.shape} size={16} />{author.name}</> : '내 메시지'}</span>
          <p>{parentMessage.text}</p>
        </> : <p className="muted">원본 메시지를 찾을 수 없습니다.</p>}
      </blockquote>
      <div className={group ? 'group-chat thread-transcript' : 'thread-transcript'}>
        <Transcript bot={bot} conversation={thread} data={data} speakerFor={speakerFor}
          modelConfigured={!!data.settings.baseUrl && !!data.settings.model} runtimeAvailable={data.runtime.available} runtimeError={data.runtime.error}
          busy={busy} onSettings={onSettings} onRuntimeSettings={onSettings} onPermission={optionId => void permission(optionId)} />
      </div>
      <Composer botName={bot.name} label="스레드 메시지" placeholder="스레드에 답장…" value={draft} onChange={setDraft} onSend={() => void send()}
        onCancel={() => void cancel()} running={running} disabled={!online || !!globalRun || roundPending(thread)} busy={busy} model={data.settings.model} />
    </>}
  </aside>;
}
