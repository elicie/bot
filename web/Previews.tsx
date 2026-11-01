// Full-screen previews for chat media: "다이어그램 미리보기" (rendered Mermaid diagrams), "미디어
// 미리보기" (image attachments and inline images), and the frame for "시각화 미리보기".
//
// Each preview is a modal <dialog> in the top layer (rendered into document.body): focus moves to
// its close button, Tab stays inside, Escape or the close button closes it, and focus returns to
// the control that opened it. Images can be zoomed with the toolbar, Ctrl/⌘ + wheel, or the
// + / - / 0 keys, and scrolled when larger than the screen.

import { useState, type KeyboardEvent, type ReactNode, type WheelEvent } from 'react';
import { createPortal } from 'react-dom';
import { Download, Maximize2, Minus, Plus, Scan, X } from 'lucide-react';
import { IconButton, Modal } from './ui';
import './previews.css';

export function PreviewModal({ title, closeLabel, heading, className = '', toolbar, onClose, onKeyDown, children }: {
  /** The dialog's accessible name, e.g. "다이어그램 미리보기". */
  title: string;
  closeLabel: string;
  /** Visible heading (the item's own name). */
  heading?: string;
  className?: string;
  toolbar?: ReactNode;
  onClose: () => void;
  onKeyDown?: (event: KeyboardEvent<HTMLDivElement>) => void;
  children: (close: () => void) => ReactNode;
}) {
  return createPortal(<Modal title={title} onClose={onClose} className={`preview-modal ${className}`}>
    <div className="preview-shell" onKeyDown={onKeyDown}>
      <header className="preview-header">
        <h2 className="preview-heading" title={heading ?? title}>{heading ?? title}</h2>
        <div className="preview-toolbar">
          {toolbar}
          <IconButton label={closeLabel} onClick={onClose} data-autofocus><X size={18} /></IconButton>
        </div>
      </header>
      <div className="preview-content">{children(onClose)}</div>
    </div>
  </Modal>, document.body);
}

const MIN_SCALE = 0.25;
const MAX_SCALE = 8;
const clampScale = (value: number) => Math.min(MAX_SCALE, Math.max(MIN_SCALE, Math.round(value * 100) / 100));

/** An image preview with zoom. `width`/`height` are its drawing size when the image has none. */
function ZoomPreview({ title, closeLabel, heading, src, alt, width, height, download, onClose }: {
  title: string; closeLabel: string; heading: string; src: string; alt: string;
  width?: number; height?: number;
  /** File name for "미디어 다운로드". */
  download?: string;
  onClose: () => void;
}) {
  const [scale, setScale] = useState<number | null>(null);
  const [natural, setNatural] = useState<{ width: number; height: number } | null>(width && height ? { width, height } : null);
  const zoom = (factor: number) => setScale((current) => clampScale((current ?? 1) * factor));
  const fit = () => setScale(null);
  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.altKey || event.ctrlKey || event.metaKey) return;
    if (event.key === '+' || event.key === '=') { event.preventDefault(); zoom(1.25); }
    else if (event.key === '-' || event.key === '_') { event.preventDefault(); zoom(0.8); }
    else if (event.key === '0') { event.preventDefault(); fit(); }
  };
  const onWheel = (event: WheelEvent<HTMLDivElement>) => {
    if (!event.ctrlKey && !event.metaKey) return;
    event.preventDefault();
    zoom(event.deltaY < 0 ? 1.1 : 0.9);
  };
  const sized = scale !== null && natural;
  const toolbar = <>
    <IconButton label="축소" onClick={() => zoom(0.8)} disabled={scale !== null && scale <= MIN_SCALE}><Minus size={16} /></IconButton>
    <output className="preview-zoom" aria-label="확대/축소">{scale === null ? '맞춤' : `${Math.round(scale * 100)}%`}</output>
    <IconButton label="확대" onClick={() => zoom(1.25)} disabled={scale !== null && scale >= MAX_SCALE}><Plus size={16} /></IconButton>
    <IconButton label="화면에 맞추기" onClick={fit} disabled={scale === null}><Scan size={15} /></IconButton>
    {download && <a className="icon-button" href={src} download={download} aria-label="미디어 다운로드" title="미디어 다운로드"><Download size={16} /></a>}
  </>;
  return <PreviewModal title={title} closeLabel={closeLabel} heading={heading} className="zoom-preview" toolbar={toolbar} onClose={onClose} onKeyDown={onKeyDown}>
    {() => <div className={`preview-canvas ${sized ? 'is-zoomed' : ''}`} tabIndex={0} aria-label={`${heading} (확대/축소: + / - / 0)`} onWheel={onWheel}>
      <img src={src} alt={alt} draggable={false}
        style={sized ? { width: natural.width * scale, height: natural.height * scale, maxWidth: 'none', maxHeight: 'none' } : undefined}
        onLoad={(event) => {
          const image = event.currentTarget;
          if (!natural && image.naturalWidth && image.naturalHeight) setNatural({ width: image.naturalWidth, height: image.naturalHeight });
        }} />
    </div>}
  </PreviewModal>;
}

export function DiagramPreview({ src, label, width, height, onClose }: { src: string; label: string; width?: number; height?: number; onClose: () => void }) {
  return <ZoomPreview title="다이어그램 미리보기" closeLabel="다이어그램 미리보기 닫기" heading={`Mermaid 다이어그램 (${label})`}
    src={src} alt={`Mermaid 다이어그램 (${label})`} width={width} height={height} onClose={onClose} />;
}

export function MediaPreview({ src, name, download, onClose }: { src: string; name: string; download?: string; onClose: () => void }) {
  return <ZoomPreview title="미디어 미리보기" closeLabel="미디어 미리보기 닫기" heading={name || '이미지'} src={src} alt={name || '이미지'} download={download} onClose={onClose} />;
}

/**
 * A preview trigger around an image: clicking the image opens the preview, and a labelled button
 * (shown on hover and focus, always on touch screens) does the same from the keyboard. The image
 * keeps its own role and name.
 */
export function PreviewTrigger({ label, className = '', onOpen, children }: { label: string; className?: string; onOpen: () => void; children: ReactNode }) {
  return <span className={`preview-trigger ${className}`} onClick={(event) => { if ((event.target as Element).closest('button') === null) onOpen(); }}>
    {children}
    <IconButton label={label} className="preview-open" aria-haspopup="dialog" onClick={onOpen}><Maximize2 size={13} /></IconButton>
  </span>;
}

/** An image that opens "미디어 미리보기" ("이미지 전체 화면으로 열기"). */
export function PreviewableImage({ src, alt, className = '', download, loading }: { src: string; alt: string; className?: string; download?: string; loading?: 'lazy' }) {
  const [open, setOpen] = useState(false);
  return <>
    <PreviewTrigger label="이미지 전체 화면으로 열기" className={`previewable-image ${className}`} onOpen={() => setOpen(true)}>
      <img src={src} alt={alt} loading={loading} />
    </PreviewTrigger>
    {open && <MediaPreview src={src} name={alt} download={download} onClose={() => setOpen(false)} />}
  </>;
}
