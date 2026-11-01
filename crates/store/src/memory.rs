//! Per-bot memories ("메모리"): short facts a bot keeps across conversations. They are injected
//! into the engine session rules as JSON data, so the whole set is bounded.

use std::collections::HashSet;

use grok_bot_model::{Bot, HttpError, HttpResult, Memory, MemorySource, iso, new_id};
use serde::Serialize;
use sha2::Digest;

use crate::State;
use crate::validate::{Issues, is_utc_timestamp, is_uuid, js_trim};

pub const MAX_MEMORIES_PER_BOT: usize = 200;
/// UTF-16 code units, like every other text limit.
pub const MAX_MEMORY_CHARACTERS: usize = 500;
/// Serialized `[{id, text}]` the engine receives in its rules.
pub const MAX_MEMORY_SET_BYTES: usize = 16 * 1024;
pub const MAX_MEMORIES_PER_REQUEST: usize = MAX_MEMORIES_PER_BOT;
pub const MAX_MEMORY_IMPORT_BYTES: usize = 64 * 1024;

const TEXT_ERROR: &str = "Memories must be nonempty text without control characters other than tabs and line breaks.";
const FULL_ERROR: &str = "A bot can remember at most 200 items. Forget some memories before adding more.";
const SET_SIZE_ERROR: &str = "Bot memories exceed the 16 KiB limit the engine receives. Forget or shorten memories before adding more.";

fn invalid_character(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}')
}

/// Normalize one memory text (trim, `\r\n` to `\n`) and append its issues under `path`.
fn memory_issues(issues: &mut Issues, path: &str, text: &str) -> String {
    let text = js_trim(&text.replace("\r\n", "\n")).to_owned();
    issues.length(path, &text, 1, MAX_MEMORY_CHARACTERS);
    if text.chars().any(invalid_character) {
        issues.push(path, TEXT_ERROR);
    }
    text
}

/// Validate and normalize one memory text.
pub fn normalize_memory(text: &str) -> HttpResult<String> {
    let mut issues = Issues::new();
    let text = memory_issues(&mut issues, "text", text);
    issues.finish()?;
    Ok(text)
}

/// Case-insensitive identity used to skip duplicate memories.
fn memory_key(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// The memory that already holds `text`, compared like [`State::add_memories`] does.
pub fn find_memory<'a>(memories: &'a [Memory], text: &str) -> Option<&'a Memory> {
    let key = memory_key(text);
    memories.iter().find(|memory| memory_key(&memory.text) == key)
}

#[derive(Serialize)]
struct Entry<'a> {
    id: &'a str,
    text: &'a str,
}

/// Serialized size of the memory data placed in the session rules.
pub fn memory_set_bytes(memories: &[Memory]) -> usize {
    let entries: Vec<Entry<'_>> = memories.iter().map(|memory| Entry { id: &memory.id, text: &memory.text }).collect();
    serde_json::to_vec(&entries).map_or(usize::MAX, |bytes| bytes.len())
}

fn validate_memory_set(memories: &[Memory]) -> HttpResult<()> {
    if memories.len() > MAX_MEMORIES_PER_BOT {
        return Err(HttpError::conflict(FULL_ERROR));
    }
    if memory_set_bytes(memories) > MAX_MEMORY_SET_BYTES {
        return Err(HttpError::too_large(SET_SIZE_ERROR));
    }
    if memories.iter().map(|memory| memory.id.as_str()).collect::<HashSet<_>>().len() != memories.len() {
        return Err(HttpError::bad_request("Memory IDs must be unique."));
    }
    Ok(())
}

/// The engine session fingerprint input for a bot: its skill fingerprint, extended with its
/// memories and knowledge index when it has any. Bots without either keep the plain skill
/// fingerprint, so their existing engine sessions stay valid.
pub fn bot_context_fingerprint(skill_fingerprint: &str, bot: &Bot) -> String {
    let memories = bot.memories.as_deref().unwrap_or_default();
    let knowledge = bot.knowledge.as_deref().unwrap_or_default();
    if memories.is_empty() && knowledge.is_empty() {
        return skill_fingerprint.to_owned();
    }
    let memories: Vec<_> = memories.iter().map(|memory| [&memory.id, &memory.text]).collect();
    let knowledge: Vec<_> = knowledge.iter().map(|file| [&file.id, &file.name, &file.hash]).collect();
    let input = serde_json::json!(["bot-context-v1", skill_fingerprint, memories, knowledge]);
    hex::encode(sha2::Sha256::digest(input.to_string().as_bytes()))
}

/// Persisted memory and knowledge metadata of one bot (`diskSchema` counterpart).
pub(crate) fn persisted_bot_data_valid(bot: &mut Bot) -> bool {
    let memories_valid = bot.memories.as_mut().is_none_or(|memories| {
        memories.iter_mut().all(|memory| {
            let mut issues = Issues::new();
            memory.text = memory_issues(&mut issues, "text", &memory.text);
            issues.is_empty() && is_uuid(&memory.id) && is_utc_timestamp(&memory.created_at)
        }) && validate_memory_set(memories).is_ok()
    });
    memories_valid && crate::knowledge::persisted_knowledge_valid(bot.knowledge.as_deref().unwrap_or_default())
}

/// Result of adding memories: the new entries and the requested texts that were already known.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoriesAdded {
    pub added: Vec<Memory>,
    pub existing: Vec<Memory>,
}

/// One reviewable item of a pasted memory import.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryPreviewItem {
    pub text: String,
    /// Already remembered by the bot or repeated earlier in the import.
    pub duplicate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Strip a list marker (`-`, `*`, `•`, `1.`, `2)`, `[ ]`) from the start of a trimmed line.
fn strip_marker(line: &str) -> (&str, bool) {
    let mut rest = line;
    let mut marked = false;
    for marker in ["- ", "* ", "+ ", "• ", "· ", "– ", "— "] {
        if let Some(after) = rest.strip_prefix(marker) {
            rest = after.trim_start();
            marked = true;
            break;
        }
    }
    if !marked {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if (1..=4).contains(&digits)
            && let Some(after) = rest[digits..].strip_prefix(". ").or_else(|| rest[digits..].strip_prefix(") "))
        {
            rest = after.trim_start();
            marked = true;
        }
    }
    for checkbox in ["[ ] ", "[x] ", "[X] "] {
        if let Some(after) = rest.strip_prefix(checkbox) {
            rest = after.trim_start();
            marked = true;
        }
    }
    (rest, marked)
}

/// Texts of a JSON array export: strings, or objects with a `text`/`content`/`memory`/`value`
/// string field. `None` when `text` is not such an array.
fn json_items(text: &str) -> Option<Vec<String>> {
    let serde_json::Value::Array(values) = serde_json::from_str(text).ok()? else { return None };
    let mut items = Vec::new();
    for value in values {
        match value {
            serde_json::Value::String(text) => items.push(text),
            serde_json::Value::Object(object) => {
                let text = ["text", "content", "memory", "value"].iter().find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))?;
                items.push(text.to_owned());
            }
            _ => return None,
        }
    }
    Some(items)
}

/// Split pasted text (from another assistant's memory export) into memory candidates: one per
/// list item or line. Headings, rules, and code fences are skipped; indented lines continue the
/// previous item. A JSON array of strings or `{text}` objects is read item by item.
pub fn split_memory_import(text: &str) -> HttpResult<Vec<String>> {
    if text.len() > MAX_MEMORY_IMPORT_BYTES {
        return Err(HttpError::too_large("Paste at most 64 KiB of memory text at once."));
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text).replace("\r\n", "\n");
    if let Some(items) = json_items(js_trim(&text)) {
        return Ok(items.iter().map(|item| js_trim(item).to_owned()).filter(|item| !item.is_empty()).collect());
    }
    let mut items: Vec<String> = Vec::new();
    let mut continuing = false;
    for line in text.split('\n') {
        let trimmed = js_trim(line);
        if trimmed.is_empty() {
            continuing = false;
            continue;
        }
        if trimmed.starts_with("```") || trimmed.starts_with('#') || trimmed.chars().all(|c| matches!(c, '-' | '=' | '*' | '_')) {
            continuing = false;
            continue;
        }
        let indented = line.starts_with([' ', '\t']);
        let (body, marked) = strip_marker(trimmed);
        if body.is_empty() {
            continue;
        }
        match items.last_mut() {
            Some(last) if continuing && indented && !marked => {
                last.push(' ');
                last.push_str(body);
            }
            _ => items.push(body.to_owned()),
        }
        continuing = true;
    }
    Ok(items)
}

/// `POST /api/bots/:botId/memories/preview`: split pasted text and mark duplicates and invalid
/// items so the user can review the list before anything is saved.
pub fn preview_memory_import(text: &str, existing: &[Memory]) -> HttpResult<Vec<MemoryPreviewItem>> {
    let items = split_memory_import(text)?;
    if items.len() > MAX_MEMORIES_PER_BOT {
        return Err(HttpError::too_large("Paste at most 200 memory items at once."));
    }
    let mut seen: HashSet<String> = existing.iter().map(|memory| memory_key(&memory.text)).collect();
    Ok(items
        .into_iter()
        .map(|item| {
            let mut issues = Issues::new();
            let text = memory_issues(&mut issues, "text", &item);
            let error = issues.finish().err().map(|error| error.message);
            let duplicate = !seen.insert(memory_key(&text));
            MemoryPreviewItem { text, duplicate, error }
        })
        .collect())
}

impl State {
    pub fn memories(&self, bot_id: &str) -> HttpResult<&[Memory]> {
        Ok(self.bot(bot_id)?.memories.as_deref().unwrap_or_default())
    }

    /// Add memories to a bot, skipping texts it already remembers (case and spacing insensitive).
    /// `within_run` admits a model-initiated save the user approved during an active run.
    /// Nothing is written when every text is a duplicate.
    pub fn add_memories(&mut self, bot_id: &str, texts: Vec<String>, source: MemorySource, within_run: bool) -> HttpResult<MemoriesAdded> {
        let mut issues = Issues::new();
        if texts.is_empty() || texts.len() > MAX_MEMORIES_PER_REQUEST {
            issues.push("texts", format!("Provide between 1 and {MAX_MEMORIES_PER_REQUEST} memories."));
        }
        let texts: Vec<String> = texts.iter().enumerate().map(|(index, text)| memory_issues(&mut issues, &format!("texts.{index}"), text)).collect();
        issues.finish()?;
        if within_run {
            self.assert_writable()?;
        } else {
            self.assert_idle()?;
        }
        let index = self.bot_index(bot_id)?;
        let mut memories = self.disk.bots[index].memories.clone().unwrap_or_default();
        let now = iso(self.now());
        let (mut added, mut existing) = (Vec::new(), Vec::new());
        for text in texts {
            let key = memory_key(&text);
            if let Some(known) = memories.iter().find(|memory| memory_key(&memory.text) == key) {
                if !existing.contains(known) {
                    existing.push(known.clone());
                }
                continue;
            }
            let memory = Memory { id: new_id(), text, source, created_at: now.clone() };
            memories.push(memory.clone());
            added.push(memory);
        }
        if added.is_empty() {
            return Ok(MemoriesAdded { added, existing });
        }
        validate_memory_set(&memories)?;
        let mut candidate = self.disk.bots[index].clone();
        candidate.memories = Some(memories);
        {
            let mut prospect = self.prospect();
            prospect.bots[index] = &candidate;
            Self::check_capacity(&prospect)?;
        }
        self.disk_mut().bots[index] = candidate;
        Ok(MemoriesAdded { added, existing })
    }

    /// Forget one memory. `within_run` admits an approved model request during an active run.
    pub fn delete_memory(&mut self, bot_id: &str, id: &str, within_run: bool) -> HttpResult<Memory> {
        if within_run {
            self.assert_writable()?;
        } else {
            self.assert_idle()?;
        }
        let index = self.bot_index(bot_id)?;
        let memory = self.memories(bot_id)?.iter().find(|memory| memory.id == id).cloned().ok_or_else(|| HttpError::not_found("Memory does not exist for this bot."))?;
        let bot = &mut self.disk_mut().bots[index];
        if let Some(memories) = bot.memories.as_mut() {
            memories.retain(|memory| memory.id != id);
            if memories.is_empty() {
                bot.memories = None;
            }
        }
        Ok(memory)
    }
}

/// Fresh memories for a bot created from a template (duplicates dropped, set limits enforced).
pub(crate) fn template_memories(texts: Vec<String>, now: &str) -> HttpResult<Vec<Memory>> {
    let mut issues = Issues::new();
    let mut memories: Vec<Memory> = Vec::new();
    let mut seen = HashSet::new();
    for (index, text) in texts.iter().enumerate() {
        let text = memory_issues(&mut issues, &format!("memories.{index}.text"), text);
        if seen.insert(memory_key(&text)) {
            memories.push(Memory { id: new_id(), text, source: MemorySource::Import, created_at: now.to_owned() });
        }
    }
    issues.finish()?;
    validate_memory_set(&memories)?;
    Ok(memories)
}
