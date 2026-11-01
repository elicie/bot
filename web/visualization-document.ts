// Builds the documents that visualization previews ("시각화") run in, and checks the messages
// those documents send back. No DOM or React here, so the rules can be tested directly.
//
// Isolation model (see `Visualization.tsx` for the frame itself):
// - The document runs in `<iframe sandbox="allow-scripts" srcdoc>`: an opaque origin, so it cannot
//   read the app (`parent.document`, storage, cookies) and the app's API answers it with CORS
//   errors. No `allow-same-origin`, `allow-top-navigation`, `allow-forms`, or `allow-popups`.
// - Its first element is a Content-Security-Policy that allows inline scripts and styles plus
//   `data:`/`blob:` images, fonts, and media, and nothing else: no fetch/XHR/WebSocket, no
//   external scripts, styles, images, frames, or workers. It precedes every byte of the source, so
//   the policy is in force before any of it is parsed.
// - Navigating the frame away from its srcdoc (scripted `location` changes, `<meta
//   http-equiv="refresh">`, link clicks) is not covered by a CSP inside the frame. The app page
//   sets `frame-src 'none'` (index.html), so the browser refuses such a navigation and the card
//   reports it. Link clicks are cancelled earlier by the bootstrap and only noted.
// - A bootstrap script runs before the source. It reports load, script errors, size, blocked
//   requests, and Escape to the app with a per-frame nonce, then removes itself so the source
//   cannot read the nonce. The app also checks `event.source` against its own frame, so messages
//   from other frames (or the page) are ignored.
//
// Known gaps (Chromium, not fixable from a web page):
// - CSP does not cover WebRTC. The bootstrap removes the WebRTC constructors from the frame's
//   window, but a script could still reach them through a nested about:blank frame.
// - A scripted navigation makes Chromium resolve and connect to the target origin (no HTTP request
//   is sent) before `frame-src` refuses it, so only the host name can leave the machine (DNS). The
//   Navigation API cannot cancel it first: it fires no events in opaque-origin documents.
// - All sandboxed frames of the page share one process, so a visualization stuck in an endless
//   loop stalls the others until the page is reloaded (the timeout message says so).

/** Fence languages shown as previews. `visualization` gets the bundled d3 runtime. */
export type VisualizationKind = 'html' | 'svg' | 'visualization';
export const visualizationLanguages: Record<string, VisualizationKind> = {
  html: 'html',
  svg: 'svg',
  visualization: 'visualization',
};

/** Longest preview source, in UTF-16 code units. Longer sources show only their code. */
export const MAX_VISUALIZATION_SOURCE = 200_000;
/** Time from mounting the frame to the document's load event. */
export const VISUALIZATION_START_TIMEOUT_MS = 10_000;
export const MIN_INLINE_HEIGHT = 120;
export const MAX_INLINE_HEIGHT = 640;
const MAX_REPORTED_TEXT = 500;
export const CHANNEL = 'grok-bot-visualization';

export const VISUALIZATION_CSP =
  "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob:; font-src data:; media-src data: blob:; form-action 'none'";

/** Permissions the frame never gets (cross-origin frames lack most of them by default already). */
export const VISUALIZATION_ALLOW =
  "camera 'none'; microphone 'none'; geolocation 'none'; display-capture 'none'; clipboard-read 'none'; clipboard-write 'none'; fullscreen 'none'; payment 'none'; usb 'none'; serial 'none'; hid 'none'; bluetooth 'none'";

export const visualizationLabels = {
  html: 'HTML 미리보기',
  svg: 'SVG 미리보기',
  visualization: '시각화',
} as const satisfies Record<VisualizationKind, string>;

export function newNonce(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return [...bytes].map((byte) => byte.toString(16).padStart(2, '0')).join('');
}

/** `<title>` of an HTML source, as plain text, for the card heading. */
export function sourceTitle(source: string): string | null {
  const match = /<title[^>]*>([^<]{1,200})<\/title>/i.exec(source);
  const title = match?.[1].replace(/\s+/g, ' ').trim();
  return title ? title.replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&#39;/g, "'").replace(/&amp;/g, '&') : null;
}

/** Whether the source uses d3 (and so gets the bundled runtime). */
export function wantsD3(kind: VisualizationKind, source: string): boolean {
  return kind === 'visualization' || /\bd3\s*\.\s*[a-zA-Z]/.test(source);
}

/** A runtime script can be inlined only if it cannot end its `<script>` element early. */
export function inlineSafe(script: string): boolean {
  return !/<\/script|<!--|<script/i.test(script);
}

export type SourceProblem = 'empty' | 'too-large';
export function sourceProblem(source: string): SourceProblem | null {
  if (!source.trim()) return 'empty';
  if (source.length > MAX_VISUALIZATION_SOURCE) return 'too-large';
  return null;
}

/**
 * The bootstrap that runs first in every preview document, as one line (so the source's line
 * numbers in error reports stay its own; `lineOffset` subtracts the runtime's lines). `nonce` is
 * hex, so it is safe in JS. Each message is one object literal: no property assignment that a
 * setter planted by the source on Object.prototype could observe (it would see the nonce). Escape
 * is reported on keyup, so the frame is not removed while the key is still being delivered to it.
 */
function bootstrap(nonce: string, lineOffset: number): string {
  if (!/^[0-9a-f]{16,64}$/.test(nonce)) throw new Error('invalid nonce');
  return `(function () {
  var nonce = "${nonce}";
  var target = window.parent;
  var post = target.postMessage.bind(target);
  var limit = ${MAX_REPORTED_TEXT};
  var offset = ${Math.max(0, Math.floor(lineOffset))};
  function send(type, message, height, count) {
    try { post({ channel: "${CHANNEL}", nonce: nonce, type: type, message: message, height: height, count: count }, "*"); } catch (error) {}
  }
  function text(value) { try { return String(value).slice(0, limit); } catch (error) { return ""; } }
  window.addEventListener("error", function (event) {
    if (!(event instanceof ErrorEvent)) return;
    var where = event.lineno > offset ? " (줄 " + (event.lineno - offset) + ")" : "";
    send("error", text(event.message || "Script error") + where);
  });
  window.addEventListener("unhandledrejection", function (event) {
    var reason = event.reason;
    send("error", text(reason && reason.message ? reason.message : reason));
  });
  var blocked = 0;
  document.addEventListener("securitypolicyviolation", function () { blocked++; send("blocked", undefined, undefined, blocked); });
  window.addEventListener("keyup", function (event) { if (event.key === "Escape") send("escape"); });
  function guardLinks(event) {
    var node = event.target;
    while (node && !(node.nodeType === 1 && (node.localName === "a" || node.localName === "area") && node.hasAttribute("href"))) node = node.parentNode;
    if (!node) return;
    var href = node.getAttribute("href") || "";
    if (href.charAt(0) === "#") return;
    event.preventDefault();
    send("navigation");
  }
  window.addEventListener("click", guardLinks, true);
  window.addEventListener("auxclick", guardLinks, true);
  ["RTCPeerConnection", "webkitRTCPeerConnection", "RTCDataChannel", "RTCRtpSender", "RTCRtpReceiver", "RTCIceTransport"].forEach(function (name) {
    try { Object.defineProperty(window, name, { value: undefined, configurable: false, writable: false }); } catch (error) {}
  });
  var lastHeight = -1;
  function size() {
    var root = document.documentElement;
    var height = Math.ceil(Math.max(root ? root.scrollHeight : 0, document.body ? document.body.scrollHeight : 0));
    if (height !== lastHeight) { lastHeight = height; send("size", undefined, height); }
  }
  window.addEventListener("load", function () {
    send("ready");
    size();
    if (typeof ResizeObserver === "function") {
      var pending = false;
      new ResizeObserver(function () {
        if (pending) return;
        pending = true;
        requestAnimationFrame(function () { pending = false; size(); });
      }).observe(document.documentElement);
    }
  });
  var self = document.currentScript;
  if (self && self.parentNode) self.parentNode.removeChild(self);
})();`.replace(/\n\s*/g, ' ');
}

const wholeDocument = /^\s*(?:<!--[\s\S]*?-->\s*)*<(?:!doctype|html|head|body)[\s>]/i;

const baseStyle = `html{color-scheme:light}body{margin:0;padding:12px;font-family:system-ui,-apple-system,"Segoe UI",sans-serif;background:#fff;color:#111}svg{max-width:100%;height:auto}`;

/**
 * The complete `srcdoc` for a preview. The CSP meta, charset, base style, bootstrap, and optional
 * runtime come first, inside an open `<head>`; the source follows verbatim, so a full document
 * (`<!DOCTYPE html><html><head>…`) keeps its head elements and body, and a fragment starts the body.
 */
export function buildVisualizationDocument(source: string, kind: VisualizationKind, nonce: string, runtime?: string): string {
  if (runtime !== undefined && !inlineSafe(runtime)) throw new Error('runtime cannot be inlined');
  const csp = `<meta http-equiv="Content-Security-Policy" content="${VISUALIZATION_CSP}">`;
  const head = [
    '<!DOCTYPE html><html><head>',
    csp,
    '<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">',
    `<style>${baseStyle}</style>`,
    `<script>${bootstrap(nonce, runtime ? runtime.split('\n').length - 1 : 0)}</script>`,
    runtime ? `<script>${runtime}</script>` : '',
  ].join('');
  // A whole document (doctype, html, head, or body tag) continues the open head, so its head
  // elements stay in the head; anything else (fragments, SVG markup) starts the body.
  return head + (kind !== 'svg' && wholeDocument.test(source) ? source : `</head><body>${source}</body></html>`);
}

export type FrameMessage =
  | { type: 'ready' }
  | { type: 'error'; message: string }
  | { type: 'size'; height: number }
  | { type: 'blocked'; count: number }
  | { type: 'escape' }
  | { type: 'navigation' };

/** Validate a message from a preview frame. Anything without this frame's nonce is ignored. */
export function readFrameMessage(data: unknown, nonce: string): FrameMessage | null {
  if (!data || typeof data !== 'object') return null;
  const value = data as Record<string, unknown>;
  if (value.channel !== CHANNEL || value.nonce !== nonce || typeof value.type !== 'string') return null;
  switch (value.type) {
    case 'ready':
    case 'escape':
    case 'navigation':
      return { type: value.type };
    case 'error':
      return { type: 'error', message: typeof value.message === 'string' ? value.message.slice(0, MAX_REPORTED_TEXT) : '' };
    case 'size':
      return typeof value.height === 'number' && Number.isFinite(value.height) && value.height >= 0 ? { type: 'size', height: Math.min(value.height, 100_000) } : null;
    case 'blocked':
      return typeof value.count === 'number' && Number.isInteger(value.count) && value.count > 0 ? { type: 'blocked', count: Math.min(value.count, 10_000) } : null;
    default:
      return null;
  }
}

export const inlineHeight = (height: number) => Math.round(Math.min(MAX_INLINE_HEIGHT, Math.max(MIN_INLINE_HEIGHT, height)));
