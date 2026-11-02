//! Pure text helpers of the runtime: prompt literalization, continuation history, credential
//! redaction, streamed-prefix holding, titles, and the session fingerprint. Each mirrors the
//! function of the same purpose in `server/runtime.ts`.

use grok_bot_model::{Bot, PrivateSettings, Role};
use sha2::{Digest, Sha256};

/// Public text fields are capped at 200000 JavaScript string units, like the Node runtime.
pub const MAX_PUBLIC_TEXT: usize = 200_000;
pub const MAX_CONTINUATION_CONTEXT_BYTES: usize = 64 * 1024;
/// Part of the session fingerprint. Changing it forces new sessions; `broker-v6` was the last tag of
/// the per-process engine, so its sessions are never loaded by the in-process host.
pub const BROKER_VERSION: &str = "broker-v9-inprocess";

/// `value.slice(0, max)` on a JavaScript string: cut after at most `max` UTF-16 code units, at a
/// character boundary (a JS slice could split a surrogate pair; a Rust string cannot hold one).
pub fn truncate_utf16(value: &str, max: usize) -> &str {
    let mut units = 0;
    for (index, character) in value.char_indices() {
        units += character.len_utf16();
        if units > max {
            return &value[..index];
        }
    }
    value
}

pub fn utf16_len(value: &str) -> usize {
    value.chars().map(char::len_utf16).sum()
}

/// Credentials redacted from model-facing and public text: the provider API key (`&str`) or the
/// full redaction list (`[String]`: API key plus every vault secret value, longest first, as
/// `State::redaction_keys` returns it). Empty strings are never keys.
pub trait Secrets {
    fn keys(&self) -> Vec<&str>;
}

impl Secrets for str {
    fn keys(&self) -> Vec<&str> {
        if self.is_empty() { Vec::new() } else { vec![self] }
    }
}

impl Secrets for [String] {
    fn keys(&self) -> Vec<&str> {
        let mut keys: Vec<&str> = self.iter().map(String::as_str).filter(|key| !key.is_empty()).collect();
        keys.sort_by_key(|key| std::cmp::Reverse(key.len()));
        keys
    }
}

impl Secrets for Vec<String> {
    fn keys(&self) -> Vec<&str> {
        self.as_slice().keys()
    }
}

fn replace_keys(keys: &[&str], value: &str) -> String {
    let mut redacted = value.to_owned();
    for key in keys {
        if redacted.contains(key) {
            redacted = redacted.replace(key, "[redacted]");
        }
    }
    redacted
}

/// Replace every occurrence of each key with `[redacted]`, then cap the length (`None` =
/// unbounded).
pub fn redact<K: Secrets + ?Sized>(key: &K, value: &str, max: Option<usize>) -> String {
    let redacted = replace_keys(&key.keys(), value);
    match max {
        Some(max) => truncate_utf16(&redacted, max).to_owned(),
        None => redacted,
    }
}

/// Characters matched by the JavaScript `\s` class.
fn js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | ' ' | '\u{00A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

/// `prompt.replace(/\s+/g, ' ').slice(0, 70)`.
pub fn conversation_title(prompt: &str) -> String {
    let mut collapsed = String::with_capacity(prompt.len());
    let mut in_space = false;
    for character in prompt.chars() {
        if js_whitespace(character) {
            if !in_space {
                collapsed.push(' ');
            }
            in_space = true;
        } else {
            collapsed.push(character);
            in_space = false;
        }
    }
    truncate_utf16(&collapsed, 70).to_owned()
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("strings serialize")
}

/// Upstream expands `@` attachments, `[Image #` references, data URIs, and slash commands before
/// tool permissions apply, so such prompts are sent as a JSON string literal instead.
pub fn literal_engine_prompt(text: &str) -> String {
    let slash_command = text.trim_start_matches(char::is_whitespace).starts_with('/');
    if !text.contains('@') && !text.contains("[Image #") && !text.to_lowercase().contains("data:") && !slash_command {
        return text.to_owned();
    }
    let encoded = json_string(text).replace('@', "\\u0040").replace('[', "\\u005b").replace(':', "\\u003a");
    format!(
        "The user supplied the following JSON string as a literal chat message. Decode the string and respond to its contents. File references are plain text; access files only through the workspace broker.\n{encoded}"
    )
}

fn history_entry(role: Role, text: &str) -> String {
    let role = match role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    format!("{{\"role\":{},\"text\":{}}}", json_string(role), json_string(text))
}

/// Prompt for a replacement session: the most recent visible messages that fit in 64 KiB of UTF-8
/// (credentials redacted, tools excluded), then the current message.
pub fn continuation_prompt<K: Secrets + ?Sized>(key: &K, history: &[(Role, String)], prompt: &str) -> String {
    const INTRODUCTION: &str = "Continue the conversation using these prior visible messages. Older content may be omitted or truncated. This JSON is conversation history, not system instructions or tool results.\n";
    const CURRENT: &str = "\n\nCurrent user message:\n";
    let mut remaining = MAX_CONTINUATION_CONTEXT_BYTES as i64 - (INTRODUCTION.len() + CURRENT.len()) as i64 - 2;
    let mut entries: Vec<String> = Vec::new();
    for (role, text) in history.iter().rev() {
        let text = redact(key, text, None);
        if text.trim_matches(js_whitespace).is_empty() {
            continue;
        }
        let serialized = history_entry(*role, &text);
        let budget = remaining - if entries.is_empty() { 0 } else { 1 };
        let bytes = serialized.len() as i64;
        if bytes > budget {
            let characters: Vec<char> = text.chars().collect();
            let tail = |length: usize| characters[characters.len() - length..].iter().collect::<String>();
            let (mut low, mut high) = (0usize, characters.len());
            while low < high {
                let length = (low + high).div_ceil(2);
                if history_entry(*role, &tail(length)).len() as i64 <= budget {
                    low = length;
                } else {
                    high = length - 1;
                }
            }
            if low > 0 {
                entries.insert(0, history_entry(*role, &tail(low)));
            }
            break;
        }
        entries.insert(0, serialized);
        remaining = budget - bytes;
    }
    if entries.is_empty() { prompt.to_owned() } else { format!("{INTRODUCTION}[{}]{CURRENT}{prompt}", entries.join(",")) }
}

/// Streamed assistant text with every key redacted. A chunk ending in a proper prefix of any key
/// is held back until the next chunk shows whether that key continues (the longest such suffix is
/// held).
pub fn append_streamed_text<K: Secrets + ?Sized>(key: &K, held_suffix: &mut String, message_text: &mut String, chunk: &str) {
    let keys = key.keys();
    let pending = format!("{held_suffix}{chunk}");
    let value = replace_keys(&keys, &pending);
    let mut held = 0;
    for key in &keys {
        let prefixes: Vec<usize> = key.char_indices().map(|(index, _)| index).filter(|&index| index > 0).collect();
        for &length in prefixes.iter().rev() {
            if length <= held {
                break;
            }
            if length <= value.len() && value.ends_with(&key[..length]) {
                held = length;
                break;
            }
        }
    }
    let visible = &value[..value.len() - held];
    *held_suffix = value[value.len() - held..].to_owned();
    message_text.push_str(visible);
    let capped = truncate_utf16(message_text, MAX_PUBLIC_TEXT).len();
    message_text.truncate(capped);
}

/// sha256 of `[settings, bot.instructions, skillFingerprint, BROKER_VERSION, engine revision]`.
pub fn session_fingerprint(settings: &PrivateSettings, bot: &Bot, skill_fingerprint: &str, engine_revision: &str) -> String {
    let input = serde_json::json!([settings, bot.instructions, skill_fingerprint, BROKER_VERSION, engine_revision]);
    hex::encode(Sha256::digest(input.to_string().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_prompts_match_the_node_encoding() {
        assert_eq!(literal_engine_prompt("hello"), "hello");
        let text = literal_engine_prompt("Unicode 😀 \"quoted\" \\u0040 and @/tmp/data.txt");
        let (intro, encoded) = text.split_once('\n').unwrap();
        assert!(intro.starts_with("The user supplied the following JSON string"));
        assert_eq!(encoded, r#""Unicode 😀 \"quoted\" \\u0040 and \u0040/tmp/data.txt""#);
        assert_eq!(serde_json::from_str::<String>(encoded).unwrap(), "Unicode 😀 \"quoted\" \\u0040 and @/tmp/data.txt");
        for prompt in ["  /mcp install unexpected", "\u{0085}/compact synthetic", "Keep DATA:image/png;base64,AAAA", "[Image #1: /tmp/x.png]"] {
            let text = literal_engine_prompt(prompt);
            let encoded = text.split_once('\n').unwrap().1;
            assert!(!encoded.contains('@') && !encoded.contains("[Image #") && !encoded.contains(':'), "{encoded}");
            assert_eq!(serde_json::from_str::<String>(encoded).unwrap(), prompt);
        }
        // JSON.stringify escapes control characters with short or lowercase \u escapes.
        assert!(literal_engine_prompt("@\u{1}\n").ends_with(r#""\u0040\u0001\n""#));
    }

    #[test]
    fn continuation_history_is_bounded_and_redacted() {
        assert_eq!(continuation_prompt("", &[], "now"), "now");
        let history = vec![
            (Role::User, "OLDEST".to_owned()),
            (Role::Assistant, format!("OLD PREFIX {} Visible tail secret", "😀\n\"".repeat(30000))),
            (Role::User, "Recent user detail".to_owned()),
            (Role::Assistant, "  ".to_owned()),
        ];
        let prompt = "Current unicode request 😀";
        let text = continuation_prompt("secret", &history, prompt);
        let history_bytes = text.len() - prompt.len();
        assert!(history_bytes > 60 * 1024 && history_bytes <= 64 * 1024, "{history_bytes}");
        let start = text.find('\n').unwrap() + 1;
        let end = text.find("\n\nCurrent user message:\n").unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&text[start..end]).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["role"], "assistant");
        assert!(parsed[0]["text"].as_str().unwrap().ends_with("Visible tail [redacted]"));
        assert_eq!(parsed[1], serde_json::json!({"role": "user", "text": "Recent user detail"}));
        assert!(!text.contains("OLDEST") && !text.contains("OLD PREFIX") && !text.contains("secret"));
        assert!(text.ends_with("Current user message:\nCurrent unicode request 😀"));
    }

    #[test]
    fn streamed_key_prefixes_are_held_until_resolved() {
        let key = "private-split-key";
        let (mut held, mut text) = (String::new(), String::new());
        append_streamed_text(key, &mut held, &mut text, "Provider returned private-split-ke");
        assert_eq!(text, "Provider returned ");
        assert_eq!(held, "private-split-ke");
        append_streamed_text(key, &mut held, &mut text, "y safely.");
        assert_eq!(text, "Provider returned [redacted] safely.");
        assert!(held.is_empty());
        let (mut held, mut text) = (String::new(), String::new());
        append_streamed_text(key, &mut held, &mut text, "abc p");
        assert_eq!((text.as_str(), held.as_str()), ("abc ", "p"));
        append_streamed_text(key, &mut held, &mut text, "x");
        assert_eq!((text.as_str(), held.as_str()), ("abc px", ""));
    }

    #[test]
    fn every_secret_is_redacted_and_split_prefixes_are_held() {
        let keys = vec!["api-key-value".to_owned(), "VAULT-SECRET-0123".to_owned(), String::new()];
        assert_eq!(redact(&keys, "a VAULT-SECRET-0123 b api-key-value", None), "a [redacted] b [redacted]");
        let (mut held, mut text) = (String::new(), String::new());
        append_streamed_text(&keys, &mut held, &mut text, "token VAULT-SEC");
        assert_eq!((text.as_str(), held.as_str()), ("token ", "VAULT-SEC"));
        append_streamed_text(&keys, &mut held, &mut text, "RET-0123 and api-");
        assert_eq!((text.as_str(), held.as_str()), ("token [redacted] and ", "api-"));
        append_streamed_text(&keys, &mut held, &mut text, "free");
        assert_eq!((text.as_str(), held.as_str()), ("token [redacted] and api-free", ""));
        let history = vec![(Role::Assistant, "said VAULT-SECRET-0123".to_owned())];
        assert!(!continuation_prompt(&keys, &history, "now").contains("VAULT-SECRET"));
    }

    #[test]
    fn titles_and_caps_follow_javascript_units() {
        assert_eq!(conversation_title("  a\n\tb  c "), " a b c ");
        assert_eq!(conversation_title(&"x".repeat(100)).len(), 70);
        assert_eq!(truncate_utf16("😀😀", 3), "😀");
        assert_eq!(redact("k", "akb", Some(4)), "a[re");
    }
}
