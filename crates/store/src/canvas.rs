//! Conversation canvas validation and atomic compare-and-swap edits.
//!
//! Every operation builds and capacity-checks a candidate before touching live state. Approval
//! waits belong outside the store; callers must pass the reviewed revision again when committing.

use grok_bot_model::{Canvas, CanvasInput, CanvasSource, CanvasVersion, HttpError, HttpResult, iso};

use crate::State;
use crate::validate::{Issues, is_utc_timestamp, js_len, js_trim};

pub const MAX_CANVAS_BYTES: usize = 128 * 1024;
pub const MAX_CANVAS_HISTORY: usize = 20;
pub const MAX_CANVAS_REVISION: u64 = 9_007_199_254_740_991;

const CONFLICT: &str = "The canvas changed since it was opened. Reload its latest revision before editing.";
const SECRET: &str = "Canvas text containing a protected secret is read-only. Remove the secret through its original source before editing.";

fn language_valid(language: &str) -> bool {
    (1..=32).contains(&language.len())
        && language.as_bytes()[0].is_ascii_lowercase()
        && language.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn text_valid(content: &str) -> bool {
    content.len() <= MAX_CANVAS_BYTES && !content.contains('\0')
}

fn fields_valid(title: &str, language: &str, content: &str) -> bool {
    (1..=100).contains(&js_len(title)) && js_trim(title) == title && !title.contains('\0') && language_valid(language) && text_valid(content)
}

/// Validate a complete save before approval or mutation. Revision 0 means create-if-absent.
pub fn validate_canvas_input(mut input: CanvasInput) -> HttpResult<CanvasInput> {
    let mut issues = Issues::new();
    issues.number("revision", input.revision, 0, MAX_CANVAS_REVISION);
    input.title = js_trim(&input.title).to_owned();
    issues.length("title", &input.title, 1, 100);
    if input.title.contains('\0') {
        issues.push("title", "Canvas titles cannot contain NUL bytes.");
    }
    issues.pattern("language", language_valid(&input.language), "/^[a-z][a-z0-9-]{0,31}$/");
    if input.content.contains('\0') {
        issues.push("content", "Canvas text cannot contain NUL bytes.");
    }
    issues.finish()?;
    if input.content.len() > MAX_CANVAS_BYTES {
        return Err(HttpError::too_large("Canvas text is limited to 128 KiB of UTF-8."));
    }
    Ok(input)
}

fn revision_valid(revision: u64) -> HttpResult<()> {
    let mut issues = Issues::new();
    issues.number("revision", revision, 1, MAX_CANVAS_REVISION);
    issues.finish()
}

pub(crate) fn contains_secret(keys: &[String], fields: &[&str]) -> bool {
    keys.iter().any(|key| !key.is_empty() && fields.iter().any(|field| field.contains(key.as_str())))
}

pub(crate) fn version_has_secret(version: &CanvasVersion, keys: &[String]) -> bool {
    contains_secret(keys, &[&version.title, &version.language, &version.content])
}

/// Shape, bounds, timestamps, and consecutive retained revisions for private persisted state.
pub(crate) fn persisted_canvas_valid(canvas: &Canvas) -> bool {
    let version = canvas.version();
    let valid = |version: &CanvasVersion| {
        (1..=MAX_CANVAS_REVISION).contains(&version.revision)
            && fields_valid(&version.title, &version.language, &version.content)
            && is_utc_timestamp(&version.updated_at)
            && !version.redacted
    };
    valid(&version)
        && canvas.history.len() == (canvas.revision - 1).min(MAX_CANVAS_HISTORY as u64) as usize
        && canvas.history.iter().enumerate().all(|(index, old)| valid(old) && old.revision == canvas.revision - canvas.history.len() as u64 + index as u64)
}

/// Replace one exact text anchor, including overlapping matches in ambiguity detection.
/// Returned bytes outside the anchor are unchanged (including Unicode and CRLF).
pub fn replace_range(content: &str, old_text: &str, replacement: &str) -> HttpResult<String> {
    if old_text.is_empty() {
        return Err(HttpError::bad_request("oldText must be a nonempty exact text anchor."));
    }
    if old_text.contains('\0') || replacement.contains('\0') {
        return Err(HttpError::bad_request("Canvas range text cannot contain NUL bytes."));
    }
    if old_text.len() > MAX_CANVAS_BYTES || replacement.len() > MAX_CANVAS_BYTES {
        return Err(HttpError::too_large("Canvas range text is limited to 128 KiB of UTF-8."));
    }
    let start = content.find(old_text).ok_or_else(|| HttpError::conflict("The exact canvas text was not found. Read the latest revision and try again."))?;
    // Advancing by one character rather than the whole anchor catches `aa` twice in `aaa`.
    let after_first = start + content[start..].chars().next().expect("nonempty match").len_utf8();
    if content[after_first..].contains(old_text) {
        return Err(HttpError::conflict("The canvas text anchor is ambiguous. Include more surrounding text so it occurs exactly once."));
    }
    let bytes = content.len() - old_text.len() + replacement.len();
    if bytes > MAX_CANVAS_BYTES {
        return Err(HttpError::too_large("Canvas text is limited to 128 KiB of UTF-8."));
    }
    let mut next = String::with_capacity(bytes);
    next.push_str(&content[..start]);
    next.push_str(replacement);
    next.push_str(&content[start + old_text.len()..]);
    Ok(next)
}

fn next_canvas(current: Option<&Canvas>, input: CanvasInput, source: CanvasSource, now: String) -> HttpResult<Canvas> {
    let revision = current.map_or(0, |canvas| canvas.revision);
    if revision >= MAX_CANVAS_REVISION {
        return Err(HttpError::conflict("The canvas revision limit has been reached. Export this document before creating another."));
    }
    let mut history = current.map_or_else(Vec::new, |canvas| canvas.history.clone());
    if let Some(current) = current {
        history.push(current.version());
    }
    let remove = history.len().saturating_sub(MAX_CANVAS_HISTORY);
    history.drain(..remove);
    Ok(Canvas {
        revision: revision + 1,
        title: input.title,
        language: input.language,
        content: input.content,
        updated_at: now,
        source,
        history,
        redacted: false,
    })
}

impl State {
    pub fn canvas(&self, conversation_id: &str) -> HttpResult<&Canvas> {
        self.conversation(conversation_id)?.canvas.as_ref().ok_or_else(|| HttpError::not_found("This conversation has no canvas."))
    }

    fn canvas_writable(&self, conversation_id: &str) -> HttpResult<()> {
        self.assert_writable()?;
        self.conversation(conversation_id)?;
        if self.disk().bot_deliveries.iter().any(|delivery| delivery.recipient_conversation_id == conversation_id) {
            return Err(HttpError::forbidden("Bot delivery conversations are chat-only and cannot edit a canvas."));
        }
        if let Some(canvas) = &self.conversation(conversation_id)?.canvas
            && (canvas.redacted || version_has_secret(&canvas.version(), &self.redaction_keys()))
        {
            return Err(HttpError::forbidden(SECRET));
        }
        Ok(())
    }

    fn commit_canvas(&mut self, conversation_id: &str, canvas: Canvas) -> HttpResult<Canvas> {
        let index = self
            .disk
            .conversations
            .iter()
            .position(|conversation| conversation.id == conversation_id)
            .ok_or_else(|| HttpError::not_found("Conversation does not exist."))?;
        let mut candidate = self.disk.conversations[index].clone();
        candidate.canvas = Some(canvas.clone());
        candidate.updated_at = canvas.updated_at.clone();
        {
            let mut prospect = self.prospect();
            prospect.conversations[index] = &candidate;
            Self::check_capacity(&prospect)?;
        }
        self.disk_mut().conversations[index] = candidate;
        Ok(canvas)
    }

    /// Save while a run is active, without changing the existing workspace/settings idle guard.
    pub fn save_canvas(&mut self, conversation_id: &str, input: CanvasInput, source: CanvasSource) -> HttpResult<Canvas> {
        let input = validate_canvas_input(input)?;
        self.canvas_writable(conversation_id)?;
        let current = self.conversation(conversation_id)?.canvas.as_ref();
        if current.map_or(0, |canvas| canvas.revision) != input.revision {
            return Err(HttpError::conflict(CONFLICT));
        }
        if contains_secret(&self.redaction_keys(), &[&input.title, &input.language, &input.content]) {
            return Err(HttpError::forbidden(SECRET));
        }
        if let Some(current) = current
            && current.title == input.title
            && current.language == input.language
            && current.content == input.content
        {
            return Ok(current.clone());
        }
        let next = next_canvas(current, input, source, iso(self.now()))?;
        self.commit_canvas(conversation_id, next)
    }

    /// Commit the reviewed exact text replacement only against its original document revision.
    pub fn replace_canvas_range(&mut self, conversation_id: &str, revision: u64, old_text: &str, replacement: &str) -> HttpResult<Canvas> {
        revision_valid(revision)?;
        self.canvas_writable(conversation_id)?;
        let current = self.canvas(conversation_id)?;
        if current.revision != revision {
            return Err(HttpError::conflict(CONFLICT));
        }
        if contains_secret(&self.redaction_keys(), &[old_text, replacement]) {
            return Err(HttpError::forbidden(SECRET));
        }
        let content = replace_range(&current.content, old_text, replacement)?;
        self.save_canvas(
            conversation_id,
            CanvasInput { revision, title: current.title.clone(), language: current.language.clone(), content },
            CanvasSource::Bot,
        )
    }

    /// Restoring is another revision, never a rollback of the compare-and-swap counter.
    pub fn restore_canvas(&mut self, conversation_id: &str, expected_revision: u64, target_revision: u64) -> HttpResult<Canvas> {
        revision_valid(expected_revision)?;
        revision_valid(target_revision)?;
        self.canvas_writable(conversation_id)?;
        let current = self.canvas(conversation_id)?;
        if current.revision != expected_revision {
            return Err(HttpError::conflict(CONFLICT));
        }
        let target = current
            .history
            .iter()
            .find(|version| version.revision == target_revision)
            .ok_or_else(|| HttpError::not_found("That canvas revision is no longer in the retained history."))?;
        if target.redacted || version_has_secret(target, &self.redaction_keys()) {
            return Err(HttpError::forbidden(SECRET));
        }
        let input =
            CanvasInput { revision: expected_revision, title: target.title.clone(), language: target.language.clone(), content: target.content.clone() };
        let next = next_canvas(Some(current), input, CanvasSource::Restore, iso(self.now()))?;
        self.commit_canvas(conversation_id, next)
    }
}
