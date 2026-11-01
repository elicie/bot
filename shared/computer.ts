export const COMPUTER_VIEWPORT = { width: 960, height: 600 } as const;

export const COMPUTER_KEYS = [
  'Enter', 'Tab', 'Shift+Tab', 'Escape', 'Backspace', 'Delete',
  'ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight',
  'Home', 'End', 'PageUp', 'PageDown', 'Space',
] as const;

export type ComputerKey = typeof COMPUTER_KEYS[number];
export type ComputerAction =
  | { type: 'navigate'; url: string }
  | { type: 'click'; x: number; y: number }
  | { type: 'type'; text: string }
  | { type: 'key'; key: ComputerKey }
  | { type: 'scroll'; deltaY: number }
  | { type: 'observe' };

export interface ComputerStartInput {
  requestId: string;
  userInitiated: true;
  origins: string[];
}

export interface ComputerFrameRequest {
  requestId: string;
  epoch: string;
  frameId: string | null;
}

export interface ComputerActionRequest extends ComputerFrameRequest {
  action: ComputerAction;
}

export interface ComputerStatus {
  state: 'stopped' | 'starting' | 'running' | 'stopping';
  epoch: string;
  frameId: string | null;
  viewport: typeof COMPUTER_VIEWPORT;
  origins: string[];
  url: string | null;
  busy: boolean;
  sandboxed: boolean;
  error?: string;
}

export interface ComputerControl {
  id: string;
  role: 'link' | 'button' | 'textbox' | 'checkbox' | 'radio' | 'select' | 'control';
  label: string;
  bounds: { x: number; y: number; width: number; height: number };
  disabled: boolean;
  editable: boolean;
  blocked: boolean;
}

export interface ComputerFrame {
  id: string;
  epoch: string;
  url: string;
  title: string;
  viewport: typeof COMPUTER_VIEWPORT;
  controls: ComputerControl[];
  mimeType: 'image/png';
}

export interface ComputerObservation {
  status: ComputerStatus;
  frame: ComputerFrame;
}
