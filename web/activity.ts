// Bot activity in the browser: marking the shown bot read, the message sound, and browser
// notifications for bots with "알림 받기" on.
//
// Read state lives on the server (`Bot.unread` in state.json), so every browser sees the same
// unread dots and they survive reloads. The server marks a bot unread when an answer finishes; the
// browser that is showing that bot (chat view, visible tab) marks it read again. A bot the user
// marked unread by hand stays unread until it is opened again.

import { useEffect, useRef } from 'react';
import type { AppState, Bot, Conversation } from '../shared/types';
import { activeRun, type Selection } from './state';
import { playMessageSound } from './sound';

export type NotificationSupport = 'granted' | 'default' | 'denied' | 'unavailable';

/** Notifications need the API and a secure context (HTTPS or localhost); LAN HTTP has neither. */
export function notificationSupport(): NotificationSupport {
  if (typeof window === 'undefined' || !('Notification' in window) || !window.isSecureContext) return 'unavailable';
  return Notification.permission;
}

export const notificationUnavailableText =
  '이 브라우저에서는 알림을 표시할 수 없어요. 알림은 HTTPS 또는 localhost 주소로 연 경우에만 사용할 수 있어요.';
export const notificationDeniedText =
  '브라우저에서 이 사이트의 알림이 차단되어 있어요. 브라우저 설정에서 알림을 허용해 주세요.';

/** Ask for permission when it has not been decided yet. Resolves to the resulting support. */
export async function requestNotificationPermission(): Promise<NotificationSupport> {
  const support = notificationSupport();
  if (support !== 'default') return support;
  try {
    return await Notification.requestPermission();
  } catch {
    return notificationSupport();
  }
}

function preview(conversation: Conversation): string {
  if (conversation.run.status === 'error') return conversation.run.error ?? '작업을 완료하지 못했어요.';
  const answer = [...conversation.messages].reverse().find((message) => message.role === 'assistant' && message.text.trim());
  const text = answer?.text.replace(/\s+/g, ' ').trim() ?? '작업을 마쳤어요.';
  return text.length > 140 ? `${text.slice(0, 139)}…` : text;
}

function notify(bot: Bot, conversation: Conversation, onOpen: (selection: Selection) => void) {
  if (notificationSupport() !== 'granted') return;
  try {
    const notification = new Notification(bot.name, { body: preview(conversation), tag: `grok-bot-${conversation.id}` });
    notification.onclick = () => {
      window.focus();
      onOpen({ botId: bot.id, conversationId: conversation.id });
      notification.close();
    };
  } catch {
    // Some browsers only allow notifications from a service worker; skip them quietly.
  }
}

export function useBotActivity({
  data,
  selection,
  chat,
  onRead,
  onOpen,
}: {
  data: AppState | null;
  selection: Selection;
  /** The chat surface (not routines or a bot-to-bot view) is shown. */
  chat: boolean;
  onRead: (botId: string) => void;
  onOpen: (selection: Selection) => void;
}) {
  const statuses = useRef<Map<string, string> | null>(null);
  const finishedWhileHidden = useRef(false);
  const latest = useRef({ data, selection, chat, onRead });
  latest.current = { data, selection, chat, onRead };
  const visible = () => document.visibilityState === 'visible';
  const readSelected = () => {
    const { data: current, selection: selected, chat: showing, onRead: read } = latest.current;
    const bot = current?.bots.find((item) => item.id === selected.botId);
    if (bot?.unread && showing && visible()) read(bot.id);
  };

  // Opening a bot (or reloading on it) marks it read.
  useEffect(readSelected, [selection.botId, chat, data !== null]);

  useEffect(() => {
    if (!data) return;
    const previous = statuses.current;
    const next = new Map(data.conversations.map((conversation) => [conversation.id, conversation.run.status] as const));
    statuses.current = next;
    if (!previous) return;
    let sound = false;
    let finishedShown = false;
    for (const conversation of data.conversations) {
      const before = previous.get(conversation.id);
      if (!before || !activeRun(before as Conversation['run']['status']) || activeRun(conversation.run.status)) continue;
      if (conversation.run.status !== 'completed' && conversation.run.status !== 'error') continue;
      const bot = data.bots.find((item) => item.id === conversation.botId);
      if (!bot) continue;
      if (conversation.run.status === 'completed') sound = true;
      const shown = chat && bot.id === selection.botId;
      if (shown && visible()) finishedShown = true;
      if (shown && !visible()) finishedWhileHidden.current = true;
      if (bot.notifyOnUpdates && (!shown || !visible())) notify(bot, conversation, onOpen);
    }
    if (sound) playMessageSound();
    // Only new activity in the shown bot is read automatically, never a manual "mark unread".
    if (finishedShown) readSelected();
  }, [data]);

  useEffect(() => {
    const changed = () => {
      if (!visible() || !finishedWhileHidden.current) return;
      finishedWhileHidden.current = false;
      readSelected();
    };
    document.addEventListener('visibilitychange', changed);
    return () => document.removeEventListener('visibilitychange', changed);
  }, []);
}
