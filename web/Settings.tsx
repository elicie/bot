import { useReducer } from 'react';
import {
  Check,
  CheckCircle2,
  CircleHelp,
  FolderOpen,
  KeyRound,
  Monitor,
  Plug,
  Route,
  Settings2,
  ShieldCheck,
  X,
} from 'lucide-react';
import type { AppState, ProviderCheck, SettingsInput } from '../shared/types';
import { errorText, request } from './api';
import type { SettingsTab } from './state';
import { ErrorNotice, IconButton, Modal, Spinner } from './ui';

interface FormState {
  baseUrl: string;
  model: string;
  workspace: string;
  apiKey: string;
  clearApiKey: boolean;
  baseline: string;
  saved: boolean;
  check:
    | { status: 'idle' }
    | { status: 'checking' }
    | { status: 'done'; result: ProviderCheck }
    | { status: 'error'; error: string };
}
type FormAction =
  | { type: 'field'; field: 'baseUrl' | 'model' | 'workspace' | 'apiKey'; value: string }
  | { type: 'clear'; value: boolean }
  | { type: 'saved' }
  | { type: 'check'; check: FormState['check'] };
const signature = (form: Pick<FormState, 'baseUrl' | 'model' | 'workspace'>) =>
  JSON.stringify([form.baseUrl, form.model, form.workspace]);
function formReducer(state: FormState, action: FormAction): FormState {
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
      return { ...state, apiKey: '', clearApiKey: false, baseline: signature(state), saved: true };
    case 'check':
      return { ...state, check: action.check };
  }
}
const tabs = [
  { id: 'general', label: 'General', description: '일반', icon: Settings2 },
  { id: 'models', label: 'Models', description: '모델 설정', icon: Route },
  { id: 'files', label: 'Local files', description: '로컬 파일 설정', icon: FolderOpen },
] as const;

export function Settings({
  data,
  tab,
  busy,
  error,
  onTab,
  onClose,
  onSave,
  onFiles,
}: {
  data: AppState;
  tab: SettingsTab;
  busy: boolean;
  error: string | null;
  onTab: (tab: SettingsTab) => void;
  onClose: () => void;
  onSave: (input: SettingsInput) => Promise<boolean>;
  onFiles: () => void;
}) {
  const [form, dispatch] = useReducer(formReducer, data.settings, (settings): FormState => ({
    baseUrl: settings.baseUrl,
    model: settings.model,
    workspace: settings.workspace,
    apiKey: '',
    clearApiKey: false,
    baseline: signature(settings),
    saved: false,
    check: { status: 'idle' },
  }));
  const dirty = signature(form) !== form.baseline || !!form.apiKey || form.clearApiKey;
  const close = () => {
    if (busy || form.check.status === 'checking') return;
    if (dirty && !window.confirm('저장하지 않은 설정을 버리고 닫을까요?')) return;
    onClose();
  };
  const testProvider = async () => {
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
          <span>Local workspace</span>
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
            if (busy || form.check.status === 'checking') return;
            const saved = await onSave({
              baseUrl: form.baseUrl.trim(),
              model: form.model.trim(),
              workspace: form.workspace.trim(),
              ...(form.apiKey ? { apiKey: form.apiKey } : {}),
              ...(form.clearApiKey ? { clearApiKey: true } : {}),
            });
            if (saved) dispatch({ type: 'saved' });
          }}
        >
          <div className="settings-scroll">
            {tab === 'general' && (
              <>
                <section className="settings-section">
                  <h3>실행 환경</h3>
                  <div className="settings-card">
                    <div className="settings-row">
                      <div className="settings-copy">
                        <strong>Grok Build CLI</strong>
                        <span>로컬 서버에서 에이전트 작업을 실행합니다.</span>
                      </div>
                      <span
                        className={`status-pill ${data.runtime.available ? 'success' : 'danger'}`}
                      >
                        {data.runtime.available ? '사용 가능' : '사용 불가'}
                      </span>
                    </div>
                    {data.runtime.version && (
                      <div className="settings-row">
                        <span>버전</span>
                        <code>{data.runtime.version}</code>
                      </div>
                    )}
                    {!data.runtime.available && (
                      <div className="settings-row">
                        <div className="settings-copy">
                          <span className="danger-text">
                            {data.runtime.error || 'Grok Build CLI를 찾을 수 없습니다.'}
                          </span>
                          <span>
                            서버 컴퓨터에 Grok Build를 설치한 뒤 서버를 다시 시작해 주세요.
                          </span>
                        </div>
                      </div>
                    )}
                  </div>
                </section>
                <section className="settings-section">
                  <h3>이 컴퓨터에서 실행</h3>
                  <div className="settings-card">
                    <div className="settings-row">
                      <div className="settings-copy">
                        <strong>
                          <Monitor size={15} /> 서버 컴퓨터
                        </strong>
                        <span>브라우저 주소와 연결된 서버에서 모델과 파일 작업을 실행합니다.</span>
                      </div>
                    </div>
                    <div className="settings-row">
                      <span>연결 주소</span>
                      <code>{window.location.host}</code>
                    </div>
                    <div className="settings-row">
                      <div className="settings-copy">
                        <span>
                          다른 기기의 브라우저로 접속해도 파일은 서버 컴퓨터에서 열립니다. 화면
                          제어나 가상 데스크톱은 제공하지 않습니다.
                        </span>
                      </div>
                    </div>
                  </div>
                </section>
                <section className="settings-section">
                  <h3>대화</h3>
                  <div className="settings-card">
                    <div className="settings-row">
                      <div className="settings-copy">
                        <strong>모델</strong>
                        <span>{data.settings.model || '아직 설정하지 않았습니다.'}</span>
                      </div>
                      <button
                        className="small-button"
                        type="button"
                        onClick={() => onTab('models')}
                      >
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
                </section>
              </>
            )}
            {tab === 'models' && (
              <>
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
                        disabled={busy || form.check.status === 'checking'}
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
                        disabled={busy || form.check.status === 'checking'}
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
                        disabled={busy || form.check.status === 'checking' || form.clearApiKey}
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
                          disabled={busy || form.check.status === 'checking'}
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
                      disabled={!form.baseUrl.trim() || form.check.status === 'checking' || busy}
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
                        disabled={busy || form.check.status === 'checking'}
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
              {form.saved ? (
                <>
                  <CheckCircle2 size={14} />
                  설정을 저장했습니다
                </>
              ) : dirty ? (
                '저장하지 않은 변경 사항'
              ) : (
                '설정은 서버에 저장됩니다'
              )}
            </span>
            <button
              className="primary-button"
              type="submit"
              disabled={busy || form.check.status === 'checking' || !dirty}
            >
              {busy ? '저장 중…' : '설정 저장'}
            </button>
          </footer>
        </form>
      </div>
    </Modal>
  );
}

export function Capabilities({ onClose }: { onClose: () => void }) {
  return (
    <Modal title="Plugins" onClose={onClose} className="compact-modal">
      <header className="modal-header">
        <h2>
          <Plug size={18} />
          Plugins
        </h2>
        <IconButton label="Plugins 닫기" onClick={onClose}>
          <X size={18} />
        </IconButton>
      </header>
      <div className="modal-body capabilities">
        <p className="muted">이 로컬 앱에서 사용할 수 있는 기능입니다.</p>
        <section>
          <FolderOpen size={20} />
          <div>
            <h3>로컬 파일</h3>
            <p>
              작업 폴더의 텍스트 파일을 탐색하고 편집합니다. 저장은 실제 서버 파일에 반영됩니다.
            </p>
          </div>
        </section>
        <section>
          <ShieldCheck size={20} />
          <div>
            <h3>Grok Build 도구</h3>
            <p>
              에이전트가 요청한 파일 작업은 대화에 표시됩니다. 승인 요청에서 경로와 내용을 확인할 수
              있습니다.
            </p>
          </div>
        </section>
        <section>
          <CircleHelp size={20} />
          <div>
            <h3>지원 범위</h3>
            <p>
              외부 플러그인 설치, 클라우드 컴퓨터, 데스크톱 제어는 이 앱에 포함되어 있지 않습니다.
            </p>
          </div>
        </section>
      </div>
    </Modal>
  );
}
