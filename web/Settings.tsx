import { useEffect, useReducer, useState } from 'react';
import {
  Check,
  CheckCircle2,
  Clipboard,
  CreditCard,
  FolderOpen,
  KeyRound,
  Monitor,
  RefreshCw,
  Route,
  Settings2,
  ShieldCheck,
  Vault,
  X,
} from 'lucide-react';
import type { AppState, ProviderCheck, SettingsInput } from '../shared/types';
import { errorText, request } from './api';
import { runningConversation, type SettingsTab } from './state';
import { ErrorNotice, IconButton, Modal, Spinner } from './ui';
import { PermissionRules } from './PermissionRules';
import { NotificationSettings } from './ActivitySettings';
import { VaultSettings } from './Vault';
import './settings.css';
import './concurrency.css';

interface FormState {
  maxConcurrentRuns: string;
  baseUrl: string;
  model: string;
  workspace: string;
  apiKey: string;
  clearApiKey: boolean;
  baseline: SettingsFields;
  saved: boolean;
  check:
    | { status: 'idle' }
    | { status: 'checking' }
    | { status: 'done'; result: ProviderCheck }
    | { status: 'error'; error: string };
}
type SettingsFields = Pick<FormState, 'baseUrl' | 'model' | 'workspace' | 'maxConcurrentRuns'>;
type FormAction =
  | { type: 'field'; field: keyof SettingsFields | 'apiKey'; value: string }
  | { type: 'clear'; value: boolean }
  | { type: 'saved' }
  | { type: 'reconcile'; settings: AppState['settings'] }
  | { type: 'check'; check: FormState['check'] };
const fields = (form: SettingsFields): SettingsFields => ({
  baseUrl: form.baseUrl, model: form.model, workspace: form.workspace, maxConcurrentRuns: form.maxConcurrentRuns,
});
const settingsFields = (settings: AppState['settings']): SettingsFields =>
  fields({ ...settings, maxConcurrentRuns: String(settings.maxConcurrentRuns ?? 3) });
const signature = (form: SettingsFields) => JSON.stringify(fields(form));
export function concurrencyLimitError(value: string): string | null {
  const limit = Number(value);
  return value.trim() && Number.isInteger(limit) && limit >= 1 && limit <= 16
    ? null : '1에서 16 사이의 정수를 입력해 주세요.';
}
export function initialSettingsForm(settings: AppState['settings']): FormState {
  const baseline = settingsFields(settings);
  return {
    ...baseline, apiKey: '', clearApiKey: false, baseline, saved: false, check: { status: 'idle' },
  };
}
export function settingsFormDirty(form: FormState): boolean {
  return signature(form) !== signature(form.baseline) || !!form.apiKey || form.clearApiKey;
}
export function settingsFormInput(form: FormState): SettingsInput | null {
  if (concurrencyLimitError(form.maxConcurrentRuns)) return null;
  return {
    maxConcurrentRuns: Number(form.maxConcurrentRuns),
    baseUrl: form.baseUrl.trim(), model: form.model.trim(), workspace: form.workspace.trim(),
    ...(form.apiKey ? { apiKey: form.apiKey } : {}),
    ...(form.clearApiKey ? { clearApiKey: true } : {}),
  };
}
export function formReducer(state: FormState, action: FormAction): FormState {
  switch (action.type) {
    case 'field':
      return {
        ...state,
        [action.field]: action.value,
        saved: false,
        check: { status: 'idle' },
        ...(action.field === 'apiKey' ? { clearApiKey: false } : {}),
      };
    case 'clear':
      return {
        ...state,
        clearApiKey: action.value,
        apiKey: '',
        saved: false,
        check: { status: 'idle' },
      };
    case 'saved':
      return { ...state, apiKey: '', clearApiKey: false, baseline: fields(state), saved: true };
    case 'reconcile': {
      const baseline = settingsFields(action.settings);
      if (signature(baseline) === signature(state.baseline) || state.check.status === 'checking') return state;
      const reconciled = fields(state);
      for (const field of Object.keys(baseline) as (keyof SettingsFields)[]) {
        if (state[field] === state.baseline[field]) reconciled[field] = baseline[field];
      }
      return { ...state, ...reconciled, baseline, saved: false, check: { status: 'idle' } };
    }
    case 'check':
      return { ...state, check: action.check };
  }
}
interface BrowserPreferences {
  theme: 'system' | 'dark' | 'light';
  spellcheck: boolean;
}
const preferencesKey = 'grok-bot.browser-preferences';
const defaultPreferences: BrowserPreferences = { theme: 'dark', spellcheck: true };
const buildCommand = 'npm run build:release';

function readBrowserPreferences(): BrowserPreferences {
  try {
    const value: unknown = JSON.parse(localStorage.getItem(preferencesKey) ?? 'null');
    if (!value || typeof value !== 'object') return defaultPreferences;
    return {
      theme:
        'theme' in value &&
        (value.theme === 'system' || value.theme === 'dark' || value.theme === 'light')
          ? value.theme
          : defaultPreferences.theme,
      spellcheck:
        'spellcheck' in value && typeof value.spellcheck === 'boolean'
          ? value.spellcheck
          : defaultPreferences.spellcheck,
    };
  } catch {
    return defaultPreferences;
  }
}
function applyBrowserPreferences(preferences: BrowserPreferences) {
  document.documentElement.dataset.theme = preferences.theme;
  document.documentElement.spellcheck = preferences.spellcheck;
}
if (typeof window !== 'undefined') applyBrowserPreferences(readBrowserPreferences());

const tabs = [
  { id: 'general', label: '일반', description: '일반', icon: Settings2 },
  { id: 'files', label: '컴퓨터', description: '로컬 파일 설정', icon: Monitor },
  { id: 'permissions', label: '권한', description: '권한', icon: ShieldCheck },
  { id: 'vault', label: '보관함', description: '보관함', icon: Vault },
  { id: 'usage', label: '사용량 및 청구', description: '사용량 및 청구', icon: CreditCard },
  { id: 'updates', label: '업데이트', description: '업데이트', icon: RefreshCw },
  { id: 'models', label: '모델', description: '모델 설정', icon: Route },
] as const;

function RuntimeDetails({ runtime }: { runtime: AppState['runtime'] }) {
  return (
    <div className="settings-card">
      <div className="settings-row">
        <div className="settings-copy">
          <strong>Grok Build 기반 엔진</strong>
          <span>Grok Build 소스를 직접 링크한 앱 내장 엔진으로 로컬 작업을 실행합니다.</span>
        </div>
        <span className={`status-pill ${runtime.available ? 'success' : 'danger'}`}>
          {runtime.available ? '사용 가능' : '사용 불가'}
        </span>
      </div>
      <div className="settings-row">
        <span>버전·소스 리비전</span>
        <code>{runtime.version || '엔진 버전을 확인할 수 없습니다.'}</code>
      </div>
      {!runtime.available && (
        <div className="settings-row">
          <div className="settings-copy">
            <span className="danger-text">{runtime.error || '앱 엔진을 사용할 수 없습니다.'}</span>
            <span>
              서버 로그를 확인한 뒤 grok-bot을 다시 시작해 주세요.
            </span>
          </div>
        </div>
      )}
    </div>
  );
}

export function Settings({
  data,
  tab,
  busy,
  error,
  onTab,
  onClose,
  onSave,
  onFiles,
  linkPreviewsEnabled,
  linkPreviewsSaving,
  onLinkPreviewsChange,
}: {
  data: AppState;
  tab: SettingsTab;
  busy: boolean;
  error: string | null;
  onTab: (tab: SettingsTab) => void;
  onClose: () => void;
  onSave: (input: SettingsInput) => Promise<boolean>;
  onFiles: () => void;
  linkPreviewsEnabled: boolean;
  linkPreviewsSaving: boolean;
  onLinkPreviewsChange: (enabled: boolean) => Promise<void>;
}) {
  const [form, dispatch] = useReducer(formReducer, data.settings, initialSettingsForm);
  useEffect(() => {
    dispatch({ type: 'reconcile', settings: data.settings });
  }, [data.settings, form.check.status]);
  const [preferences, setPreferences] = useState(readBrowserPreferences);
  const [preferenceError, setPreferenceError] = useState<string | null>(null);
  const [copyStatus, setCopyStatus] = useState<'idle' | 'copying' | 'copied' | 'error'>('idle');
  const dirty = settingsFormDirty(form);
  const concurrencyError = concurrencyLimitError(form.maxConcurrentRuns);
  const running = !!runningConversation(data);
  const locked = busy || running;
  const pendingPermissions = data.conversations.filter(
    (conversation) => conversation.run.status === 'awaiting_permission' && conversation.run.permission,
  ).length;
  const messages = data.conversations.flatMap((conversation) => conversation.messages);
  const usage = [
    { label: '봇', value: data.bots.length },
    { label: '대화', value: data.conversations.length },
    { label: '기록된 실행', value: messages.filter((message) => message.role === 'assistant').length },
    { label: '도구 활동', value: messages.reduce((total, message) => total + message.tools.length, 0) },
  ];
  const updatePreferences = (next: BrowserPreferences) => {
    setPreferences(next);
    applyBrowserPreferences(next);
    try {
      localStorage.setItem(preferencesKey, JSON.stringify(next));
      setPreferenceError(null);
    } catch {
      setPreferenceError(
        '브라우저 저장소에 접근할 수 없어 이번 탭에서만 적용됩니다. 브라우저의 저장소 권한을 확인해 주세요.',
      );
    }
  };
  const copyBuildCommand = async (button: HTMLButtonElement) => {
    setCopyStatus('copying');
    try {
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(buildCommand);
      } else {
        const field = document.createElement('textarea');
        field.value = buildCommand;
        field.readOnly = true;
        field.className = 'settings-clipboard-field';
        button.closest('dialog')?.append(field);
        try {
          field.select();
          if (!document.execCommand('copy')) throw new Error('Clipboard unavailable');
        } finally {
          field.remove();
          button.focus();
        }
      }
      setCopyStatus('copied');
    } catch {
      setCopyStatus('error');
    }
  };
  const close = () => {
    if (busy || form.check.status === 'checking') return;
    if (dirty && !window.confirm('저장하지 않은 설정을 버리고 닫을까요?')) return;
    onClose();
  };
  const testProvider = async () => {
    if (locked || form.check.status === 'checking') return;
    dispatch({ type: 'check', check: { status: 'checking' } });
    try {
      const result = await request<ProviderCheck>('/api/provider/test', {
        method: 'POST',
        body: {
          baseUrl: form.baseUrl.trim(),
          ...(form.apiKey || form.clearApiKey ? { apiKey: form.apiKey } : {}),
        },
      });
      dispatch({ type: 'check', check: { status: 'done', result } });
    } catch (error) {
      dispatch({ type: 'check', check: { status: 'error', error: errorText(error) } });
    }
  };
  const checkResult = form.check.status === 'done' ? form.check.result : null;
  return (
    <Modal title="설정" onClose={close} className="settings-modal">
      <nav className="settings-nav" aria-label="설정 메뉴">
        <span className="settings-nav-heading">설정</span>
        {tabs.map((item) => (
          <button
            type="button"
            key={item.id}
            className={`settings-nav-item ${tab === item.id ? 'selected' : ''}`}
            aria-current={tab === item.id ? 'page' : undefined}
            aria-label={item.description}
            onClick={() => onTab(item.id)}
          >
            <item.icon size={16} />
            <span>{item.label}</span>
          </button>
        ))}
        <div className="settings-nav-bottom">
          <ShieldCheck size={15} />
          <span>로컬 작업 공간</span>
        </div>
      </nav>
      <div className="settings-panel">
        <header className="settings-header">
          <h2>{tabs.find((item) => item.id === tab)?.label}</h2>
          <IconButton
            label="설정 닫기"
            onClick={close}
            disabled={busy || form.check.status === 'checking'}
          >
            <X size={18} />
          </IconButton>
        </header>
        <form
          className="settings-form"
          onSubmit={async (event) => {
            event.preventDefault();
            const input = settingsFormInput(form);
            if (locked || form.check.status === 'checking' || !input || !dirty) return;
            const saved = await onSave(input);
            if (saved) dispatch({ type: 'saved' });
          }}
        >
          <div className="settings-scroll">
            {tab === 'general' && (
              <>
                <section className="settings-section">
                  <h3>계정</h3>
                  <div className="settings-card">
                    <div className="settings-row">
                      <div className="settings-copy">
                        <strong>로컬 연결</strong>
                        <span>클라우드 계정에 로그인하지 않은 로컬 앱입니다.</span>
                      </div>
                      <span className="settings-value">{window.location.host}</span>
                    </div>
                  </div>
                </section>
                <section className="settings-section">
                  <h3>모양</h3>
                  <div className="settings-card">
                    <div className="settings-row">
                      <label className="settings-copy" htmlFor="settings-theme">
                        <strong>테마</strong>
                        <span>이 브라우저의 앱 화면에 적용됩니다.</span>
                      </label>
                      <select
                        id="settings-theme"
                        aria-label="테마"
                        tabIndex={0}
                        value={preferences.theme}
                        onChange={(event) => {
                          const theme = event.target.value;
                          if (theme === 'system' || theme === 'dark' || theme === 'light')
                            updatePreferences({ ...preferences, theme });
                        }}
                      >
                        <option value="system">시스템</option>
                        <option value="dark">다크</option>
                        <option value="light">라이트</option>
                      </select>
                    </div>
                    <div className="settings-row">
                      <span>강조 색상</span>
                      <span className="settings-value">
                        <span className="settings-accent-swatch" />
                        파랑 · 기본
                      </span>
                    </div>
                  </div>
                </section>
                <section className="settings-section">
                  <h3>언어 및 입력</h3>
                  <div className="settings-card">
                    <div className="settings-row">
                      <span>앱 언어</span>
                      <span className="settings-value">한국어</span>
                    </div>
                    <div className="settings-row">
                      <label className="settings-copy" htmlFor="settings-spellcheck">
                        <strong>맞춤법 검사</strong>
                        <span>입력란에서 브라우저의 맞춤법 검사를 사용합니다.</span>
                      </label>
                      <span className="settings-switch">
                        <input
                          id="settings-spellcheck"
                          type="checkbox"
                          role="switch"
                          aria-label="맞춤법 검사"
                          checked={preferences.spellcheck}
                          onChange={(event) =>
                            updatePreferences({ ...preferences, spellcheck: event.target.checked })
                          }
                        />
                        <span aria-hidden="true" />
                      </span>
                    </div>
                    <div className="settings-row">
                      <div className="settings-copy">
                        <strong>이 브라우저의 시간대</strong>
                        <span>실행 시간대는 각 루틴에서 설정합니다.</span>
                      </div>
                      <span className="settings-value">
                        {Intl.DateTimeFormat().resolvedOptions().timeZone}
                      </span>
                    </div>
                  </div>
                  <p className="settings-note">
                    테마와 맞춤법 검사는 이 브라우저에 즉시 저장됩니다. 코드 편집기의 맞춤법 검사는
                    사용하지 않습니다.
                  </p>
                  {preferenceError && <ErrorNotice>{preferenceError}</ErrorNotice>}
                </section>
                <NotificationSettings />
                <section className="settings-section">
                  <h3>실행 환경</h3>
                  <RuntimeDetails runtime={data.runtime} />
                </section>
                <section className="settings-section">
                  <h3>대화</h3>
                  <div className="settings-card">
                    <div className="settings-row">
                      <label className="settings-copy" htmlFor="settings-link-previews">
                        <strong>링크 미리보기</strong>
                        <span id="settings-link-previews-description">기본값은 꺼짐입니다. 켜면 완료된 봇 답변마다 링크를 최대 3개 미리 보여줍니다.</span>
                        {typeof data.linkPreviewsEnabled !== 'boolean' && <span>이 서버 버전에서는 지원하지 않습니다. 서버를 업데이트해 주세요.</span>}
                        {linkPreviewsSaving && <span role="status">서버에 저장 중…</span>}
                      </label>
                      <span className="settings-switch">
                        <input id="settings-link-previews" type="checkbox" role="switch" aria-label="링크 미리보기"
                          aria-describedby="settings-link-previews-description settings-link-previews-privacy"
                          checked={linkPreviewsEnabled} disabled={busy || typeof data.linkPreviewsEnabled !== 'boolean'}
                          onChange={event => void onLinkPreviewsChange(event.target.checked)} />
                        <span aria-hidden="true" />
                      </span>
                    </div>
                    <div className="settings-row">
                      <div className="settings-copy">
                        <strong>모델</strong>
                        <span>{data.settings.model || '아직 설정하지 않았습니다.'}</span>
                      </div>
                      <button className="small-button" type="button" onClick={() => onTab('models')}>
                        모델 설정
                      </button>
                    </div>
                    <div className="settings-row">
                      <div className="settings-copy">
                        <strong>작업 폴더</strong>
                        <span className="path-text">
                          {data.settings.workspace || '연결된 폴더가 없습니다.'}
                        </span>
                      </div>
                      <button className="small-button" type="button" onClick={() => onTab('files')}>
                        폴더 설정
                      </button>
                    </div>
                  </div>
                  <p className="settings-note" id="settings-link-previews-privacy">
                    링크 미리보기를 켜면 서버가 외부 사이트에 접속해 페이지 정보와 이미지를 가져옵니다. 대상 사이트에는 서버의 IP 주소가 보입니다. 브라우저는 미리보기를 위해 외부 사이트에 직접 요청하지 않습니다. 이 설정은 서버에 즉시 저장됩니다.
                  </p>
                </section>
              </>
            )}
            {tab === 'permissions' && (
              <>
                <PermissionRules data={data} disabled={locked} />
                <section className="settings-section">
                  <h3>승인 대기</h3>
                  <div className="settings-card">
                    <div className="settings-row">
                      <div className="settings-copy">
                        <strong>
                          {pendingPermissions
                            ? `${pendingPermissions}개의 요청이 대기 중입니다.`
                            : '대기 중인 요청이 없습니다.'}
                        </strong>
                        <span>승인과 거절은 해당 대화의 요청에서 선택합니다. 거절하거나 실행을 중단하면 파일을 쓰지 않습니다.</span>
                      </div>
                    </div>
                  </div>
                </section>
              </>
            )}
            {tab === 'vault' && (
              <>
                <section className="settings-section">
                  <h3>모델 제공자 인증 정보</h3>
                  <div className="settings-card">
                    <div className="settings-row">
                      <div className="settings-copy">
                        <strong><KeyRound size={15} /> API 키</strong>
                        <span>{data.settings.hasApiKey ? '서버에 저장되어 있습니다.' : '저장된 키가 없습니다.'}</span>
                      </div>
                      {data.settings.hasApiKey && (
                        <span className="settings-masked-key" aria-label="저장된 API 키 마스킹됨">
                          ••••••••
                        </span>
                      )}
                    </div>
                    <div className="settings-row">
                      <div className="settings-copy">
                        <strong>키 관리</strong>
                        <span>모델 설정에서 키를 교체하거나 삭제할 수 있습니다. 저장된 키 원문은 브라우저로 다시 불러오지 않습니다.</span>
                      </div>
                      <button className="small-button" type="button" onClick={() => onTab('models')}>
                        모델 설정 열기
                      </button>
                    </div>
                  </div>
                </section>
                <VaultSettings data={data} busy={busy} />
              </>
            )}
            {tab === 'usage' && (
              <>
                <section className="settings-section">
                  <h3>이 서버에 저장된 활동</h3>
                  <dl className="settings-card settings-usage">
                    {usage.map((item) => (
                      <div className="settings-row" key={item.label}>
                        <dt>{item.label}</dt>
                        <dd>{item.value.toLocaleString('ko-KR')}</dd>
                      </div>
                    ))}
                  </dl>
                  <p className="settings-note">
                    실행 수는 실행을 시작할 때 생성된 응답 기록을 기준으로 계산합니다. 도구 활동은
                    대화에 남은 활동 기록 수이며, 삭제한 대화는 집계에서 제외됩니다.
                  </p>
                </section>
                <section className="settings-section">
                  <h3>사용량 및 청구</h3>
                  <div className="settings-card">
                    <div className="settings-row">
                      <div className="settings-copy">
                        <strong>모델 제공자에서 확인</strong>
                        <span>
                          이 앱은 토큰 사용량, 결제 내역, 구독 정보를 조회하지 않습니다. 요금이
                          발생하는 모델을 연결했다면 해당 제공자의 관리 페이지에서 확인해 주세요.
                        </span>
                      </div>
                    </div>
                  </div>
                  <p className="settings-note">위 활동 수는 비용이나 남은 크레딧을 의미하지 않습니다.</p>
                </section>
              </>
            )}
            {tab === 'updates' && (
              <>
                <section className="settings-section">
                  <h3>현재 실행 중인 엔진</h3>
                  <RuntimeDetails runtime={data.runtime} />
                </section>
                <section className="settings-section">
                  <h3>소스에서 업데이트</h3>
                  <p className="settings-note">
                    이 앱은 업데이트를 자동으로 다운로드하거나 설치하지 않습니다. 서버 컴퓨터에서
                    프로젝트 소스를 갱신한 뒤 앱 폴더에서 다음 명령을 실행해 주세요.
                  </p>
                  <div className="settings-command">
                    <code>{buildCommand}</code>
                    <button
                      className="small-button"
                      type="button"
                      aria-label="빌드 명령 복사"
                      disabled={copyStatus === 'copying'}
                      onClick={(event) => void copyBuildCommand(event.currentTarget)}
                    >
                      {copyStatus === 'copied' ? <Check size={14} /> : <Clipboard size={14} />}
                      {copyStatus === 'copied' ? '복사됨' : '복사'}
                    </button>
                  </div>
                  <span className="settings-copy-status" role="status">
                    {copyStatus === 'copied' ? '빌드 명령을 클립보드에 복사했습니다.' : ''}
                  </span>
                  {copyStatus === 'error' && (
                    <ErrorNotice>
                      클립보드에 접근할 수 없습니다. 위 명령을 직접 선택해 복사해 주세요.
                    </ErrorNotice>
                  )}
                  <ol className="settings-instructions">
                    <li>Node.js 22.12 이상, Rust stable, 네이티브 컴파일러와 ripgrep이 필요합니다.</li>
                    <li>빌드 전에 <code>npm ci</code>로 의존성을 설치합니다.</li>
                    <li>빌드가 끝나면 실행 중인 서버를 중지하고 <code>./target/release/grok-bot</code>으로 다시 시작합니다.</li>
                    <li>완성된 바이너리에는 엔진과 웹 화면이 포함되어 Node.js 없이 실행할 수 있습니다.</li>
                    <li>설정을 다시 열어 엔진의 버전과 사용 가능 상태를 확인합니다.</li>
                  </ol>
                </section>
              </>
            )}
            {tab === 'models' && (
              <>
                <section className="settings-section">
                  <h3>동시 실행</h3>
                  <div className="settings-card field-card">
                    <label className="settings-field" htmlFor="settings-concurrency">
                      <span>동시 실행 Bot 수</span>
                      <input id="settings-concurrency" className="settings-concurrency-input" type="number" min={1} max={16} step={1} required
                        aria-label="동시 실행 Bot 수" aria-invalid={!!concurrencyError}
                        aria-describedby={`settings-concurrency-help${concurrencyError ? ' settings-concurrency-error' : ''}`}
                        value={form.maxConcurrentRuns} disabled={locked || form.check.status === 'checking'}
                        onChange={(event) => dispatch({ type: 'field', field: 'maxConcurrentRuns', value: event.target.value })} />
                      <small id="settings-concurrency-help">1~16 · 기본값 3. 서로 다른 Bot은 설정한 수만큼 동시에 실행합니다. 같은 Bot의 작업은 요청 순서대로 하나씩 실행합니다.</small>
                      {concurrencyError && <small id="settings-concurrency-error" className="settings-concurrency-error" role="alert">{concurrencyError}</small>}
                    </label>
                  </div>
                </section>
                <section className="settings-section">
                  <h3>OpenAI-compatible provider</h3>
                  <div className="settings-card field-card">
                    <label className="settings-field">
                      <span>Base URL</span>
                      <input
                        type="url"
                        aria-label="Base URL"
                        placeholder="http://127.0.0.1:8000/v1"
                        autoComplete="off"
                        value={form.baseUrl}
                        onChange={(event) =>
                          dispatch({ type: 'field', field: 'baseUrl', value: event.target.value })
                        }
                        disabled={locked || form.check.status === 'checking'}
                      />
                      <small>OpenAI 호환 API의 기본 주소를 입력하세요.</small>
                    </label>
                    <label className="settings-field">
                      <span>Model ID</span>
                      <input
                        type="text"
                        aria-label="Model ID"
                        placeholder="모델 ID"
                        autoComplete="off"
                        value={form.model}
                        onChange={(event) =>
                          dispatch({ type: 'field', field: 'model', value: event.target.value })
                        }
                        disabled={locked || form.check.status === 'checking'}
                      />
                      <small>서버가 제공하는 모델 ID를 그대로 입력하세요.</small>
                    </label>
                  </div>
                </section>
                <section className="settings-section">
                  <h3>인증</h3>
                  <div className="settings-card field-card">
                    <label className="settings-field">
                      <span className="field-heading">
                        <span>
                          API key <span className="muted">선택</span>
                        </span>
                        <span className={`status-pill ${data.settings.hasApiKey ? 'success' : ''}`}>
                          <KeyRound size={12} />
                          {data.settings.hasApiKey ? '키 저장됨' : '저장된 키 없음'}
                        </span>
                      </span>
                      <input
                        type="password"
                        aria-label="API key"
                        autoComplete="new-password"
                        placeholder={
                          data.settings.hasApiKey
                            ? '비워 두면 저장된 키를 유지합니다'
                            : 'API 키 입력'
                        }
                        value={form.apiKey}
                        onChange={(event) =>
                          dispatch({ type: 'field', field: 'apiKey', value: event.target.value })
                        }
                        disabled={locked || form.check.status === 'checking' || form.clearApiKey}
                      />
                      <small>키는 서버에만 저장됩니다. 브라우저로 다시 불러오지 않습니다.</small>
                    </label>
                    {data.settings.hasApiKey && (
                      <label className="checkbox-label">
                        <input
                          type="checkbox"
                          checked={form.clearApiKey}
                          onChange={(event) =>
                            dispatch({ type: 'clear', value: event.target.checked })
                          }
                          disabled={locked || form.check.status === 'checking'}
                        />
                        저장된 API 키 삭제
                      </label>
                    )}
                  </div>
                </section>
                <section className="settings-section">
                  <div className="section-action">
                    <span className="muted">입력한 주소로 모델 목록을 확인합니다.</span>
                    <button
                      type="button"
                      className="small-button"
                      onClick={testProvider}
                      disabled={!form.baseUrl.trim() || form.check.status === 'checking' || locked}
                    >
                      {form.check.status === 'checking' ? (
                        <Spinner label="테스트 중" />
                      ) : (
                        '연결 테스트'
                      )}
                    </button>
                  </div>
                  {form.check.status === 'error' && <ErrorNotice>{form.check.error}</ErrorNotice>}
                  {checkResult &&
                    (checkResult.ok ? (
                      <div className="provider-result" role="status">
                        <CheckCircle2 size={16} />
                        <span>
                          연결 성공.{' '}
                          {checkResult.models.length
                            ? `${checkResult.models.length}개 모델을 확인했습니다.`
                            : '모델 목록이 비어 있습니다.'}
                        </span>
                      </div>
                    ) : (
                      <ErrorNotice>
                        {checkResult.error || '모델 서버에 연결하지 못했습니다.'}
                      </ErrorNotice>
                    ))}
                  {checkResult?.ok && checkResult.models.length > 0 && (
                    <div className="model-list" aria-label="사용 가능한 모델">
                      {checkResult.models.map((model) => (
                        <button
                          type="button"
                          key={model}
                          aria-pressed={model === form.model}
                          disabled={locked || form.check.status === 'checking'}
                          onClick={() => dispatch({ type: 'field', field: 'model', value: model })}
                        >
                          {model === form.model && <Check size={13} />}
                          {model}
                        </button>
                      ))}
                    </div>
                  )}
                </section>
              </>
            )}
            {tab === 'files' && (
              <>
                <section className="settings-section">
                  <h3>연결된 컴퓨터</h3>
                  <div className="settings-card">
                    <div className="settings-row">
                      <div className="settings-copy">
                        <strong><Monitor size={15} /> 서버 컴퓨터</strong>
                        <span>{window.location.host}</span>
                      </div>
                      <span className="status-pill">로컬 실행</span>
                    </div>
                    <div className="settings-row">
                      <div className="settings-copy">
                        <strong>파일 쓰기는 매번 확인</strong>
                        <span>
                          다른 기기에서 접속해도 파일 작업은 서버에서 실행됩니다. 작업 폴더 설정은
                          브라우저 기기의 파일이나 운영체제 전체 접근 권한을 부여하지 않습니다.
                        </span>
                      </div>
                      <button className="small-button" type="button" onClick={() => onTab('permissions')}>
                        권한 보기
                      </button>
                    </div>
                  </div>
                </section>
                <section className="settings-section">
                  <h3>관리형 브라우저</h3>
                  <p className="settings-note">컴퓨터 패널에서 허용할 공개 사이트를 정하고 Chrome을 직접 시작합니다. 서버의 localhost 연결에서 조작할 수 있으며, Chrome 샌드박스를 사용할 수 없으면 시작이 차단됩니다. 로그인·파일 업로드·다운로드와 개인 데스크톱 제어는 제공하지 않습니다.</p>
                </section>
                <section className="settings-section">
                  <h3>작업 폴더</h3>
                  <div className="settings-card field-card">
                    <label className="settings-field">
                      <span>로컬 작업 폴더 경로</span>
                      <input
                        aria-label="로컬 작업 폴더 경로"
                        placeholder="/home/user/project"
                        value={form.workspace}
                        autoComplete="off"
                        spellCheck={false}
                        onChange={(event) =>
                          dispatch({ type: 'field', field: 'workspace', value: event.target.value })
                        }
                        disabled={locked || form.check.status === 'checking'}
                      />
                      <small>서버 컴퓨터에 있는 폴더의 절대 경로를 입력하세요.</small>
                    </label>
                  </div>
                </section>
                <section className="settings-section">
                  <h3>파일 접근</h3>
                  <div className="settings-card">
                    <div className="settings-row">
                      <div className="settings-copy">
                        <strong>
                          <FolderOpen size={15} /> 명시적으로 저장
                        </strong>
                        <span>
                          텍스트 파일을 열고 수정한 뒤 저장 버튼을 누르면 실제 파일에 반영됩니다.
                          외부 변경과 충돌하면 덮어쓰지 않습니다.
                        </span>
                      </div>
                    </div>
                    <div className="settings-row">
                      <div className="settings-copy">
                        <strong>에이전트의 파일 작업</strong>
                        <span>작업 중 표시되는 경로와 내용을 확인하고 승인 여부를 선택하세요.</span>
                      </div>
                    </div>
                    <div className="settings-row">
                      <div className="settings-copy">
                        <strong>실행 위치</strong>
                        <span>
                          파일은 {window.location.host} 서버의 작업 폴더에 있습니다. 브라우저 기기의
                          파일 선택창이 아닙니다.
                        </span>
                      </div>
                    </div>
                  </div>
                </section>
                {data.settings.workspace && !dirty && (
                  <button
                    type="button"
                    className="small-button"
                    onClick={() => {
                      onClose();
                      onFiles();
                    }}
                  >
                    <FolderOpen size={15} />
                    파일 탐색기 열기
                  </button>
                )}
              </>
            )}
            {error && <ErrorNotice>{error}</ErrorNotice>}
          </div>
          <footer className="settings-footer">
            <span className="muted" role="status">
              {running ? '모든 Bot의 작업이 끝나면 설정을 저장할 수 있습니다' : form.saved ? (
                <>
                  <CheckCircle2 size={14} />
                  설정을 저장했습니다
                </>
              ) : dirty ? (
                '저장하지 않은 변경 사항'
              ) : tab === 'general' ? (
                '모양과 입력은 브라우저에, 링크 미리보기는 서버에 적용됩니다'
              ) : (
                '모델과 작업 폴더 설정은 서버에 저장됩니다'
              )}
            </span>
            <button
              className="primary-button"
              type="submit"
              disabled={locked || form.check.status === 'checking' || !!concurrencyError || !dirty}
            >
              {busy ? '저장 중…' : '설정 저장'}
            </button>
          </footer>
        </form>
      </div>
    </Modal>
  );
}
