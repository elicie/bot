import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { ArrowDown, ArrowUp, AtSign, Clock3, Menu, MoreHorizontal, Pencil, Plus, SquarePen, Trash2, Users, X } from 'lucide-react';
import type { AppState, AvatarShape, Bot, Conversation, Group, GroupInput, Message } from '../shared/types';
import { request } from './api';
import { botColors, History } from './Bots';
import { Composer, Transcript } from './Conversation';
import { activeRun, conversationPreview, type Surface } from './state';
import { ThreadLinks } from './Threads';
import { Avatar, avatarShapes, ErrorNotice, IconButton, Modal } from './ui';
import './groups.css';

type GroupSurface = Extract<Surface, { kind: 'group' }>;
export type Run = <T>(name: string, operation: () => Promise<T>) => Promise<T | undefined>;

/** The group's own conversations (threads excluded), newest first. */
export const groupConversations = (data: AppState | null, groupId: string) =>
  (data?.conversations ?? []).filter(conversation => conversation.groupId === groupId && !conversation.thread).sort((a, b) => b.updatedAt.localeCompare(a.updatedAt));

/** A bot-shaped stand-in so shared conversation components can show a group. */
export const groupAsBot = (group: Group): Bot => ({ id: group.id, name: group.name, color: group.color, shape: group.shape, instructions: '', createdAt: group.createdAt });

const formerMember = (id: string): Bot => ({ id, name: '삭제된 Bot', color: '#808080', instructions: '', createdAt: '' });

/** The member bot that wrote a group message (assistant messages only). */
export const speakerLookup = (data: AppState) => (message: Message) =>
  message.speakerBotId ? data.bots.find(bot => bot.id === message.speakerBotId) ?? formerMember(message.speakerBotId) : undefined;

/** The group conversation a group route shows: the routed one, else the newest. */
export const shownGroupConversation = (data: AppState | null, surface: GroupSurface) => {
  const conversations = groupConversations(data, surface.groupId);
  return conversations.find(item => item.id === surface.conversationId) ?? (surface.conversationId ? undefined : conversations[0]);
};

export const roundPending = (conversation: Conversation | undefined) => !!conversation?.groupRound?.pendingBotIds.length;

export function GroupList({ data, search, selectedId, busy, onOpen, onCreate, onEdit }: {
  data: AppState | null; search: string; selectedId: string | null; busy: boolean;
  onOpen: (group: Group) => void; onCreate: () => void; onEdit: (group: Group) => void;
}) {
  const [menu, setMenu] = useState<string | null>(null);
  useEffect(() => {
    if (!menu) return;
    const close = (event: MouseEvent) => { if (!(event.target as Element).closest('.group-actions')) setMenu(null); };
    const escape = (event: KeyboardEvent) => { if (event.key === 'Escape') setMenu(null); };
    document.addEventListener('click', close); document.addEventListener('keydown', escape);
    return () => { document.removeEventListener('click', close); document.removeEventListener('keydown', escape); };
  }, [menu]);
  if (!data) return null;
  const groups = (data.groups ?? []).filter(group => group.name.toLocaleLowerCase().includes(search.toLocaleLowerCase()));
  if (search && !groups.length) return null;
  return <section className="group-list" aria-label="그룹">
    <div className="sidebar-section-heading group-heading"><span>그룹</span><button type="button" onClick={onCreate} disabled={busy || data.bots.length < 2} title={data.bots.length < 2 ? '그룹을 만들려면 Bot이 2개 이상 필요합니다.' : undefined}><Plus size={12} />그룹 채팅 만들기</button></div>
    {groups.map(group => {
      const latest = groupConversations(data, group.id)[0];
      const running = data.conversations.some(conversation => conversation.groupId === group.id && (activeRun(conversation.run.status) || roundPending(conversation)));
      const members = group.memberBotIds.map(id => data.bots.find(bot => bot.id === id)).filter((bot): bot is Bot => !!bot);
      return <div className="bot-item group-item" key={group.id}>
        <button type="button" className={`bot-row group-row ${group.id === selectedId ? 'selected' : ''}`} aria-current={group.id === selectedId ? 'true' : undefined} title={group.name}
          onClick={() => onOpen(group)} onContextMenu={event => { event.preventDefault(); setMenu(group.id); }}>
          <span className="bot-avatar group-avatar"><Avatar color={group.color} seed={group.id} shape={group.shape} size={36} />
            <span className="group-member-stack" aria-hidden="true">{members.slice(0, 3).map(bot => <Avatar key={bot.id} color={bot.color} seed={bot.id} shape={bot.shape} size={14} />)}</span>
            {running && <span className="running-dot" />}</span>
          <span className="bot-copy"><span>{group.name}</span><small>{latest ? conversationPreview(latest) : `멤버 ${members.length}명`}</small></span>
        </button>
        <div className="bot-actions group-actions">
          <IconButton label={`추가 작업: ${group.name}`} onClick={() => setMenu(menu === group.id ? null : group.id)}><MoreHorizontal size={15} /></IconButton>
          {menu === group.id && <div className="bot-context" role="menu" aria-label={`${group.name} 작업`}>
            <button role="menuitem" disabled={busy} onClick={() => { setMenu(null); onEdit(group); }}><Pencil size={14} />그룹 편집</button>
          </div>}
        </div>
      </div>;
    })}
  </section>;
}

export function GroupDialog({ data, group, busy, error: serverError, run, onSnapshot, onClose, onCreated, onDeleted }: {
  data: AppState; group?: Group; busy: boolean; error: string | null; run: Run;
  onSnapshot: (state: AppState) => void; onClose: () => void; onCreated: (group: Group) => void; onDeleted: (group: Group) => void;
}) {
  const [name, setName] = useState(group?.name ?? '');
  const [color, setColor] = useState(group?.color ?? botColors[7]);
  const [shape, setShape] = useState<AvatarShape>(group?.shape ?? 'cloud');
  const [members, setMembers] = useState<string[]>(group?.memberBotIds.filter(id => data.bots.some(bot => bot.id === id)) ?? []);
  const [attempted, setAttempted] = useState(false);
  const error = attempted ? serverError : null;
  const errorRef = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => { if (error) errorRef.current?.scrollIntoView({ block: 'nearest' }); }, [error]);
  const initialMembers = group?.memberBotIds.join(',') ?? '';
  const dirty = name !== (group?.name ?? '') || color !== (group?.color ?? botColors[7]) || shape !== (group?.shape ?? 'cloud') || members.join(',') !== initialMembers;
  const close = () => { if (busy || (dirty && !window.confirm('저장하지 않은 그룹 설정을 버릴까요?'))) return; onClose(); };
  const move = (index: number, delta: number) => setMembers(current => { const next = [...current]; const [item] = next.splice(index, 1); next.splice(index + delta, 0, item); return next; });
  const others = data.bots.filter(bot => !members.includes(bot.id));
  const valid = !!name.trim() && members.length >= 2 && members.length <= 12;
  const save = async () => {
    setAttempted(true);
    const body: GroupInput = { name: name.trim(), color, shape, memberBotIds: members };
    const saved = await run('group', async () => {
      if (group) {
        const patch: Partial<GroupInput> = { name: body.name, color, shape };
        if (members.join(',') !== initialMembers) patch.memberBotIds = members;
        onSnapshot(await request<AppState>(`/api/groups/${encodeURIComponent(group.id)}`, { method: 'PATCH', body: patch }));
        return group;
      }
      const result = await request<{ group: Group; state: AppState }>('/api/groups', { method: 'POST', body });
      onSnapshot(result.state);
      return result.group;
    });
    if (saved) { if (!group) onCreated(saved); onClose(); }
  };
  const remove = async () => {
    if (!group || !window.confirm('이 그룹과 채팅 기록이 영구적으로 삭제돼요. 안의 Bot은 삭제되지 않고 각각 그대로 쓸 수 있어요. 되돌릴 수 없어요.')) return;
    setAttempted(true);
    const removed = await run('group-delete', async () => {
      onSnapshot(await request<AppState>(`/api/groups/${encodeURIComponent(group.id)}`, { method: 'DELETE', body: {} }));
      return true;
    });
    if (removed) { onDeleted(group); onClose(); }
  };
  const title = group ? '그룹 편집' : '그룹 채팅 만들기';
  return <Modal title={title} onClose={close} className="compact-modal bot-dialog group-dialog">
    <header className="modal-header"><h2>{title}</h2><IconButton label="그룹 설정 닫기" onClick={close} disabled={busy}><X size={18} /></IconButton></header>
    <form aria-busy={busy} onSubmit={event => { event.preventDefault(); if (valid && dirty && !busy) void save(); }}>
      <div className="bot-dialog-body">
        <div className="bot-dialog-preview" role="img" aria-label="그룹 아바타 미리보기"><Avatar color={color} seed={group?.id ?? name} size={80} shape={shape} /></div>
        <label className="bot-dialog-name"><span>그룹 이름</span>
          <input data-autofocus aria-label="그룹 이름" maxLength={100} required value={name} onChange={event => setName(event.target.value)} disabled={busy} placeholder="그룹 이름" autoComplete="off" /></label>
        <div className="bot-dialog-shapes" role="group" aria-label="그룹 아바타 편집">
          {avatarShapes.map(option => <button type="button" key={option.value} className="bot-dialog-shape" aria-label={`${option.label} 그룹 모양`} aria-pressed={shape === option.value} title={`${option.label} 그룹 모양`} onClick={() => setShape(option.value)} disabled={busy}><Avatar color={color} size={40} shape={option.value} /></button>)}
        </div>
        <div className="bot-dialog-colors" role="group" aria-label="그룹 색상">
          {botColors.map(option => <button type="button" key={option} className="bot-dialog-color" style={{ backgroundColor: option }} aria-label={`${option} 그룹 색상`} aria-pressed={color.toLowerCase() === option} onClick={() => setColor(option)} disabled={busy} />)}
        </div>
        <section className="group-members" aria-label="멤버">
          <h3>멤버 <small>답변 순서 · {members.length}명</small></h3>
          {members.length === 0 && <p className="muted">아래에서 Bot을 2개 이상 추가하세요.</p>}
          <ol>{members.map((id, index) => { const bot = data.bots.find(item => item.id === id); return <li key={id}>
            <span className="group-member-order">{index + 1}</span><Avatar color={bot?.color} seed={id} shape={bot?.shape} size={22} /><span className="group-member-name">{bot?.name ?? '삭제된 Bot'}</span>
            <IconButton label={`${bot?.name ?? 'Bot'} 위로`} disabled={busy || index === 0} onClick={() => move(index, -1)}><ArrowUp size={13} /></IconButton>
            <IconButton label={`${bot?.name ?? 'Bot'} 아래로`} disabled={busy || index === members.length - 1} onClick={() => move(index, 1)}><ArrowDown size={13} /></IconButton>
            <IconButton label={`${bot?.name ?? 'Bot'} 멤버에서 빼기`} disabled={busy} onClick={() => setMembers(members.filter(item => item !== id))}><X size={13} /></IconButton>
          </li>; })}</ol>
          {others.length > 0 && <div className="group-member-candidates" role="group" aria-label="멤버 추가">
            {others.map(bot => <button type="button" key={bot.id} className="small-button" disabled={busy || members.length >= 12} onClick={() => setMembers([...members, bot.id])} aria-label={`${bot.name} 멤버 추가`}><Plus size={12} /><Avatar color={bot.color} seed={bot.id} shape={bot.shape} size={16} />{bot.name}</button>)}
          </div>}
          <small className="muted">그룹에 보낸 메시지에는 멤버가 순서대로 한 번씩 답합니다. @이름으로 멘션하면 멘션한 멤버만 답합니다.</small>
        </section>
        {error && <div ref={errorRef}><ErrorNotice>{error}</ErrorNotice></div>}
      </div>
      <footer className="modal-footer">
        {group && <button type="button" className="small-button danger-button" onClick={() => void remove()} disabled={busy}><Trash2 size={14} />그룹 삭제</button>}
        <button type="button" className="small-button" onClick={close} disabled={busy}>취소</button>
        <button type="submit" className="primary-button" aria-label={group ? '그룹 저장' : '그룹 만들기'} disabled={busy || !valid || !dirty}>{busy ? '저장 중…' : group ? '그룹 저장' : '만들기'}</button>
      </footer>
    </form>
  </Modal>;
}

export function GroupView({ data, surface, busy, online, run, onSnapshot, onRoute, onSidebar, onEdit, onThread, draft, onDraft, onSettings }: {
  data: AppState; surface: GroupSurface; busy: boolean; online: boolean; run: Run;
  onSnapshot: (state: AppState) => void; onRoute: (surface: Surface) => void; onSidebar: () => void; onEdit: (group: Group) => void;
  onThread: (conversation: Conversation, threadId: string) => void;
  draft: (key: string) => string; onDraft: (key: string, text: string) => void; onSettings: () => void;
}) {
  const [history, setHistory] = useState(false);
  const group = data.groups?.find(item => item.id === surface.groupId);
  const conversations = group ? groupConversations(data, group.id) : [];
  const conversation = shownGroupConversation(data, surface);
  if (!group) return <div className="main-empty"><button type="button" className="mobile-only icon-button group-empty-menu" aria-label="봇 목록 열기" onClick={onSidebar}><Menu size={19} /></button><Users size={28} /><h1>삭제된 그룹</h1><p>이 그룹을 찾을 수 없습니다. 사이드바에서 다른 Bot이나 그룹을 선택하세요.</p></div>;
  const bot = groupAsBot(group);
  const members = group.memberBotIds.map(id => data.bots.find(item => item.id === id)).filter((item): item is Bot => !!item);
  const globalRun = data.conversations.find(item => activeRun(item.run.status));
  const pending = roundPending(conversation);
  const running = !!conversation && (activeRun(conversation.run.status) || pending);
  const key = `group:${group.id}:${conversation?.id ?? 'new'}`;
  const text = draft(key);
  const speakerFor = speakerLookup(data);
  const select = (conversationId: string | null) => onRoute({ kind: 'group', groupId: group.id, conversationId });
  const send = async () => {
    const message = text.trim();
    if (!message || running || globalRun) return;
    if (!data.settings.baseUrl || !data.settings.model) { onSettings(); return; }
    await run('group-message', async () => {
      let target = conversation?.id;
      if (!target) {
        const created = await request<{ conversation: Conversation; state: AppState }>(`/api/groups/${encodeURIComponent(group.id)}/conversations`, { method: 'POST', body: {} });
        onSnapshot(created.state);
        target = created.conversation.id;
        select(target);
      }
      onSnapshot(await request<AppState>(`/api/conversations/${encodeURIComponent(target)}/messages`, { method: 'POST', body: { text: message } }));
      onDraft(key, '');
    });
  };
  const cancel = async () => {
    if (!conversation || !running) return;
    await run('cancel', async () => onSnapshot(await request<AppState>(`/api/conversations/${encodeURIComponent(conversation.id)}/cancel`, { method: 'POST', body: {} })));
  };
  const permission = async (optionId: string) => {
    const pendingPermission = conversation?.run.permission;
    if (!conversation || !pendingPermission) return;
    await run('permission', async () => onSnapshot(await request<AppState>(`/api/conversations/${encodeURIComponent(conversation.id)}/permissions/${encodeURIComponent(pendingPermission.id)}`, { method: 'POST', body: { optionId } })));
  };
  const newConversation = async () => {
    await run('conversation', async () => {
      const created = await request<{ conversation: Conversation; state: AppState }>(`/api/groups/${encodeURIComponent(group.id)}/conversations`, { method: 'POST', body: {} });
      onSnapshot(created.state);
      select(created.conversation.id);
      setHistory(false);
    });
  };
  const waiting = (conversation?.groupRound?.pendingBotIds ?? []).map(id => data.bots.find(item => item.id === id)?.name).filter(Boolean);
  const mention = (name: string) => onDraft(key, `${text}${text && !/\s$/.test(text) ? ' ' : ''}@${name} `);
  return <>
    <header className="chat-header group-header">
      <div className="chat-identity-wrap">
        <IconButton label="봇 목록 열기" className="mobile-only" onClick={onSidebar}><Menu size={19} /></IconButton>
        <button type="button" className="chat-identity" aria-label="그룹 편집" onClick={() => onEdit(group)} disabled={busy}>
          <Avatar color={group.color} seed={group.id} shape={group.shape} size={24} /><span>{group.name}</span>
          <span className="group-header-members" aria-label={`멤버 ${members.length}명`}>{members.slice(0, 4).map(member => <Avatar key={member.id} color={member.color} seed={member.id} shape={member.shape} size={16} />)}</span>
        </button>
      </div>
      <div className="chat-controls">
        <IconButton label="대화 기록" onClick={() => setHistory(true)}><Clock3 size={17} /></IconButton>
        <IconButton label="새 대화" onClick={() => void newConversation()} disabled={busy || !!globalRun}><SquarePen size={17} /></IconButton>
      </div>
    </header>
    {members.length < 2 && <div className="group-warning" role="status"><span>멤버가 {members.length}명입니다. 그룹 편집에서 Bot을 2개 이상 추가하세요.</span><button type="button" onClick={() => onEdit(group)}>그룹 편집</button></div>}
    <div className="group-chat">
      <Transcript bot={bot} conversation={conversation} data={data} speakerFor={speakerFor}
        messageExtras={message => conversation && <ThreadLinks data={data} conversation={conversation} message={message} busy={busy} run={run} onSnapshot={onSnapshot} onOpen={threadId => onThread(conversation, threadId)} />}
        modelConfigured={!!data.settings.baseUrl && !!data.settings.model} runtimeAvailable={data.runtime.available} runtimeError={data.runtime.error}
        busy={busy} onSettings={onSettings} onRuntimeSettings={onSettings} onPermission={optionId => void permission(optionId)} />
    </div>
    {pending && waiting.length > 0 && <p className="group-queue" role="status"><Clock3 size={13} />다음 차례: {waiting.join(', ')}</p>}
    <div className="group-mentions" role="group" aria-label="멘션">
      <AtSign size={13} aria-hidden="true" />
      {members.length ? members.map(member => <button type="button" key={member.id} onClick={() => mention(member.name)} aria-label={`${member.name} 멘션`}><Avatar color={member.color} seed={member.id} shape={member.shape} size={14} />{member.name}</button>) : <span>아직 멘션할 대상이 없어요</span>}
    </div>
    <Composer botName={group.name} placeholder="그룹에 메시지 보내기" value={text} onChange={value => onDraft(key, value)} onSend={() => void send()}
      onCancel={() => void cancel()} running={running} disabled={!online || !!globalRun || pending || members.length === 0} busy={busy} model={data.settings.model} />
    {history && <History bot={bot} conversations={conversations} selectedId={conversation?.id ?? null} busy={busy || !!globalRun} onClose={() => setHistory(false)} onNew={() => void newConversation()} onSelect={id => { select(id); setHistory(false); }} />}
  </>;
}
