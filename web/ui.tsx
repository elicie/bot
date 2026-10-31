import { useEffect, useRef, type ButtonHTMLAttributes, type ReactNode } from 'react';
import { AlertCircle, LoaderCircle, X } from 'lucide-react';

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
export function Avatar({
  color = '#67a8c8',
  seed = 'bot',
  size = 34,
}: {
  color?: string;
  seed?: string;
  size?: number;
}) {
  const variant = [...seed].reduce((sum, letter) => sum + letter.charCodeAt(0), 0) % 3;
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 40 40"
      fill="none"
      className="avatar"
      aria-hidden="true"
    >
      {variant === 0 ? (
        <>
          <path d="M20 3 34 11 37 26 27 37 11 36 3 24 6 10Z" fill={color} />
          <path d="m8 8 6 2M31 8l-5 3" stroke={color} strokeWidth="4" strokeLinecap="round" />
        </>
      ) : variant === 1 ? (
        <>
          <rect x="3" y="7" width="34" height="29" rx="13" fill={color} />
          <path d="M15 8 18 3l4 5" fill={color} />
        </>
      ) : (
        <path d="M20 3c11 0 17 8 17 18S29 37 18 36 3 29 3 19 9 3 20 3Z" fill={color} />
      )}
      <rect
        x="12"
        y="15"
        width="4"
        height="8"
        rx="2"
        fill="#122027"
        transform={variant === 2 ? 'rotate(-12 14 19)' : undefined}
      />
      <rect
        x="25"
        y="15"
        width="4"
        height="8"
        rx="2"
        fill="#122027"
        transform={variant === 2 ? 'rotate(-12 27 19)' : undefined}
      />
      {variant === 0 && (
        <path d="M16 29h8" stroke="#122027" strokeWidth="2" strokeLinecap="round" />
      )}
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
          'button:not(:disabled), input:not(:disabled), textarea:not(:disabled), a[href], [tabindex="0"]',
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
