//! Page scripts of the managed computer, verbatim from `server/computer.ts` except the page-text
//! walk of [`capture`], which is new in this port.
//!
//! The init script runs in the page's main world before any page script and disables network and
//! popup APIs and sensitive input events. The observation scripts run in a fresh isolated world
//! (`Page.createIsolatedWorld`), so page scripts cannot replace the DOM functions they rely on.

use serde_json::{Value, json};

use super::VIEWPORT;

const SENSITIVE: &str = r#"/pass|secret|token|credential|login|username|one.?time|credit|card|cvv|cvc|ssn|api.?key|^pin$/i"#;
const SENSITIVE_FIELD: &str = r#"'iframe,input[type="password"],input[type="file"],input[type="email"],input[type="tel"]'"#;

fn expand(template: &str) -> String {
    template.replace("SENSITIVE_FIELD", SENSITIVE_FIELD).replace("SENSITIVE", SENSITIVE)
}

/// `context.addInitScript` of the Node implementation.
pub(crate) fn init(origins: &[String]) -> String {
    let template = r#"(() => {
      const origins = ORIGINS;
      const denied = () => { throw new Error('This API is disabled in the managed computer.'); };
      for (const name of ['WebSocket', 'WebTransport', 'RTCPeerConnection', 'webkitRTCPeerConnection', 'RTCIceGatherer', 'RTCIceTransport']) {
        Object.defineProperty(window, name, { value: denied, writable: false, configurable: false });
      }
      Object.defineProperty(window, 'open', { value: () => null, writable: false, configurable: false });
      Object.defineProperty(navigator, 'sendBeacon', { value: () => false, writable: false, configurable: false });
      Object.defineProperty(navigator, 'serviceWorker', { value: undefined, writable: false, configurable: false });
      const sensitive = element => {
        const field = element.closest('input,textarea,[contenteditable],iframe,form');
        if (!field) return false;
        return field.matches(SENSITIVE_FIELD)
          || SENSITIVE.test(['name', 'id', 'autocomplete'].map(key => field.getAttribute(key) ?? '').join(' '))
          || !!field.closest('form')?.querySelector('input[type="password"],input[type="file"]');
      };
      for (const name of ['click', 'auxclick', 'beforeinput', 'paste', 'drop', 'dragover']) {
        window.addEventListener(name, event => {
          const element = event.composedPath().find(item => item instanceof Element);
          if (!element) return;
          const anchor = element.closest('a');
          let blocked = sensitive(element) || name === 'drop' || name === 'dragover' || name === 'paste';
          if (anchor) {
            try { const url = new URL(anchor.href); blocked ||= !origins.includes(url.origin) || !['http:', 'https:'].includes(url.protocol) || !!url.username || !!url.password || anchor.hasAttribute('download') || !!anchor.target && anchor.target !== '_self'; }
            catch { blocked = true; }
          }
          if (blocked) { event.preventDefault(); event.stopImmediatePropagation(); }
        }, true);
      }
      window.addEventListener('submit', event => { event.preventDefault(); event.stopImmediatePropagation(); }, true);
    })();"#;
    expand(template).replace("ORIGINS", &json!(origins).to_string())
}

/// `ManagedComputer.target`: whether the control at a point (or the focused element) accepts text
/// and whether it is blocked.
pub(crate) fn target(point: Option<(f64, f64)>, origins: &[String]) -> String {
    let template = r#"(({ point, origins }) => {
      const element = point ? document.elementFromPoint(point.x, point.y) : document.activeElement;
      if (!element) return { editable: false, blocked: true };
      const field = element.closest('input,textarea,[contenteditable],iframe');
      const blocked = !!field && (field.matches(SENSITIVE_FIELD)
        || SENSITIVE.test(['name', 'id', 'autocomplete'].map(key => field.getAttribute(key) ?? '').join(' '))
        || !!field.closest('form')?.querySelector('input[type="password"],input[type="file"]'));
      const anchor = element.closest('a');
      let blockedLink = false;
      if (anchor) {
        try { const url = new URL(anchor.href); blockedLink = !origins.includes(url.origin) || !['http:', 'https:'].includes(url.protocol) || !!url.username || !!url.password || anchor.hasAttribute('download') || !!anchor.target && anchor.target !== '_self'; }
        catch { blockedLink = true; }
      }
      const editable = !blocked && (field instanceof HTMLTextAreaElement && !field.readOnly && !field.disabled
        || field instanceof HTMLInputElement && ['text', 'search', 'url', 'number'].includes(field.type) && !field.readOnly && !field.disabled
        || element instanceof HTMLElement && element.isContentEditable);
      return { editable: !!editable, blocked: blocked || blockedLink };
    })(ARGUMENTS)"#;
    let point: Value = point.map(|(x, y)| json!({ "x": x, "y": y })).unwrap_or(Value::Null);
    expand(template).replace("ARGUMENTS", &json!({ "point": point, "origins": origins }).to_string())
}

/// `ManagedComputer.capture`: title, up to 100 visible controls, and the visible rendered text of
/// the top-level document.
///
/// The text walk is innerText-like but deliberately narrower: it never reads form state (`input`,
/// `textarea`, `select`, `option`, `output`, and contenteditable subtrees are skipped whole, so
/// typed or prefilled values are never collected), skips `script`/`style`/`noscript`/`template`,
/// embedded frames and media, and skips what is not rendered: elements with `display: none`,
/// `opacity: 0`, or `content-visibility: hidden` (or that `checkVisibility` rejects), the body of a
/// closed `<details>`, text that is `visibility: hidden`, and text nodes without layout boxes
/// (for example light-DOM children a shadow root does not slot). Open shadow roots are walked in
/// flat-tree order through their slots. Whitespace collapses as CSS renders it; block boundaries
/// become line breaks and paragraphs and headings become blank lines. Collection stops after
/// `textLimit` UTF-16 units or `nodeLimit` nodes and reports `textTruncated`; the caller enforces
/// the final bound.
pub(crate) fn capture(origins: &[String], text_limit: usize, node_limit: usize) -> String {
    let template = r#"(({ origins, viewport, textLimit, nodeLimit }) => {
      const skipped = new Set(['head', 'script', 'style', 'noscript', 'template', 'iframe', 'frame', 'frameset', 'object', 'embed', 'input', 'textarea', 'select', 'option', 'optgroup', 'datalist', 'output', 'canvas', 'video', 'audio', 'picture', 'img', 'math']);
      let text = '', breaks = 0, nodes = 0, textTruncated = false;
      const range = document.createRange();
      const block = count => { breaks = Math.max(breaks, count); };
      const push = (value, preserve) => {
        if (!preserve) value = value.replace(/\s+/g, ' ');
        if (!value) return;
        if (!preserve && value === ' ') {
          if (!breaks && text && !/\s$/.test(text)) text += ' ';
          return;
        }
        if (breaks) {
          text = text.replace(/[ \t]+$/, '');
          if (text) text += breaks > 1 ? '\n\n' : '\n';
          breaks = 0;
        }
        if (!preserve && (!text || /\s$/.test(text))) value = value.replace(/^ /, '');
        text += value;
        if (text.length > textLimit) { text = text.slice(0, textLimit); textTruncated = true; }
      };
      const walk = element => {
        if (textTruncated) return;
        if (++nodes > nodeLimit) { textTruncated = true; return; }
        const name = element.localName;
        if (skipped.has(name) || element instanceof HTMLElement && element.isContentEditable) return;
        if (name === 'br') { breaks = Math.min(2, breaks + 1); return; }
        const style = getComputedStyle(element);
        const display = style.display;
        if (display === 'none' || style.opacity === '0' || style.contentVisibility === 'hidden') return;
        if (display !== 'contents' && typeof element.checkVisibility === 'function' && !element.checkVisibility({ opacityProperty: true })) return;
        const visible = style.visibility === 'visible';
        const collapse = style.whiteSpaceCollapse || (/^pre|^break-spaces/.test(style.whiteSpace) ? 'preserve' : 'collapse');
        const preserve = collapse !== 'collapse';
        const lines = name === 'p' || /^h[1-6]$/.test(name) ? 2 : display.startsWith('inline') || display === 'contents' || display === 'table-cell' ? 0 : 1;
        if (display === 'table-cell') push(' ', false);
        if (lines) block(lines);
        const assigned = name === 'slot' ? element.assignedNodes() : [];
        const children = assigned.length ? assigned : element.shadowRoot ? element.shadowRoot.childNodes : element.childNodes;
        for (const child of children) {
          if (textTruncated) return;
          if (name === 'details' && !element.open && !(child instanceof Element && child.localName === 'summary')) continue;
          if (child.nodeType === Node.TEXT_NODE) {
            if (!visible) continue;
            if (++nodes > nodeLimit) { textTruncated = true; return; }
            if (/\S/.test(child.data)) {
              range.selectNodeContents(child);
              if (!range.getClientRects().length) continue;
            }
            push(preserve ? child.data.replace(/\r\n?/g, '\n') : child.data, preserve);
          } else if (child.nodeType === Node.ELEMENT_NODE) walk(child);
        }
        if (lines) block(lines);
        if (display === 'table-cell') push(' ', false);
      };
      if (document.body) walk(document.body);
      text = text.replace(/[ \t]+\n/g, '\n').replace(/\n{3,}/g, '\n\n').trim();
      if (typeof text.toWellFormed === 'function') text = text.toWellFormed();
      const controls = [];
      for (const element of document.querySelectorAll('a,button,input,textarea,select,[role="button"],[role="link"],[contenteditable="true"],iframe')) {
        const bounds = element.getBoundingClientRect();
        const style = getComputedStyle(element);
        if (bounds.width <= 0 || bounds.height <= 0 || bounds.bottom <= 0 || bounds.right <= 0 || bounds.x >= viewport.width || bounds.y >= viewport.height || style.visibility !== 'visible' || style.display === 'none' || style.opacity === '0') continue;
        const field = element;
        const sensitive = element.matches(SENSITIVE_FIELD)
          || SENSITIVE.test(['name', 'id', 'autocomplete'].map(key => element.getAttribute(key) ?? '').join(' '))
          || !!element.closest('form')?.querySelector('input[type="password"],input[type="file"]');
        let blocked = sensitive;
        if (element instanceof HTMLAnchorElement) {
          try { const url = new URL(element.href); blocked ||= !origins.includes(url.origin) || !['http:', 'https:'].includes(url.protocol) || !!url.username || !!url.password || element.hasAttribute('download') || !!element.target && element.target !== '_self'; }
          catch { blocked = true; }
        }
        const editable = !sensitive && (element instanceof HTMLTextAreaElement || element instanceof HTMLInputElement && ['text', 'search', 'url', 'number'].includes(element.type) || element instanceof HTMLElement && element.isContentEditable) && !field.readOnly && !field.disabled;
        const role = element.matches('a,[role="link"]') ? 'link'
          : element.matches('button,[role="button"],input[type="button"],input[type="submit"]') ? 'button'
          : element.matches('input[type="checkbox"]') ? 'checkbox'
          : element.matches('input[type="radio"]') ? 'radio'
          : element.matches('select') ? 'select' : editable ? 'textbox' : 'control';
        const labels = element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement || element instanceof HTMLSelectElement ? [...element.labels ?? []].map(label => label.textContent ?? '').join(' ') : '';
        const label = element.getAttribute('aria-label') || labels || element.getAttribute('placeholder') || (element.matches('input,textarea,select') ? '' : element.textContent ?? '');
        controls.push({
          id: String(controls.length + 1), role, label: label.replace(/[\u0000-\u001f\u007f]/g, ' ').slice(0, 160),
          bounds: { x: Math.max(0, bounds.x), y: Math.max(0, bounds.y), width: Math.min(bounds.right, viewport.width) - Math.max(0, bounds.x), height: Math.min(bounds.bottom, viewport.height) - Math.max(0, bounds.y) },
          disabled: !!field.disabled, editable, blocked,
        });
        if (controls.length === 100) break;
      }
      return { title: document.title, controls, text, textTruncated };
    })(ARGUMENTS)"#;
    expand(template).replace("ARGUMENTS", &json!({ "origins": origins, "viewport": VIEWPORT, "textLimit": text_limit, "nodeLimit": node_limit }).to_string())
}

/// `page.locator('body').innerText()`; `null` until the body exists.
pub(crate) const BODY_TEXT: &str = "document.body ? document.body.innerText : null";
