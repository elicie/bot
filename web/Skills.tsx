import { useEffect, useId, useLayoutEffect, useRef, useState, type FormEvent } from 'react';
import { ArrowLeft, ChevronRight, FileText, LockKeyhole, MessageSquare, Plug, Plus, Trash2, X } from 'lucide-react';
import type { AppState, Bot, Skill } from '../shared/types';
import { ApiError, errorText, request } from './api';
import { Avatar, ErrorNotice, IconButton, Modal } from './ui';
import './skills.css';

interface SkillDraft {
  name: string;
  description: string;
  instructions: string;
}
interface SkillEditor {
  initial: Skill | null;
  draft: SkillDraft;
}
type SkillsTab = 'skills' | 'connectors';
type SkillOperation = { kind: 'save' | 'delete' | 'toggle' | 'import'; id?: string };
type SkillValidation = Partial<Record<keyof SkillDraft, string>>;

const instructionLimit = 30000;
const skillBytes = (draft: SkillDraft) => new TextEncoder().encode(JSON.stringify({
  name: draft.name.trim(), description: draft.description.trim(), instructions: draft.instructions,
})).byteLength;
const maximumSkillBytes = 18 * 1024;
const navigationGuards = new Set<() => boolean>();
const tabs = [
  { id: 'skills', label: '내 플러그인', icon: FileText },
  { id: 'connectors', label: '커넥터', icon: Plug },
] as const;

// Parent-driven navigation must check this before replacing the overlay.
export function confirmSkillsNavigation() {
  for (const guard of navigationGuards) if (!guard()) return false;
  return true;
}

function skillDraft(skill?: Skill | null): SkillDraft {
  return {
    name: skill?.name ?? '',
    description: skill?.description ?? '',
    instructions: skill?.instructions ?? '',
  };
}
function draftIsDirty(editor: SkillEditor | null) {
  if (!editor) return false;
  const initial = skillDraft(editor.initial);
  return (Object.keys(initial) as (keyof SkillDraft)[]).some((key) => initial[key] !== editor.draft[key]);
}
function validateDraft(draft: SkillDraft): SkillValidation {
  const errors: SkillValidation = {};
  if (!draft.name.trim()) errors.name = '스킬 이름을 입력해 주세요.';
  if (!draft.description.trim()) errors.description = '스킬 설명을 입력해 주세요.';
  if (!draft.instructions.trim()) errors.instructions = '스킬 지침을 입력해 주세요.';
  else if (draft.instructions.length > instructionLimit)
    errors.instructions = '스킬 지침은 최대 30,000자까지 저장할 수 있습니다. 내용을 줄여 주세요.';
  else if (skillBytes(draft) > maximumSkillBytes)
    errors.instructions = '스킬 전체 텍스트는 JSON 인코딩 기준 18 KiB까지 저장할 수 있습니다. 지침을 줄여 다시 저장해 주세요. 초안은 유지됩니다.';
  if (draft.name.trim().length > 100) errors.name = '스킬 이름은 최대 100자까지 입력해 주세요.';
  if (draft.description.trim().length > 2000) errors.description = '스킬 설명은 최대 2,000자까지 입력해 주세요.';
  return errors;
}

export function Skills({ bot, busy, onClose, onChange, onCreateWithBot }: {
  bot: Bot;
  busy: boolean;
  onClose: () => void;
  onChange: (data: AppState) => void;
  onCreateWithBot: () => void;
}) {
  const [tab, setTab] = useState<SkillsTab>('skills');
  const [editor, setEditor] = useState<SkillEditor | null>(null);
  const [pending, setPending] = useState<SkillOperation | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [validation, setValidation] = useState<SkillValidation>({});
  const [status, setStatus] = useState<string | null>(null);
  const [horizontalTabs, setHorizontalTabs] = useState(() => window.matchMedia('(max-width: 540px)').matches);
  const prefix = useId();
  const nameInput = useRef<HTMLInputElement>(null);
  const importInput = useRef<HTMLInputElement>(null);
  const descriptionInput = useRef<HTMLTextAreaElement>(null);
  const instructionsInput = useRef<HTMLTextAreaElement>(null);
  const errorRef = useRef<HTMLDivElement>(null);
  const panelRef = useRef<HTMLDivElement>(null);
  const addButton = useRef<HTMLButtonElement>(null);
  const tabButtons = useRef<Partial<Record<SkillsTab, HTMLButtonElement | null>>>({});
  const previousEditor = useRef<string | null>(null);
  const returnToSkill = useRef<string | null>(null);
  const requestLock = useRef(false);
  const mounted = useRef(true);
  const leaving = useRef(false);
  const navigationGuard = useRef<() => boolean>(() => true);
  const dirty = draftIsDirty(editor);
  const unloadState = useRef({ dirty });
  unloadState.current = { dirty };
  const skills = bot.skills ?? [];
  const latest = editor?.initial ? skills.find((skill) => skill.id === editor.initial?.id) : undefined;
  const removed = !!editor?.initial && !latest;
  const stale = !!editor?.initial && !!latest && latest.revision !== editor.initial.revision;
  const editorKey = editor ? editor.initial?.id ?? 'new' : null;
  const errors = [...Object.values(validation).filter((message): message is string => !!message), ...(error ? [error] : [])];
  const blocked = busy || !!pending;

  const confirmLeave = () => {
    if (leaving.current) return true;
    if (requestLock.current) {
      setError('스킬 변경 요청을 처리 중입니다. 요청이 끝난 뒤 이동해 주세요.');
      return false;
    }
    return !dirty || window.confirm('저장하지 않은 스킬 변경 사항을 버리고 이동할까요?');
  };
  navigationGuard.current = confirmLeave;

  useLayoutEffect(() => {
    mounted.current = true;
    const guard = () => navigationGuard.current();
    navigationGuards.add(guard);
    return () => {
      mounted.current = false;
      navigationGuards.delete(guard);
    };
  }, []);
  useEffect(() => {
    const query = window.matchMedia('(max-width: 540px)');
    const update = () => setHorizontalTabs(query.matches);
    query.addEventListener('change', update);
    update();
    return () => query.removeEventListener('change', update);
  }, []);
  useEffect(() => {
    const beforeUnload = (event: BeforeUnloadEvent) => {
      if (!unloadState.current.dirty && !requestLock.current) return;
      event.preventDefault();
      event.returnValue = '';
    };
    window.addEventListener('beforeunload', beforeUnload);
    return () => window.removeEventListener('beforeunload', beforeUnload);
  }, []);
  useEffect(() => {
    if (pending) return;
    setEditor((before) => {
      if (!before?.initial || draftIsDirty(before)) return before;
      const current = bot.skills?.find((skill) => skill.id === before.initial?.id);
      if (!current || current.revision === before.initial.revision) return before;
      return { initial: current, draft: skillDraft(current) };
    });
  }, [bot.skills, pending]);
  useLayoutEffect(() => {
    if (editorKey !== null) nameInput.current?.focus();
    else if (previousEditor.current !== null && tab === 'skills') {
      const row = [...(panelRef.current?.querySelectorAll<HTMLButtonElement>('[data-skill-id]') ?? [])]
        .find((button) => button.dataset.skillId === returnToSkill.current);
      (row ?? addButton.current)?.focus();
    }
    previousEditor.current = editorKey;
  }, [editorKey, tab]);
  useLayoutEffect(() => {
    if (error || Object.keys(validation).length) errorRef.current?.scrollIntoView({ block: 'nearest' });
  }, [error, validation]);

  const clearFeedback = () => {
    setError(null);
    setValidation({});
    setStatus(null);
  };
  const leave = (action: () => void) => {
    if (!confirmLeave()) return;
    leaving.current = true;
    try { action(); } finally { leaving.current = false; }
  };
  const changeTab = (next: SkillsTab) => {
    if (next === tab) return true;
    if (!confirmLeave()) return false;
    clearFeedback();
    setEditor(null);
    setTab(next);
    return true;
  };
  const edit = (skill?: Skill) => {
    if (!confirmLeave()) return;
    clearFeedback();
    returnToSkill.current = skill?.id ?? null;
    setEditor({ initial: skill ?? null, draft: skillDraft(skill) });
  };
  const back = () => {
    if (!confirmLeave()) return;
    clearFeedback();
    setEditor(null);
  };
  const updateDraft = (field: keyof SkillDraft, value: string) => {
    setEditor((before) => before ? { ...before, draft: { ...before.draft, [field]: value } } : before);
    setValidation((before) => {
      const next = { ...before };
      delete next[field];
      return next;
    });
  };
  const mutate = async (
    operation: SkillOperation,
    path: string,
    method: 'POST' | 'PATCH' | 'DELETE',
    body: unknown,
    message: string,
  ) => {
    if (busy || requestLock.current) return false;
    requestLock.current = true;
    setPending(operation);
    setError(null);
    setStatus(null);
    try {
      const data = await request<AppState>(path, { method, body });
      onChange(data);
      if (mounted.current) setStatus(message);
      return true;
    } catch (reason) {
      if (mounted.current) setError(
        reason instanceof ApiError && reason.status === 409
          ? `${errorText(reason)}\n최신 내용을 덮어쓰지 않았습니다. ${operation.kind === 'toggle'
            ? '목록의 최신 상태를 확인한 뒤 다시 시도해 주세요.'
            : '초안은 그대로 유지됩니다. 최신 내용을 다시 불러오기 전에 필요한 내용을 복사해 주세요.'}`
          : errorText(reason),
      );
      return false;
    } finally {
      requestLock.current = false;
      if (mounted.current) setPending(null);
    }
  };
  const root = `/api/bots/${encodeURIComponent(bot.id)}/skills`;
  const importMarkdown = async (file: File) => {
    if (blocked || requestLock.current) return;
    clearFeedback();
    if (file.size > 20 * 1024) {
      setError('SKILL.md 파일은 최대 20 KiB입니다. 파일을 줄여 다시 선택해 주세요.');
      return;
    }
    requestLock.current = true;
    setPending({ kind: 'import' });
    try {
      const markdown = new TextDecoder('utf-8', { fatal: true }).decode(await file.arrayBuffer());
      const data = await request<AppState>(`${root}/import`, { method: 'POST', body: { markdown } });
      onChange(data);
      if (mounted.current) setStatus('SKILL.md에서 비공개 스킬을 가져왔습니다.');
    } catch (reason) {
      if (mounted.current) setError(reason instanceof TypeError ? 'SKILL.md 파일은 UTF-8 텍스트여야 합니다.' : errorText(reason));
    } finally {
      requestLock.current = false;
      if (mounted.current) setPending(null);
    }
  };
  const save = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (!editor || blocked || requestLock.current || removed) return;
    const invalid = validateDraft(editor.draft);
    setValidation(invalid);
    if (Object.keys(invalid).length) {
      setError(null);
      if (invalid.name) nameInput.current?.focus();
      else if (invalid.description) descriptionInput.current?.focus();
      else instructionsInput.current?.focus();
      return;
    }
    if (editor.initial && !dirty) return;
    const body = {
      name: editor.draft.name.trim(),
      description: editor.draft.description.trim(),
      instructions: editor.draft.instructions,
      ...(editor.initial ? { revision: editor.initial.revision } : {}),
    };
    const saved = await mutate(
      { kind: 'save', id: editor.initial?.id },
      editor.initial ? `${root}/${encodeURIComponent(editor.initial.id)}` : root,
      editor.initial ? 'PATCH' : 'POST',
      body,
      `“${body.name}” 스킬을 저장했습니다.`,
    );
    if (saved && mounted.current) {
      setValidation({});
      setEditor(null);
    }
  };
  const remove = async () => {
    const skill = editor?.initial;
    if (!skill || blocked || requestLock.current || removed) return;
    if (!window.confirm(
      `“${skill.name}” 스킬을 “${bot.name}” 봇에서 삭제할까요?\n이 작업은 되돌릴 수 없습니다.${dirty ? '\n저장하지 않은 편집 내용도 버려집니다.' : ''}`,
    )) return;
    const deleted = await mutate(
      { kind: 'delete', id: skill.id }, `${root}/${encodeURIComponent(skill.id)}`,
      'DELETE', { revision: skill.revision }, `“${skill.name}” 스킬을 삭제했습니다.`,
    );
    if (deleted && mounted.current) {
      setValidation({});
      setEditor(null);
    }
  };

  return (
    <Modal title="앱 연결" className="skills-modal" onClose={() => leave(onClose)}>
      <header className="skills-header">
        <div><Plug size={18} aria-hidden="true" /><h2>앱 연결</h2></div>
        <IconButton label="앱 연결 닫기" onClick={() => leave(onClose)} disabled={!!pending}>
          <X size={18} />
        </IconButton>
      </header>
      <div className="skills-shell">
        <aside className="skills-rail">
          <div className="skills-tabs" role="tablist" aria-label="앱 연결 메뉴" aria-orientation={horizontalTabs ? 'horizontal' : 'vertical'}>
            {tabs.map((item, index) => (
              <button
                type="button" role="tab" key={item.id} id={`${prefix}-tab-${item.id}`}
                aria-selected={tab === item.id} aria-controls={`${prefix}-panel-${item.id}`}
                tabIndex={tab === item.id ? 0 : -1} disabled={!!pending}
                ref={(button) => { tabButtons.current[item.id] = button; }}
                onClick={() => changeTab(item.id)}
                onKeyDown={(event) => {
                  let next: number;
                  if (event.key === 'ArrowDown' || event.key === 'ArrowRight') next = (index + 1) % tabs.length;
                  else if (event.key === 'ArrowUp' || event.key === 'ArrowLeft') next = (index + tabs.length - 1) % tabs.length;
                  else if (event.key === 'Home') next = 0;
                  else if (event.key === 'End') next = tabs.length - 1;
                  else return;
                  event.preventDefault();
                  if (changeTab(tabs[next].id)) tabButtons.current[tabs[next].id]?.focus();
                }}
              >
                <item.icon size={16} aria-hidden="true" /><span>{item.label}</span>
                {item.id === 'skills' && <small>{skills.length}</small>}
              </button>
            ))}
          </div>
          <div className="skills-rail-scope"><LockKeyhole size={13} aria-hidden="true" /><span>선택한 봇의 비공개 스킬</span></div>
        </aside>
        <div className="skills-workspace">
          <div className="skills-scope" aria-label="스킬 적용 대상 봇">
            <Avatar color={bot.color} seed={bot.id} shape={bot.shape} size={25} />
            <span><strong>{bot.name}</strong><small>이 봇에만 적용 · 비공개</small></span>
          </div>
          <div className="skills-panel" ref={panelRef} role="tabpanel" id={`${prefix}-panel-${tab}`} aria-labelledby={`${prefix}-tab-${tab}`} aria-busy={!!pending}>
            {editor ? (
              <form className="skills-editor" noValidate onSubmit={(event) => void save(event)} aria-label={editor.initial ? '비공개 스킬 편집' : '스킬 추가'}>
                <div className="skills-editor-heading">
                  <IconButton label="스킬 목록으로 뒤로" onClick={back} disabled={!!pending}><ArrowLeft size={18} /></IconButton>
                  <div><h3>{editor.initial ? '비공개 스킬 편집' : '스킬 추가'}</h3><p>{editor.initial ? `편집 기준 버전 ${editor.initial.revision}` : '이 봇만 사용할 스킬을 직접 작성합니다.'}</p></div>
                </div>
                {!editor.initial && <p className="skills-local-note"><strong>로컬 확장 · 직접 추가</strong>이름, 사용 조건과 Markdown 지침을 직접 작성합니다. SKILL.md 파일은 목록의 ‘SKILL.md 가져오기’에서 이름과 설명을 읽어 저장할 수 있습니다. 지침은 기존 권한 안에서 적용됩니다.</p>}
                {(stale || removed) && (
                  <div className="skills-stale" role="alert">
                    <p>{removed
                      ? '다른 곳에서 이 스킬이 삭제되었습니다. 편집 내용은 유지되지만 이 스킬을 저장하거나 삭제할 수 없습니다. 필요한 내용을 복사한 뒤 목록으로 돌아가 주세요.'
                      : `다른 곳에서 이 스킬이 변경되었습니다 (최신 버전 ${latest?.revision}). ${dirty ? '초안과 편집 기준 버전을 유지했습니다. 저장 요청에는 기존 버전을 사용하며 최신 내용을 덮어쓰지 않습니다.' : '최신 내용을 확인해 주세요.'}`}</p>
                    {!removed && <button type="button" className="small-button" disabled={!!pending} onClick={() => {
                      if (!latest || !confirmLeave()) return;
                      clearFeedback();
                      setEditor({ initial: latest, draft: skillDraft(latest) });
                      nameInput.current?.focus();
                    }}>최신 내용 다시 불러오기</button>}
                  </div>
                )}
                <fieldset disabled={!!pending}>
                  <legend className="skills-sr-only">스킬 내용</legend>
                  <label className="skills-field" htmlFor={`${prefix}-name`}>
                    <span>이름 <small>필수</small></span>
                    <input id={`${prefix}-name`} aria-label="스킬 이름" ref={nameInput} value={editor.draft.name} autoComplete="off" required
                      aria-invalid={!!validation.name} aria-describedby={validation.name ? `${prefix}-errors` : undefined}
                      onChange={(event) => updateDraft('name', event.target.value)} />
                  </label>
                  <label className="skills-field" htmlFor={`${prefix}-description`}>
                    <span>설명 <small>필수</small></span>
                    <textarea id={`${prefix}-description`} aria-label="스킬 설명" ref={descriptionInput} rows={2} value={editor.draft.description} required
                      aria-invalid={!!validation.description} aria-describedby={`${prefix}-description-help${validation.description ? ` ${prefix}-errors` : ''}`}
                      onChange={(event) => updateDraft('description', event.target.value)} />
                    <small id={`${prefix}-description-help`}>어떤 작업에 언제 사용하는 스킬인지 설명해 주세요.</small>
                  </label>
                  <label className="skills-field" htmlFor={`${prefix}-instructions`}>
                    <span>지침 (Markdown) <small>필수</small></span>
                    <textarea id={`${prefix}-instructions`} aria-label="스킬 지침 (Markdown)" className="skills-instructions" ref={instructionsInput} rows={10}
                      value={editor.draft.instructions} maxLength={instructionLimit} required spellCheck={false}
                      aria-invalid={!!validation.instructions} aria-describedby={`${prefix}-instructions-help${validation.instructions ? ` ${prefix}-errors` : ''}`}
                      onChange={(event) => updateDraft('instructions', event.target.value)}
                      onPaste={(event) => {
                        const input = event.currentTarget;
                        const length = input.value.length - (input.selectionEnd - input.selectionStart) + event.clipboardData.getData('text').length;
                        if (length <= instructionLimit) return;
                        event.preventDefault();
                        setError('지침은 최대 30,000자까지 입력할 수 있습니다. 텍스트를 자르지 않도록 붙여넣기를 취소했습니다. 내용을 줄여 다시 붙여넣어 주세요.');
                      }} />
                    <small id={`${prefix}-instructions-help`} className="skills-instructions-help"><span>이름·설명·지침 전체 JSON은 18 KiB까지 저장합니다. 비밀번호나 API 키를 넣지 마세요.</span><span>{(skillBytes(editor.draft) / 1024).toFixed(1)} / 18 KiB</span></small>
                  </label>
                </fieldset>
                {errors.length > 0 && <div id={`${prefix}-errors`} ref={errorRef}><ErrorNotice><ul className="skills-errors">{errors.map((message, index) => <li key={index}>{message}</li>)}</ul></ErrorNotice></div>}
                <footer className="skills-editor-actions">
                  {editor.initial && <button type="button" className="small-button skills-delete" onClick={() => void remove()} disabled={blocked || removed}><Trash2 size={14} aria-hidden="true" />{pending?.kind === 'delete' ? '삭제 중…' : '스킬 삭제'}</button>}
                  <span className="skills-draft-status" role="status">{dirty ? '저장하지 않은 변경 사항' : '변경 사항 없음'}</span>
                  <button type="button" className="small-button" onClick={back} disabled={!!pending}>뒤로</button>
                  <button type="submit" className="primary-button" disabled={blocked || removed || (!!editor.initial && !dirty)}>{pending?.kind === 'save' ? '저장 중…' : '저장'}</button>
                </footer>
                {busy && <p className="skills-note" role="status">다른 작업이 끝나면 스킬을 저장하거나 삭제할 수 있습니다. 작성 중인 내용은 유지됩니다.</p>}
              </form>
            ) : tab === 'skills' ? (
              <section className="skills-management" aria-labelledby={`${prefix}-management`}>
                <div className="skills-page-heading"><h3 id={`${prefix}-management`}>플러그인 및 스킬 관리</h3><p>이 봇의 비공개 스킬을 확인하고 사용할 스킬을 선택하세요.</p></div>
                {error && <div ref={errorRef}><ErrorNotice>{error}</ErrorNotice></div>}
                {status && <p className="skills-feedback" role="status">{status}</p>}
                <div className="skills-section-heading"><h4><LockKeyhole size={14} aria-hidden="true" />비공개 스킬 <span>{skills.length}</span></h4><button type="button" className="small-button" ref={addButton} onClick={() => edit()} disabled={!!pending}><Plus size={14} aria-hidden="true" />스킬 추가</button></div>
                <div className="skills-import">
                  <button type="button" className="small-button" disabled={blocked} onClick={() => importInput.current?.click()}>{pending?.kind === 'import' ? '가져오는 중…' : 'SKILL.md 가져오기'}</button>
                  <input type="file" ref={importInput} accept=".md,text/markdown,text/plain" aria-label="SKILL.md 파일 선택" onChange={event => { const file = event.currentTarget.files?.[0]; event.currentTarget.value = ''; if (file) void importMarkdown(file); }} />
                  <p>UTF-8 · 최대 20 KiB. YAML의 name·description과 Markdown 지침을 저장합니다. 도구·스크립트·훅 설정은 지원하지 않습니다.</p>
                </div>
                {skills.length ? <>
                  <ul className="skills-list" aria-label="이 봇의 비공개 스킬">
                    {skills.map((skill) => (
                      <li key={skill.id} className="skills-row">
                        <button type="button" className="skills-row-open" data-skill-id={skill.id} onClick={() => edit(skill)} disabled={!!pending} aria-label={`${skill.name} 스킬 편집`}>
                          <span className="skills-row-icon"><FileText size={18} aria-hidden="true" /></span>
                          <span className="skills-row-copy"><strong>{skill.name}</strong><span>{skill.description}</span></span>
                          <ChevronRight size={15} aria-hidden="true" />
                        </button>
                        <div className="skills-row-state"><span aria-hidden="true">{pending?.kind === 'toggle' && pending.id === skill.id ? '변경 중…' : skill.enabled ? '사용 중' : '사용 안 함'}</span><button type="button" role="switch" className="skills-switch" aria-checked={skill.enabled} aria-label={`${bot.name}에서 ${skill.name} 스킬 사용`} disabled={blocked}
                          onClick={() => void mutate({ kind: 'toggle', id: skill.id }, `${root}/${encodeURIComponent(skill.id)}`, 'PATCH', { revision: skill.revision, enabled: !skill.enabled }, `“${skill.name}” 스킬을 이 봇에서 ${skill.enabled ? '사용하지 않습니다' : '사용합니다'}.`)}><span aria-hidden="true" /></button></div>
                      </li>
                    ))}
                  </ul>
                  <button type="button" className="text-button skills-ask" disabled={blocked} onClick={() => leave(onCreateWithBot)}><MessageSquare size={14} aria-hidden="true" />봇에게 스킬 만들기 요청</button>
                </> : <div className="skills-empty"><span className="skills-empty-icon"><FileText size={26} aria-hidden="true" /></span><h4>아직 비공개 스킬이 없습니다</h4><p>봇에게 반복해서 사용할 작업을 스킬로 만들어 달라고 요청해 보세요. 대화에서 제안된 이름과 지침을 검토한 뒤 저장할 수 있습니다.</p><button type="button" className="primary-button" disabled={blocked} onClick={() => leave(onCreateWithBot)}><MessageSquare size={14} aria-hidden="true" />봇에게 만들기 요청</button><p className="skills-note">‘스킬 추가’로 직접 작성하거나 ‘SKILL.md 가져오기’로 파일을 저장할 수 있습니다. 직접 추가는 이 앱의 로컬 확장입니다.</p></div>}
                <div className="skills-connector-notice"><Plug size={15} aria-hidden="true" /><p><strong>외부 커넥터</strong>외부 커넥터 인증·연결은 아직 구현되지 않았습니다. 이 화면은 봇별 로컬 비공개 스킬만 관리합니다.</p></div>
                {busy && <p className="skills-note" role="status">다른 작업이 끝나면 스킬 사용 설정을 변경하거나 봇에게 만들기를 요청할 수 있습니다.</p>}
              </section>
            ) : (
              <section className="skills-connectors" aria-labelledby={`${prefix}-connectors`}><div className="skills-page-heading"><h3 id={`${prefix}-connectors`}>커넥터</h3><p>외부 앱 연결 기능의 지원 범위입니다.</p></div><div className="skills-empty"><span className="skills-empty-icon"><Plug size={26} aria-hidden="true" /></span><h4>외부 커넥터 연결은 아직 지원하지 않습니다</h4><p>계정 인증, 외부 앱 설치, 커넥터 실행은 구현되지 않았습니다. 외부 앱 목록이나 연결된 계정을 표시하지 않습니다.</p><button type="button" className="small-button" onClick={() => changeTab('skills')}>비공개 스킬 관리로 돌아가기</button></div></section>
            )}
          </div>
        </div>
      </div>
    </Modal>
  );
}
