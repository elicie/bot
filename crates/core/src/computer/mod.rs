//! Managed computer: an app-owned private Chrome the user starts locally and the model may observe
//! or act on with one-use local approval. Port of `shared/computer.ts`, `server/computer.ts`,
//! `server/computer-network.ts`, and `server/computer-api.ts`.
//!
//! - [`network`]: public-origin allowlist, DNS validation of every answer, address pinning.
//! - [`proxy`]: the authenticated loopback forward proxy Chrome must use for every request.
//! - [`cdp`]: Chrome launch and a Chrome DevTools Protocol client over `--remote-debugging-pipe`
//!   (no debugging port is opened).
//! - [`managed`]: `ManagedComputer`, the browser session state machine.
//! - [`application`]: `ApplicationComputer`, per-bot ownership, PNG frame cache, cancellation.

use futures::future::BoxFuture;
use grok_bot_model::HttpResult;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod application;
mod cdp;
pub mod managed;
pub mod network;
pub mod proxy;
mod scripts;
#[cfg(test)]
mod tests;

pub use application::ApplicationComputer;
pub use managed::{ComputerOptions, ManagedComputer, TestBrowser};

pub const VIEWPORT_WIDTH: u32 = 960;
pub const VIEWPORT_HEIGHT: u32 = 600;

pub const COMPUTER_KEYS: [&str; 15] = [
    "Enter", "Tab", "Shift+Tab", "Escape", "Backspace", "Delete", "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Home", "End", "PageUp", "PageDown",
    "Space",
];

/// Browser test overrides (loopback fixture origins, another Chrome executable, no sandbox) are
/// honoured only by unit tests and by debug builds started with `NODE_ENV=test`. Release builds
/// never enable them.
pub fn test_mode() -> bool {
    cfg!(test) || (cfg!(debug_assertions) && std::env::var("NODE_ENV").is_ok_and(|value| value == "test"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

pub const VIEWPORT: Viewport = Viewport { width: VIEWPORT_WIDTH, height: VIEWPORT_HEIGHT };

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComputerState {
    Stopped,
    Starting,
    Running,
    Stopping,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputerStatus {
    pub state: ComputerState,
    pub epoch: String,
    pub frame_id: Option<String>,
    pub viewport: Viewport,
    pub origins: Vec<String>,
    pub url: Option<String>,
    pub busy: bool,
    pub sandboxed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComputerControl {
    pub id: String,
    pub role: String,
    pub label: String,
    pub bounds: Bounds,
    pub disabled: bool,
    pub editable: bool,
    pub blocked: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputerFrame {
    pub id: String,
    pub epoch: String,
    pub url: String,
    pub title: String,
    pub viewport: Viewport,
    pub controls: Vec<ComputerControl>,
    pub mime_type: String,
    /// Visible rendered text of the top-level document, at most [`PAGE_TEXT_LIMIT`] characters.
    /// Untrusted page content; never contains form field values.
    #[serde(default)]
    pub page_text: String,
    /// Whether `page_text` was cut at the limit.
    #[serde(default)]
    pub text_truncated: bool,
}

/// Characters of visible page text an observation carries.
pub const PAGE_TEXT_LIMIT: usize = 6000;

/// Elements the page-text walk visits before it stops and reports truncation.
pub(crate) const PAGE_TEXT_NODE_LIMIT: usize = 20_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComputerObservation {
    pub status: ComputerStatus,
    pub frame: ComputerFrame,
}

/// An observation with its PNG screenshot. The PNG stays on this computer: it is served only to
/// loopback clients and never sent to the model provider.
#[derive(Clone, Debug)]
pub struct ComputerScreenshot {
    pub status: ComputerStatus,
    pub frame: ComputerFrame,
    pub png: Vec<u8>,
}

/// The browser service behind [`ApplicationComputer`]. Inputs are untrusted JSON validated by the
/// service (`ManagedComputer` parses them with the Node schemas); tests substitute a fake.
pub trait ComputerService: Send + Sync {
    fn status(&self) -> ComputerStatus;
    fn start(&self, input: Value) -> BoxFuture<'_, HttpResult<ComputerStatus>>;
    fn stop(&self) -> BoxFuture<'_, ComputerStatus>;
    fn perform(&self, input: Value) -> BoxFuture<'_, HttpResult<ComputerObservation>>;
    fn screenshot(&self, input: Value) -> BoxFuture<'_, HttpResult<ComputerScreenshot>>;
}

/// JavaScript `String.prototype.length` (UTF-16 code units).
pub(crate) fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

/// `value.replace(/[\u0000-\u001f\u007f]/g, ' ').slice(0, limit)` without splitting a character.
pub(crate) fn safe_label(value: &str, limit: usize) -> String {
    let mut out = String::new();
    let mut units = 0;
    for c in value.chars() {
        let c = if (c as u32) < 0x20 || c == '\u{7f}' { ' ' } else { c };
        units += c.len_utf16();
        if units > limit {
            break;
        }
        out.push(c);
    }
    out
}

/// Page text bounded to `limit` characters (never splitting one), with control characters other
/// than line breaks and tabs replaced by spaces. Returns whether anything was cut.
pub(crate) fn bounded_text(value: &str, limit: usize) -> (String, bool) {
    let mut out = String::new();
    for (count, c) in value.chars().enumerate() {
        if count == limit {
            return (out.trim_end().to_owned(), true);
        }
        out.push(if c != '\n' && c != '\t' && ((c as u32) < 0x20 || c == '\u{7f}') { ' ' } else { c });
    }
    (out, false)
}

/// `z.string().uuid()`: canonical hyphenated form.
pub(crate) fn is_uuid(value: &str) -> bool {
    value.len() == 36 && value.as_bytes().iter().enumerate().all(|(i, b)| if [8, 13, 18, 23].contains(&i) { *b == b'-' } else { b.is_ascii_hexdigit() }) && uuid::Uuid::parse_str(value).is_ok()
}
