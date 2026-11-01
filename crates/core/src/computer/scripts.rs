//! Page scripts of the managed computer, verbatim from `server/computer.ts`.
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

/// `ManagedComputer.capture`: title and up to 100 visible controls.
pub(crate) fn capture(origins: &[String]) -> String {
    let template = r#"(({ origins, viewport }) => {
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
      return { title: document.title, controls };
    })(ARGUMENTS)"#;
    expand(template).replace("ARGUMENTS", &json!({ "origins": origins, "viewport": VIEWPORT }).to_string())
}

/// `page.locator('body').innerText()`; `null` until the body exists.
pub(crate) const BODY_TEXT: &str = "document.body ? document.body.innerText : null";
