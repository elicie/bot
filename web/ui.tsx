import { useEffect, useRef, type ButtonHTMLAttributes, type ReactNode } from 'react';
import { AlertCircle, LoaderCircle, X } from 'lucide-react';
import type { AvatarShape } from '../shared/types';

export function IconButton({
  label,
  children,
  className = '',
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & { label: string; children: ReactNode }) {
  return (
    <button
      type="button"
      className={`icon-button ${className}`}
      aria-label={label}
      title={label}
      {...props}
    >
      {children}
    </button>
  );
}
export const avatarShapes = [
  { value: 'blob', label: '방울형', path: 'M31 5C43 3 56 13 58 26C62 40 51 56 37 59C22 63 7 54 5 40C2 27 8 8 22 6C25 5 28 6 31 5Z' },
  { value: 'cloud', label: '구름형', path: 'M17 20C15 11 23 5 31 7C37 1 49 7 49 16C60 16 64 28 58 36C65 45 56 57 46 55C40 63 28 60 24 55C13 60 2 51 6 41C-1 34 5 22 17 20Z' },
  { value: 'square', label: '사각형', path: 'M16 6H48C55 6 58 10 58 17V47C58 55 54 58 47 58H17C10 58 6 54 6 47V18C6 10 10 6 16 6Z' },
  { value: 'spark', label: '반짝이형', path: 'M32 3C35 3 36 18 42 22C47 27 61 28 61 32C61 36 46 37 42 42C37 47 36 61 32 61C28 61 27 46 22 42C17 37 3 36 3 32C3 28 18 27 22 22C27 17 28 3 32 3Z' },
  { value: 'clover', label: '클로버형', path: 'M32 13C44-1 63 16 51 29C67 40 49 62 35 50C22 66 2 49 15 35C-1 22 16 2 30 14Z' },
  { value: 'heart', label: '하트형', path: 'M32 58C26 54 6 40 5 25C4 12 19 4 29 13L32 16L35 13C45 4 60 12 59 25C58 40 38 54 32 58Z' },
  { value: 'flower', label: '꽃형', path: 'M32 9C40-1 51 7 49 18C62 18 65 31 55 37C63 47 53 59 43 54C39 66 25 66 21 54C10 60 0 48 9 38C-2 31 3 18 15 18C13 6 25-1 32 9Z' },
  { value: 'droplet', label: '물방울형', path: 'M32 3C28 12 7 26 7 39C7 52 18 60 32 60S57 52 57 39C57 26 36 12 32 3Z' },
  { value: 'pill', label: '알약형', path: 'M32 4C44 4 52 13 52 25V40C52 52 44 60 32 60S12 52 12 40V25C12 13 20 4 32 4Z' },
  { value: 'wedge', label: '쐐기형', path: 'M16 8C18 4 23 4 26 7L59 43C64 49 60 57 52 57H12C5 57 2 52 4 45Z' },
  { value: 'house', label: '집형', path: 'M8 25L27 6C30 3 34 3 37 6L56 25C58 27 59 30 59 33V50C59 56 56 59 50 59H14C8 59 5 56 5 50V33C5 30 6 27 8 25Z' },
  { value: 'star', label: '별형', path: 'M28 7C30 2 34 2 36 7L42 20L56 22C62 23 63 27 59 31L48 41L51 55C52 61 48 63 43 60L32 53L21 60C16 63 12 61 13 55L16 41L5 31C1 27 2 23 8 22L22 20Z' },
  { value: 'hexagon', label: '육각형', path: 'M24 5H40C44 5 46 7 48 10L59 28C61 31 61 34 59 37L48 55C46 58 44 59 40 59H24C20 59 18 58 16 55L5 37C3 34 3 31 5 28L16 10C18 7 20 5 24 5Z' },
  { value: 'pumpkin', label: '호박형', path: 'M31 11C29 5 33 2 39 3L38 11C50 6 62 20 60 36C59 52 49 62 36 58C22 65 6 55 4 39C1 23 13 8 25 11L31 11Z' },
] as const satisfies readonly { value: AvatarShape; label: string; path: string }[];

export function Avatar({
  color = '#67a8c8',
  seed = 'bot',
  size = 34,
  shape,
}: {
  color?: string;
  seed?: string;
  size?: number;
  shape?: AvatarShape;
}) {
  const variant = [...seed].reduce((sum, letter) => sum + letter.charCodeAt(0), 0) % 3;
  const fallback = (['blob', 'house', 'hexagon'] as const)[variant];
  const selectedShape = avatarShapes.find(option => option.value === (shape ?? fallback))!;
  return (
    <svg width={size} height={size} viewBox="0 0 64 64" fill="none" className="avatar" aria-hidden="true">
      <g transform="rotate(-22 32 32) translate(3 3) scale(.91)">
        <path d={selectedShape.path} fill={color} />
        <rect x="23" y="22" width="6" height="15" rx="3" fill="#202020" />
        <rect x="38" y="22" width="6" height="15" rx="3" fill="#202020" />
      </g>
    </svg>
  );
}
export function Spinner({ label = '불러오는 중' }: { label?: string }) {
  return (
    <span className="loading-state" role="status">
      <LoaderCircle size={16} className="spin" aria-hidden="true" />
      {label}
    </span>
  );
}
export function ErrorNotice({
  children,
  onDismiss,
}: {
  children: ReactNode;
  onDismiss?: () => void;
}) {
  return (
    <div className="error-notice" role="alert">
      <AlertCircle size={16} aria-hidden="true" />
      <span>{children}</span>
      {onDismiss && (
        <IconButton label="오류 닫기" onClick={onDismiss}>
          <X size={15} />
        </IconButton>
      )}
    </div>
  );
}
export function Modal({
  title,
  children,
  onClose,
  className = '',
}: {
  title: string;
  children: ReactNode;
  onClose: () => void;
  className?: string;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  const closeRef = useRef(onClose);
  closeRef.current = onClose;
  useEffect(() => {
    const dialog = ref.current;
    if (!dialog) return;
    const previous = document.activeElement;
    dialog.showModal();
    const target = dialog.querySelector<HTMLElement>('[data-autofocus]');
    target?.focus();
    const trap = (event: KeyboardEvent) => {
      if (event.key !== 'Tab') return;
      const controls = [
        ...dialog.querySelectorAll<HTMLElement>(
          'button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), a[href], [tabindex="0"]',
        ),
      ].filter((element) => element.getClientRects().length > 0);
      const first = controls[0];
      const last = controls.at(-1);
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last?.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first?.focus();
      }
    };
    dialog.addEventListener('keydown', trap);
    const cancel = (event: Event) => {
      event.preventDefault();
      closeRef.current();
    };
    dialog.addEventListener('cancel', cancel);
    return () => {
      dialog.removeEventListener('cancel', cancel);
      dialog.removeEventListener('keydown', trap);
      dialog.close();
      if (previous instanceof HTMLElement && previous.isConnected) previous.focus();
    };
  }, []);
  return (
    <dialog
      ref={ref}
      className={`modal ${className}`}
      aria-label={title}
      onClick={(event) => {
        if (event.target !== event.currentTarget) return;
        const bounds = event.currentTarget.getBoundingClientRect();
        if (
          event.clientX < bounds.left ||
          event.clientX > bounds.right ||
          event.clientY < bounds.top ||
          event.clientY > bounds.bottom
        )
          closeRef.current();
      }}
    >
      {children}
    </dialog>
  );
}
