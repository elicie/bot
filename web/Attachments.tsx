// Composer attachments (file picker, drag and drop, paste, chips with remove) and the transcript's
// "사용자 첨부 파일" list. Limits mirror `grok_bot_store::attachments`; the server re-validates.

import { useEffect, useMemo, useRef, useState, type DragEvent, type ReactNode } from 'react';
import { FileText, Paperclip, X } from 'lucide-react';
import type { Attachment, AttachmentUpload } from '../shared/types';
import { IconButton } from './ui';
import './chat-features.css';

export const MAX_ATTACHMENTS = 8;
export const MAX_TEXT_BYTES = 256 * 1024;
export const MAX_TEXT_TOTAL_BYTES = 512 * 1024;
export const MAX_IMAGE_BYTES = 5 * 1024 * 1024;
export const MAX_TOTAL_BYTES = 10 * 1024 * 1024;
const imageTypes = ['image/png', 'image/jpeg', 'image/gif', 'image/webp'];

export interface PendingAttachment {
  key: string;
  file: File;
  kind: 'text' | 'image';
}

/** What the composer needs to show and change the pending attachments of the current draft. */
export interface ComposerAttachments {
  items: PendingAttachment[];
  errors: string[];
  add: (files: File[]) => void;
  remove: (key: string) => void;
  dismiss: () => void;
}

export function sizeLabel(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${Number((bytes / (1024 * 1024)).toFixed(1))} MB`;
  if (bytes >= 1024) return `${Math.ceil(bytes / 1024)} KB`;
  return `${bytes} B`;
}

async function isText(file: File): Promise<boolean> {
  try {
    const bytes = new Uint8Array(await file.arrayBuffer());
    if (bytes.includes(0)) return false;
    new TextDecoder('utf-8', { fatal: true }).decode(bytes);
    return true;
  } catch {
    return false;
  }
}

/** Validate new files against what is already attached. Returns accepted files and error lines. */
export async function admitFiles(existing: PendingAttachment[], files: File[]): Promise<{ added: PendingAttachment[]; errors: string[] }> {
  const added: PendingAttachment[] = [];
  const errors: string[] = [];
  let total = existing.reduce((sum, item) => sum + item.file.size, 0);
  let textTotal = existing.filter((item) => item.kind === 'text').reduce((sum, item) => sum + item.file.size, 0);
  let overTotal = 0;
  let overCount = 0;
  for (const file of files) {
    const quoted = `"${file.name}"`;
    if (file.size === 0) { errors.push(`${quoted}은(는) 비어 있어서 첨부되지 않았어요`); continue; }
    const image = imageTypes.includes(file.type);
    if (!image && file.type.startsWith('image/')) { errors.push(`${quoted}은(는) 첨부할 수 없어요. PNG, JPEG, GIF, WebP 이미지나 텍스트 파일을 선택하세요.`); continue; }
    const limit = image ? MAX_IMAGE_BYTES : MAX_TEXT_BYTES;
    if (file.size > limit) { errors.push(`${quoted}은(는) 너무 커서 첨부할 수 없어요 (최대 ${sizeLabel(limit)})`); continue; }
    if (!image && !(await isText(file))) { errors.push(`${quoted}은(는) 첨부할 수 없어요. 텍스트 파일이나 PNG, JPEG, GIF, WebP 이미지를 선택하세요.`); continue; }
    if (existing.length + added.length >= MAX_ATTACHMENTS) { overCount++; continue; }
    if (total + file.size > MAX_TOTAL_BYTES || (!image && textTotal + file.size > MAX_TEXT_TOTAL_BYTES)) { overTotal++; continue; }
    total += file.size;
    if (!image) textTotal += file.size;
    added.push({ key: crypto.randomUUID?.() ?? `${Date.now()}-${Math.random()}`, file, kind: image ? 'image' : 'text' });
  }
  if (overCount) errors.push(`한 메시지에는 파일을 ${MAX_ATTACHMENTS}개까지 첨부할 수 있어 ${overCount}개는 추가되지 않았어요`);
  if (overTotal) errors.push(`한 메시지의 첨부 파일은 합계 ${sizeLabel(MAX_TOTAL_BYTES)}(텍스트 ${sizeLabel(MAX_TEXT_TOTAL_BYTES)})까지만 추가할 수 있어 ${overTotal}개는 추가되지 않았어요`);
  return { added, errors };
}

function base64(buffer: ArrayBuffer): string {
  const bytes = new Uint8Array(buffer);
  let binary = '';
  for (let index = 0; index < bytes.length; index += 0x8000) binary += String.fromCharCode(...bytes.subarray(index, index + 0x8000));
  return btoa(binary);
}

export async function uploads(items: PendingAttachment[]): Promise<AttachmentUpload[]> {
  return Promise.all(items.map(async (item) => ({
    name: item.file.name,
    mimeType: item.file.type || (item.kind === 'text' ? 'text/plain' : ''),
    data: base64(await item.file.arrayBuffer()),
  })));
}

export function AttachButton({ disabled, onFiles }: { disabled: boolean; onFiles: (files: File[]) => void }) {
  const input = useRef<HTMLInputElement>(null);
  return <>
    <IconButton label="파일 첨부" className="attach-button attach-file-button" disabled={disabled} onClick={() => input.current?.click()}>
      <Paperclip size={16} />
    </IconButton>
    <input ref={input} type="file" multiple hidden aria-hidden="true" tabIndex={-1} data-testid="attachment-input"
      onChange={(event) => { const files = [...(event.target.files ?? [])]; event.target.value = ''; if (files.length) onFiles(files); }} />
  </>;
}

/** Drag-and-drop handlers for the composer dock plus the "놓아서 첨부" overlay state. */
export function useFileDrop(enabled: boolean, onFiles: (files: File[]) => void) {
  const [dragging, setDragging] = useState(false);
  const depth = useRef(0);
  const carriesFiles = (event: DragEvent) => [...event.dataTransfer.types].includes('Files');
  return {
    dragging,
    handlers: {
      onDragEnter: (event: DragEvent) => { if (!enabled || !carriesFiles(event)) return; event.preventDefault(); depth.current++; setDragging(true); },
      onDragOver: (event: DragEvent) => { if (!enabled || !carriesFiles(event)) return; event.preventDefault(); event.dataTransfer.dropEffect = 'copy'; },
      onDragLeave: () => { if (!enabled) return; depth.current = Math.max(0, depth.current - 1); if (!depth.current) setDragging(false); },
      onDrop: (event: DragEvent) => {
        if (!enabled || !carriesFiles(event)) return;
        event.preventDefault();
        depth.current = 0;
        setDragging(false);
        const files = [...event.dataTransfer.files];
        if (files.length) onFiles(files);
      },
    },
  };
}

function ImagePreview({ file }: { file: File }) {
  const url = useMemo(() => URL.createObjectURL(file), [file]);
  useEffect(() => () => URL.revokeObjectURL(url), [url]);
  return <img src={url} alt="" />;
}

export function AttachmentTray({ items, errors, disabled, onRemove, onDismiss }: {
  items: PendingAttachment[]; errors: string[]; disabled: boolean;
  onRemove: (key: string) => void; onDismiss: () => void;
}) {
  if (!items.length && !errors.length) return null;
  return <div className="attachment-tray">
    {!!items.length && <ul className="attachment-chips" aria-label="첨부할 파일">
      {items.map((item) => <li key={item.key} className={`attachment-chip is-${item.kind}`}>
        <span className="attachment-thumb">{item.kind === 'image' ? <ImagePreview file={item.file} /> : <FileText size={15} />}</span>
        <span className="attachment-name" title={item.file.name}>{item.file.name}<small>{sizeLabel(item.file.size)}</small></span>
        <IconButton label={`${item.file.name} 첨부 취소`} disabled={disabled} onClick={() => onRemove(item.key)}><X size={12} /></IconButton>
      </li>)}
    </ul>}
    {!!errors.length && <div className="attachment-error" role="alert">
      <div>{errors.map((error, index) => <p key={index}>{error}</p>)}</div>
      <IconButton label="첨부 오류 닫기" onClick={onDismiss}><X size={12} /></IconButton>
    </div>}
  </div>;
}

export function DropOverlay({ show }: { show: boolean }): ReactNode {
  return show ? <div className="attachment-drop" aria-hidden="true"><Paperclip size={18} />놓아서 첨부</div> : null;
}

/** "사용자 첨부 파일" in a sent user message. */
export function MessageAttachments({ conversationId, attachments }: { conversationId: string; attachments: Attachment[] }) {
  const url = (attachment: Attachment) => `/api/conversations/${encodeURIComponent(conversationId)}/attachments/${encodeURIComponent(attachment.id)}`;
  return <ul className="message-attachments" aria-label="사용자 첨부 파일">
    {attachments.map((attachment) => <li key={attachment.id}>
      {attachment.kind === 'image'
        ? <a className="message-attachment-image" href={url(attachment)} target="_blank" rel="noopener noreferrer" title={attachment.name}>
            <img src={url(attachment)} alt={attachment.name} loading="lazy" />
          </a>
        : <a className="attachment-chip is-text" href={url(attachment)} target="_blank" rel="noopener noreferrer" title={attachment.name}>
            <span className="attachment-thumb"><FileText size={15} /></span>
            <span className="attachment-name">{attachment.name}<small>{sizeLabel(attachment.size)}</small></span>
          </a>}
    </li>)}
  </ul>;
}
