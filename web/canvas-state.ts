import type { Canvas, CanvasInput, CanvasVersion } from '../shared/types';

export interface CanvasDraft {
  base: CanvasInput;
  redacted: boolean;
  title: string;
  language: string;
  content: string;
}
export function canvasDraft(canvas?: CanvasVersion): CanvasDraft {
  const base: CanvasInput = canvas
    ? { revision: canvas.revision, title: canvas.title, language: canvas.language, content: canvas.content }
    : { revision: 0, title: '문서.md', language: 'markdown', content: '' };
  return { base, redacted: !!canvas?.redacted, title: base.title, language: base.language, content: base.content };
}
export const canvasDirty = (draft: CanvasDraft) => draft.title !== draft.base.title || draft.language !== draft.base.language || draft.content !== draft.base.content;
/** Secret configuration can change the public projection without changing the document revision. */
export function canvasStale(draft: CanvasDraft, latest?: Canvas): boolean {
  const projected = canvasDraft(latest);
  return projected.base.revision !== draft.base.revision || projected.base.title !== draft.base.title
    || projected.base.language !== draft.base.language || projected.base.content !== draft.base.content || projected.redacted !== draft.redacted;
}
/** Live snapshots never overwrite an unsaved draft; stale/projection-changed drafts cannot save. */
export function reconcileCanvas(draft: CanvasDraft, latest?: Canvas): CanvasDraft {
  if (!canvasStale(draft, latest) || canvasDirty(draft)) return draft;
  return canvasDraft(latest);
}
export function canvasFilename(version: Pick<CanvasVersion, 'title' | 'language'>): string {
  const safe = version.title.replace(/[\\/:*?"<>|\u0000-\u001f]/g, '_').replace(/^\.+/, '').trim() || 'canvas';
  const extensions: Record<string, string> = { markdown: 'md', text: 'txt', javascript: 'js', typescript: 'ts', python: 'py', rust: 'rs', html: 'html', css: 'css', json: 'json' };
  return /\.[a-z0-9]+$/i.test(safe) ? safe : `${safe}.${extensions[version.language] ?? 'txt'}`;
}
export function canvasExportRequest(version: CanvasVersion): string {
  return `이 대화의 캔버스를 읽고 버전이 ${version.revision}인지 확인한 뒤, 작업 폴더의 ${JSON.stringify(canvasFilename(version))} 파일로 저장해 줘. 버전이 달라졌다면 저장하지 말고 알려줘. 기존 파일이 있으면 먼저 읽어 변경 충돌을 확인하고, 기존 파일 쓰기 승인 절차를 사용해 줘.`;
}
