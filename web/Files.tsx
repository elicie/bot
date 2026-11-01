import { useEffect, useId, useRef, type Dispatch } from 'react';
import {
  ChevronRight,
  FileText,
  Folder,
  FolderOpen,
  Home,
  Monitor,
  RefreshCw,
  Save,
  X,
} from 'lucide-react';
import type { FileDocument } from '../shared/types';
import { ApiError, errorText, listFiles, readDocument, request } from './api';
import { isDirty, type Action, type FilesState } from './state';
import { ErrorNotice, IconButton, Spinner } from './ui';

export function Files({
  workspace,
  files,
  dispatch,
  onClose,
  onSettings,
  embedded = false,
}: {
  embedded?: boolean;
  workspace: string;
  files: FilesState;
  dispatch: Dispatch<Action>;
  onClose: () => void;
  onSettings: () => void;
}) {
  const requestPrefix = useId();
  const requestSequence = useRef(0);
  const filesRef = useRef(files);
  filesRef.current = files;
  const workspaceRef = useRef(workspace);
  workspaceRef.current = workspace;
  useEffect(() => {
    if (!workspace) return;
    const controller = new AbortController();
    dispatch({ type: 'listing', listing: { status: 'loading' } });
    void listFiles(files.directory, controller.signal)
      .then((listing) => {
        if (!controller.signal.aborted)
          dispatch({ type: 'listing', listing: { status: 'ready', value: listing } });
      })
      .catch((error) => {
        if (!controller.signal.aborted)
          dispatch({ type: 'listing', listing: { status: 'error', error: errorText(error) } });
      });
    return () => controller.abort();
  }, [workspace, files.directory, files.revision, dispatch]);
  const openFile = async (path: string) => {
    const current = filesRef.current.editor;
    if (current.kind === 'ready' && current.save === 'saving') return;
    if (
      isDirty(current) &&
      !window.confirm('저장하지 않은 파일 변경 사항을 버리고 다른 파일을 열까요?')
    )
      return;
    await reloadFile(path);
  };
  const saveFile = async () => {
    const editor = filesRef.current.editor;
    if (editor.kind !== 'ready' || editor.save === 'saving' || !isDirty(editor)) return;
    const root = workspace;
    const previousHash = editor.document.hash;
    dispatch({ type: 'save-start' });
    try {
      const document = await request<FileDocument>('/api/file', {
        method: 'PUT',
        body: {
          path: editor.document.path,
          content: editor.content,
          hash: previousHash,
        },
      });
      if (workspaceRef.current === root) dispatch({ type: 'file-saved', document, previousHash });
    } catch (error) {
      const detail =
        error instanceof ApiError && error.status === 409 && /changed on disk/i.test(error.message)
          ? `파일이 다른 곳에서 변경되었습니다. 입력 내용은 유지됩니다. 내용을 복사한 뒤 파일을 새로고침해 주세요. ${error.message}`
          : errorText(error);
      if (workspaceRef.current === root)
        dispatch({ type: 'save-error', path: editor.document.path, previousHash, error: detail });
    }
  };
  const refresh = () => {
    const editor = filesRef.current.editor;
    if (editor.kind === 'ready' && editor.save === 'saving') return;
    if (
      isDirty(editor) &&
      !window.confirm('파일을 새로고침하면 저장하지 않은 내용이 사라집니다. 계속할까요?')
    )
      return;
    dispatch({ type: 'refresh-files' });
    if (editor.kind === 'ready') {
      dispatch({ type: 'close-file' });
      void reloadFile(editor.document.path);
    } else if (editor.kind === 'error') {
      void reloadFile(editor.path);
    }
  };
  const reloadFile = async (path: string) => {
    const id = `${requestPrefix}-${++requestSequence.current}`;
    const root = workspace;
    dispatch({ type: 'open-file', path, requestId: id });
    try {
      const document = await readDocument(path);
      if (workspaceRef.current === root) dispatch({ type: 'file-loaded', document, requestId: id });
    } catch (error) {
      if (workspaceRef.current === root)
        dispatch({ type: 'file-error', path, error: errorText(error), requestId: id });
    }
  };
  useEffect(() => {
    const editor = filesRef.current.editor;
    if (!workspace || editor.kind !== 'ready' || editor.save === 'saving') return;
    const controller = new AbortController();
    const previousHash = editor.document.hash;
    void readDocument(editor.document.path, controller.signal).then(document => {
      if (!controller.signal.aborted) dispatch({ type: 'document-revalidated', document, previousHash });
    }).catch(error => {
      if (!controller.signal.aborted) dispatch({ type: 'save-error', path: editor.document.path, previousHash, error: errorText(error) });
    });
    return () => controller.abort();
  }, [workspace, files.documentRevision, dispatch]);
  const editor = files.editor;
  const saving = editor.kind === 'ready' && editor.save === 'saving';
  const segments = files.directory.split('/').filter(Boolean);
  return (
    <aside className={`computer-panel ${embedded ? 'embedded-files' : ''}`} aria-label="로컬 파일 탐색기">
      <header className="computer-header">
        <span>
          <Monitor size={16} />
          {embedded ? '작업 폴더' : '컴퓨터'}
        </span>
        <div>
          <IconButton label="파일 새로고침" onClick={refresh} disabled={!workspace || saving}>
            <RefreshCw size={15} />
          </IconButton>
          <IconButton label="컴퓨터 닫기" onClick={onClose} disabled={saving}>
            <X size={17} />
          </IconButton>
        </div>
      </header>
      <div className="execution-location">
        <span className="location-dot" />
        <span>서버 컴퓨터 · {window.location.host}</span>
      </div>
      {!workspace ? (
        <div className="file-empty">
          <FolderOpen size={30} strokeWidth={1.3} />
          <h3>작업 폴더를 연결하세요</h3>
          <p>서버 컴퓨터의 폴더를 연결하면 실제 파일을 열고 수정할 수 있습니다.</p>
          <button type="button" className="small-button" onClick={onSettings}>
            로컬 파일 설정
          </button>
        </div>
      ) : (
        <>
          <div className="workspace-path">
            <FolderOpen size={14} />
            <span title={workspace}>{workspace}</span>
            <IconButton label="작업 폴더 설정" onClick={onSettings}>
              <ChevronRight size={15} />
            </IconButton>
          </div>
          <nav className="breadcrumbs" aria-label="폴더 경로">
            <button
              type="button"
              onClick={() => dispatch({ type: 'directory', path: '' })}
              aria-label="작업 폴더 루트"
            >
              <Home size={13} />
            </button>
            {segments.map((segment, index) => (
              <span key={`${index}-${segment}`}>
                <ChevronRight size={12} />
                <button
                  type="button"
                  onClick={() =>
                    dispatch({ type: 'directory', path: segments.slice(0, index + 1).join('/') })
                  }
                >
                  {segment}
                </button>
              </span>
            ))}
          </nav>
          <div
            className={`file-browser ${editor.kind !== 'closed' ? 'with-editor' : ''}`}
            aria-label="파일 목록"
          >
            {files.listing.status === 'loading' && <Spinner label="파일 목록 불러오는 중" />}
            {files.listing.status === 'error' && (
              <div className="file-list-error">
                <ErrorNotice>{files.listing.error}</ErrorNotice>
                <button
                  type="button"
                  className="small-button"
                  onClick={() => dispatch({ type: 'refresh-files' })}
                >
                  다시 시도
                </button>
                {files.directory && (
                  <button
                    type="button"
                    className="text-button"
                    onClick={() => dispatch({ type: 'directory', path: '' })}
                  >
                    작업 폴더로 돌아가기
                  </button>
                )}
              </div>
            )}
            {files.listing.status === 'ready' && (
              <>
                {files.directory && (
                  <button
                    type="button"
                    className="file-entry"
                    onClick={() =>
                      dispatch({ type: 'directory', path: segments.slice(0, -1).join('/') })
                    }
                  >
                    <Folder size={16} />
                    <span>..</span>
                  </button>
                )}
                {[...files.listing.value.entries]
                  .sort((a, b) =>
                    a.type !== b.type
                      ? a.type === 'directory'
                        ? -1
                        : 1
                      : a.name.localeCompare(b.name),
                  )
                  .map((entry) => (
                    <button
                      key={entry.path}
                      type="button"
                      className={`file-entry ${editor.kind === 'ready' && editor.document.path === entry.path ? 'selected' : ''}`}
                      disabled={saving && entry.type === 'file'}
                      aria-label={`${entry.type === 'directory' ? '폴더' : '파일'} ${entry.name}`}
                      onClick={() =>
                        entry.type === 'directory'
                          ? dispatch({ type: 'directory', path: entry.path })
                          : void openFile(entry.path)
                      }
                    >
                      {entry.type === 'directory' ? <Folder size={16} /> : <FileText size={16} />}
                      <span>{entry.name}</span>
                      {entry.type === 'directory' && <ChevronRight size={13} />}
                    </button>
                  ))}
                {files.listing.value.entries.length === 0 && (
                  <p className="file-list-empty">이 폴더는 비어 있습니다.</p>
                )}
              </>
            )}
          </div>
          {editor.kind === 'closed' && (
            <div className="file-editor-placeholder">
              <FileText size={25} strokeWidth={1.2} />
              <span>텍스트 파일을 선택해 주세요.</span>
              <small>파일은 저장 버튼을 누를 때만 변경됩니다.</small>
            </div>
          )}
          {editor.kind === 'loading' && (
            <div className="file-editor-placeholder">
              <Spinner label="파일 여는 중" />
              <span className="path-text">{editor.path}</span>
            </div>
          )}
          {editor.kind === 'error' && (
            <div className="editor-error">
              <span className="path-text">{editor.path}</span>
              <ErrorNotice>{editor.error}</ErrorNotice>
              <button
                type="button"
                className="small-button"
                onClick={() => void reloadFile(editor.path)}
              >
                파일 다시 열기
              </button>
            </div>
          )}
          {editor.kind === 'ready' && (
            <section className="file-editor" aria-label="파일 편집기">
              <div className="editor-toolbar">
                <span className="editor-filename" title={editor.document.path}>
                  <FileText size={14} />
                  {editor.document.path}
                  {isDirty(editor) && <span className="unsaved-dot" title="저장하지 않음" />}
                </span>
                <button
                  type="button"
                  className="small-button"
                  aria-label="파일 저장"
                  disabled={saving || !isDirty(editor)}
                  onClick={() => void saveFile()}
                >
                  <Save size={14} />
                  {saving ? '저장 중…' : '저장'}
                </button>
              </div>
              {editor.error && <ErrorNotice>{editor.error}</ErrorNotice>}
              <textarea
                className="code-editor"
                aria-label="파일 내용"
                spellCheck={false}
                autoCapitalize="off"
                autoCorrect="off"
                value={editor.content}
                onChange={(event) => dispatch({ type: 'edit-file', content: event.target.value })}
                onKeyDown={(event) => {
                  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 's') {
                    event.preventDefault();
                    void saveFile();
                  }
                }}
              />
              <footer className="editor-status">
                <span role="status">
                  {saving
                    ? '서버에 저장 중'
                    : isDirty(editor)
                      ? '저장하지 않은 변경 사항'
                      : editor.save === 'saved'
                        ? '저장 완료'
                        : '텍스트 파일'}
                </span>
                <span>{editor.content.length.toLocaleString()}자</span>
              </footer>
            </section>
          )}
        </>
      )}
    </aside>
  );
}
