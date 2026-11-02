import { useEffect, useLayoutEffect, useRef, type FormEvent, type ReactNode } from 'react';
import {
  ArrowDown,
  ArrowUp,
  CheckCircle2,
  ChevronDown,
  Circle,
  Clock3,
  FileText,
  LoaderCircle,
  Plus,
  ShieldQuestion,
  Square,
  Terminal,
  XCircle,
} from 'lucide-react';
import type {
  AppState,
  Bot,
  Conversation,
  Message,
  PendingPermission,
  RoutineRun,
  ToolActivity,
} from '../shared/types';
import { activeRun, runLabels } from './state';
import { Avatar, ErrorNotice, IconButton } from './ui';
import { BotMessageLinks, BotReportMarker } from './BotChat';
import { RichMarkdown } from './RichMarkdown';
import { TodoList } from './TodoList';
import { AttachButton, AttachmentTray, DropOverlay, MessageAttachments, useFileDrop, type ComposerAttachments } from './Attachments';

const toolLabels = {
  pending: '대기',
  in_progress: '실행 중',
  completed: '완료',
  failed: '실패',
} as const;
/** "생성됨 루틴 · <이름> 루틴 열기" under a message whose routine tool finished. */
const ROUTINE_RESULT = /^(생성됨|업데이트됨|일시 중지됨|재개됨) 루틴: (.+)$/;
function RoutineLinks({ message, onOpen }: { message: Message; onOpen: () => void }) {
  const changes = message.tools.flatMap((tool) => {
    const match = tool.status === 'completed' && tool.output ? ROUTINE_RESULT.exec(tool.output) : null;
    return match ? [{ id: tool.id, label: match[1], name: match[2] }] : [];
  });
  return <>{changes.map((change) => (
    <div className="bot-exchange-summary routine-change-summary" key={change.id}>
      <Clock3 size={13} /><span>{change.label} 루틴 ·</span>
      <button type="button" onClick={onOpen} aria-label={`${change.name} 루틴 열기`}>{change.name}</button>
    </div>
  ))}</>;
}

function ToolCard({ tool }: { tool: ToolActivity }) {
  const StatusIcon =
    tool.status === 'completed'
      ? CheckCircle2
      : tool.status === 'failed'
        ? XCircle
        : tool.status === 'in_progress'
          ? LoaderCircle
          : Circle;
  return (
    <details className={`tool-card tool-${tool.status}`}>
      <summary>
        <StatusIcon size={14} className={tool.status === 'in_progress' ? 'spin' : ''} />
        <span>{tool.title || tool.kind || '도구 작업'}</span>
        {tool.approvedByRule && <span className="tool-rule-badge">내 규칙으로 자동 승인됨</span>}
        <small>{toolLabels[tool.status]}</small>
        <ChevronDown size={13} />
      </summary>
      <div className="tool-details">
        {tool.paths?.length ? (
          <div className="tool-paths">
            {tool.paths.map((path, index) => (
              <code key={`${path}-${index}`}>
                <FileText size={12} />
                {path}
              </code>
            ))}
          </div>
        ) : null}
        {tool.input && (
          <div>
            <h4>입력</h4>
            <pre>{tool.input}</pre>
          </div>
        )}
        {tool.output && (
          <div>
            <h4>출력</h4>
            <pre>{tool.output}</pre>
          </div>
        )}
        {!tool.input && !tool.output && !tool.paths?.length && (
          <p className="muted">추가 정보가 없습니다.</p>
        )}
      </div>
    </details>
  );
}
function MessageView({ message, bot, conversationId, named }: { message: Message; bot: Bot; conversationId?: string; named?: boolean }) {
  // Successful native plan updates are represented once by the latest checklist, not tool history.
  const tools = message.tools.filter(tool => tool.kind !== 'think' || tool.title !== 'Updating plan' || tool.status === 'failed');
  return (
    <article
      className={`message-row ${message.role}`}
      aria-label={message.role === 'user' ? '내 메시지' : `${bot.name} 메시지`}
    >
      {message.role === 'assistant' && <Avatar color={bot.color} seed={bot.id} shape={bot.shape} size={25} />}
      <div className="message-content">
        {named && message.role === 'assistant' && <strong className="message-speaker" style={{ color: bot.color === '#ffffff' ? undefined : bot.color }}>{bot.name}</strong>}
        {conversationId && !!message.attachments?.length && (
          <MessageAttachments conversationId={conversationId} attachments={message.attachments} />
        )}
        {message.text && (
          <div className="message-bubble">
            {message.role === 'assistant' ? (
              <RichMarkdown text={message.text} />
            ) : (
              <p>{message.text}</p>
            )}
          </div>
        )}
        {tools.length > 0 && (
          <div className="tool-stack">
            {tools.map((tool) => (
              <ToolCard key={tool.id} tool={tool} />
            ))}
          </div>
        )}
        {(message.text || tools.length > 0 || !!message.attachments?.length) && (
          <time className="message-time" dateTime={message.createdAt}>
            {formatTime(message.createdAt)}
          </time>
        )}
      </div>
    </article>
  );
}
function PermissionCard({
  permission,
  busy,
  onChoose,
}: {
  permission: PendingPermission;
  busy: boolean;
  onChoose: (optionId: string) => void;
}) {
  return (
    <section className="permission-card" aria-label="작업 승인 요청">
      <header>
        <ShieldQuestion size={19} />
        <div>
          <h3>{permission.title || '작업 승인이 필요합니다'}</h3>
          <small>{permission.kind} · 승인 대기</small>
        </div>
      </header>
      {permission.description && (
        <pre className="permission-description">{permission.description}</pre>
      )}
      {permission.paths.length > 0 && (
        <div className="permission-paths">
          <h4>대상 경로</h4>
          {permission.paths.map((path, index) => (
            <code key={`${path}-${index}`}>
              <FileText size={13} />
              {path}
            </code>
          ))}
        </div>
      )}
      <p className="permission-hint">
        {permission.localOnly ? '서버의 localhost 연결에서 페이지 정보 공유와 브라우저 조작을 한 번만 승인합니다.' : '입력·출력과 경로를 확인한 뒤 선택하세요. 승인한 작업은 서버에서 실행됩니다.'}
      </p>
      <div className="permission-actions">
        {permission.options.map((option) => (
          <button
            type="button"
            key={option.id}
            className={option.kind.startsWith('allow') ? 'primary-button' : 'small-button'}
            data-option-id={option.id}
            disabled={busy}
            onClick={() => onChoose(option.id)}
          >
            {option.label}
          </button>
        ))}
      </div>
    </section>
  );
}
const dateKey = (date: string) =>
  Number.isNaN(new Date(date).getTime())
    ? ''
    : new Date(date).toLocaleDateString('ko-KR', {
        year: 'numeric',
        month: 'long',
        day: 'numeric',
      });
function formatTime(date: string) {
  return Number.isNaN(new Date(date).getTime())
    ? ''
    : new Date(date).toLocaleTimeString('ko-KR', { hour: 'numeric', minute: '2-digit' });
}

export function Transcript({
  bot,
  conversation,
  queuedRoutine,
  modelConfigured,
  runtimeAvailable,
  runtimeError,
  busy,
  onSettings,
  onRuntimeSettings,
  onPermission,
  data,
  onBotChat,
  onCancelReports,
  cancelReportsDisabled,
  reportNotice,
  onRoutines,
  speakerFor,
  messageExtras,
}: {
  bot: Bot;
  conversation?: Conversation;
  queuedRoutine?: RoutineRun;
  modelConfigured: boolean;
  runtimeAvailable: boolean;
  runtimeError?: string;
  busy: boolean;
  onSettings: () => void;
  onRuntimeSettings: () => void;
  onPermission: (optionId: string) => void;
  data?: AppState;
  onBotChat?: (id: string) => void;
  onCancelReports?: (conversationId: string, deliveryIds: string[]) => void;
  cancelReportsDisabled?: boolean;
  reportNotice?: string;
  onRoutines?: () => void;
  /** Group conversations: the member bot that wrote an assistant message. */
  speakerFor?: (message: Message) => Bot | undefined;
  /** Extra controls under a message (thread links). */
  messageExtras?: (message: Message) => ReactNode;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const nearBottom = useRef(true);
  const bottomButton = useRef<HTMLButtonElement>(null);
  const conversationId = conversation?.id ?? bot.id;
  const messages = conversation?.messages ?? [];
  const tail = messages.at(-1);
  const scrollBottom = () => {
    const element = scrollRef.current;
    if (!element) return;
    element.scrollTop = element.scrollHeight;
    nearBottom.current = true;
    if (bottomButton.current) bottomButton.current.hidden = true;
  };
  useLayoutEffect(() => {
    nearBottom.current = true;
    scrollBottom();
  }, [conversationId]);
  useLayoutEffect(() => {
    if (nearBottom.current) scrollBottom();
  }, [
    messages.length,
    tail?.text,
    tail?.tools,
    conversation?.run.status,
    conversation?.run.permission,
    conversation?.run.todos,
  ]);
  useEffect(() => {
    if (!scrollRef.current || typeof ResizeObserver === 'undefined') return;
    const content = scrollRef.current.firstElementChild;
    if (!content) return;
    const observer = new ResizeObserver(() => {
      if (nearBottom.current) scrollBottom();
    });
    observer.observe(content);
    return () => observer.disconnect();
  }, []);
  const running = conversation && activeRun(conversation.run.status);
  const status = conversation?.run.status;
  return (
    <div className="transcript-container">
      <div
        ref={scrollRef}
        className="transcript"
        role="log"
        aria-label="대화 메시지"
        aria-live="polite"
        aria-relevant="additions text"
        onScroll={(event) => {
          const element = event.currentTarget;
          nearBottom.current = element.scrollHeight - element.scrollTop - element.clientHeight < 96;
          if (bottomButton.current) bottomButton.current.hidden = nearBottom.current;
        }}
      >
        <div className="transcript-inner">
          {messages.length > 0 && (
            <div className="conversation-label">{conversation?.title || '새 대화'}</div>
          )}
          {messages.length === 0 && (
            <div className="conversation-empty">
              <Avatar size={54} color={bot.color} seed={bot.id} shape={bot.shape} />
              <h1>{bot.name}</h1>
              <p>
                {queuedRoutine ? '다른 작업이 끝나면 이 루틴을 실행합니다.' : '무엇을 함께 해볼까요?'}
              </p>
              {queuedRoutine ? (
                <small className="queued-routine-prompt">{queuedRoutine.prompt}</small>
              ) : !modelConfigured ? (
                <button type="button" className="setup-hint" onClick={onSettings}>
                  모델을 연결하고 첫 대화를 시작하세요 <ArrowUp size={14} />
                </button>
              ) : (
                <small>대화를 입력하거나 컴퓨터에서 작업 파일을 열어보세요.</small>
              )}
            </div>
          )}
          {messages.map((message, index) => (
            <div key={message.id}>
              {dateKey(message.createdAt) !== dateKey(messages[index - 1]?.createdAt ?? '') && (
                <div className="date-label">{dateKey(message.createdAt)}</div>
              )}
              {message.reportDeliveryIds ? (
                // The input of an automatic follow-up turn is app-written data: show only a marker.
                <BotReportMarker message={message} data={data} onOpen={onBotChat} />
              ) : (
                <>
                  {message === tail && message.role === 'assistant' && !!conversation?.run.todos?.length && (
                    <TodoList key={`${conversationId}-${conversation.run.id}`} todos={conversation.run.todos} running={!!running} />
                  )}
                  <MessageView message={message} bot={speakerFor?.(message) ?? bot} conversationId={conversation?.id} named={!!speakerFor} />
                  {data && onBotChat && <BotMessageLinks message={message} data={data} onOpen={onBotChat} onCancelReports={onCancelReports} cancelReportsDisabled={cancelReportsDisabled} />}
                  {onRoutines && <RoutineLinks message={message} onOpen={onRoutines} />}
                  {messageExtras?.(message)}
                </>
              )}
            </div>
          ))}
          {reportNotice && <p className="run-indicator" role="status">{reportNotice}</p>}
          {queuedRoutine && (
            <p className="run-indicator" role="status">
              <Clock3 size={14} />루틴 실행 대기 중
            </p>
          )}
          {running && (
            <div className="run-indicator" role="status">
              {status === 'awaiting_permission' ? (
                <ShieldQuestion size={14} />
              ) : (
                <LoaderCircle size={14} className="spin" />
              )}
              <span>{runLabels[conversation.run.status]}</span>
            </div>
          )}
          {conversation?.run.permission && status === 'awaiting_permission' && (
            <PermissionCard
              permission={conversation.run.permission}
              busy={busy}
              onChoose={onPermission}
            />
          )}
          {(status === 'error' || status === 'interrupted') && (
            <ErrorNotice>
              {conversation?.run.error ||
                (status === 'interrupted'
                  ? '작업이 중단되었습니다. 다시 메시지를 보내 이어갈 수 있습니다.'
                  : '에이전트 작업을 실행하지 못했습니다.')}
            </ErrorNotice>
          )}
          {status === 'cancelled' && (
            <p className="terminal-note" role="status">
              <Square size={12} />
              작업을 중단했습니다.
            </p>
          )}
          {status === 'completed' && (
            <p className="terminal-note" role="status">
              <CheckCircle2 size={13} />
              작업 완료
            </p>
          )}
          {!runtimeAvailable && (
            <div className="runtime-notice">
              <Terminal size={16} />
              <div>
                <strong>앱 엔진을 사용할 수 없습니다.</strong>
                <span>
                  {runtimeError || '서버 컴퓨터의 앱 폴더에서 npm run build:engine 명령으로 엔진을 빌드한 뒤 서버를 다시 시작해 주세요.'}
                </span>
              </div>
              <button type="button" className="text-button" onClick={onRuntimeSettings}>
                설정 확인
              </button>
            </div>
          )}
        </div>
      </div>
      <button
        ref={bottomButton}
        type="button"
        className="scroll-bottom"
        aria-label="최신 메시지로 이동"
        hidden
        onClick={scrollBottom}
      >
        <ArrowDown size={16} />
      </button>
    </div>
  );
}

export function Composer({
  botName,
  value,
  onChange,
  onSend,
  onFiles,
  onCancel,
  running,
  disabled,
  busy,
  model,
  attachments,
  placeholder,
  label = '메시지',
}: {
  botName: string;
  value: string;
  onChange: (value: string) => void;
  onSend: () => void;
  /** Omitted where no workspace panel exists (group chats, threads). */
  onFiles?: () => void;
  onCancel: () => void;
  running: boolean;
  disabled: boolean;
  busy: boolean;
  model: string;
  /** Pending files of this draft; omitted where attachments are not offered. */
  attachments?: ComposerAttachments;
  placeholder?: string;
  label?: string;
}) {
  const textarea = useRef<HTMLTextAreaElement>(null);
  const attachable = !!attachments && !disabled && !busy && !running;
  const drop = useFileDrop(attachable, (files) => attachments?.add(files));
  const hasFiles = !!attachments?.items.length;
  useLayoutEffect(() => {
    if (!textarea.current) return;
    textarea.current.style.height = '0px';
    textarea.current.style.height = `${Math.min(152, Math.max(28, textarea.current.scrollHeight))}px`;
  }, [value]);
  const submit = (event?: FormEvent) => {
    event?.preventDefault();
    if (!disabled && (value.trim() || hasFiles) && !busy && !running) onSend();
  };
  return (
    <div className="composer-dock" {...drop.handlers}>
      <DropOverlay show={drop.dragging} />
      {attachments && (
        <AttachmentTray items={attachments.items} errors={attachments.errors} disabled={busy} onRemove={attachments.remove} onDismiss={attachments.dismiss} />
      )}
      <form className="composer" onSubmit={submit}>
        {onFiles && (
          <IconButton label="작업 폴더 열기" className="attach-button" onClick={onFiles}>
            <Plus size={20} />
          </IconButton>
        )}
        {attachments && <AttachButton disabled={!attachable} onFiles={attachments.add} />}
        <textarea
          ref={textarea}
          rows={1}
          aria-label={label}
          placeholder={placeholder ?? `${botName}에게 메시지 보내기`}
          value={value}
          onChange={(event) => onChange(event.target.value)}
          onPaste={(event) => {
            const files = [...event.clipboardData.files];
            if (!attachable || !files.length) return;
            event.preventDefault();
            attachments?.add(files);
          }}
          onKeyDown={(event) => {
            if (
              event.key === 'Enter' &&
              !event.shiftKey &&
              !event.nativeEvent.isComposing &&
              event.keyCode !== 229
            ) {
              event.preventDefault();
              submit();
            }
          }}
        />
        {running ? (
          <IconButton
            label="작업 중단"
            className="send-button stop-button"
            onClick={onCancel}
            disabled={busy}
          >
            <Square size={14} fill="currentColor" />
          </IconButton>
        ) : (
          <button
            type="submit"
            className="send-button"
            aria-label="메시지 보내기"
            title="메시지 보내기"
            disabled={disabled || busy || (!value.trim() && !hasFiles)}
          >
            <ArrowUp size={19} />
          </button>
        )}
      </form>
      <div className="composer-meta">
        <span title={model}>{model || '모델을 설정해 주세요'}</span>
        <span>Enter 전송 · Shift+Enter 줄바꿈</span>
      </div>
    </div>
  );
}
