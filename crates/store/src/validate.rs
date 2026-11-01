//! JavaScript and zod compatibility helpers. The Node backend validated input with zod and measured
//! strings with JavaScript semantics (UTF-16 lengths, ECMAScript `trim`); these helpers reproduce
//! both so limits and error text stay identical.

use std::sync::LazyLock;

use chrono::{DateTime, NaiveDate, Utc};
use grok_bot_model::{HttpError, HttpResult};
use regex::Regex;

/// ECMAScript `WhiteSpace` and `LineTerminator` (what `String#trim` strips).
pub fn js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}' | '\u{000A}' | '\u{000B}' | '\u{000C}' | '\u{000D}' | '\u{0020}' | '\u{00A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

/// `String#trim`.
pub fn js_trim(value: &str) -> &str {
    value.trim_matches(js_whitespace)
}

/// `String#length` (UTF-16 code units).
pub fn js_len(value: &str) -> usize {
    value.chars().map(char::len_utf16).sum()
}

/// `String#slice(0, units)`. A surrogate pair split by the limit is dropped entirely because Rust
/// strings cannot hold the lone surrogate JavaScript would keep.
pub fn js_slice(value: &str, units: usize) -> &str {
    let mut used = 0;
    for (index, c) in value.char_indices() {
        used += c.len_utf16();
        if used > units {
            return &value[..index];
        }
    }
    value
}

/// Collects zod-style issues and renders them like `handleError` in `server/app.ts`.
#[derive(Default)]
pub struct Issues(Vec<String>);

impl Issues {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, path: &str, message: impl AsRef<str>) {
        let path = if path.is_empty() { "Request" } else { path };
        self.0.push(format!("{path}: {}", message.as_ref()));
    }

    /// `z.string().min(min).max(max)` on an already transformed value.
    pub fn length(&mut self, path: &str, value: &str, min: usize, max: usize) {
        let length = js_len(value);
        if length < min {
            self.push(path, format!("Too small: expected string to have >={min} characters"));
        }
        if length > max {
            self.push(path, format!("Too big: expected string to have <={max} characters"));
        }
    }

    pub fn max_length(&mut self, path: &str, value: &str, max: usize) {
        self.length(path, value, 0, max);
    }

    pub fn number(&mut self, path: &str, value: u64, min: u64, max: u64) {
        if value < min {
            self.push(path, format!("Too small: expected number to be >={min}"));
        }
        if value > max {
            self.push(path, format!("Too big: expected number to be <={max}"));
        }
    }

    pub fn uuid(&mut self, path: &str, value: &str) {
        if !is_uuid(value) {
            self.push(path, "Invalid UUID");
        }
    }

    pub fn pattern(&mut self, path: &str, matched: bool, pattern: &str) {
        if !matched {
            self.push(path, format!("Invalid string: must match pattern {pattern}"));
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn finish(self) -> HttpResult<()> {
        if self.0.is_empty() {
            return Ok(());
        }
        Err(HttpError::bad_request(js_slice(&self.0.join("; "), 1000).to_owned()))
    }
}

static UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[1-8][0-9a-fA-F]{3}-[89abAB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}|00000000-0000-0000-0000-000000000000|ffffffff-ffff-ffff-ffff-ffffffffffff)$",
    )
    .unwrap()
});

/// zod 4 `z.string().uuid()` (RFC 9562 version and variant bits).
pub fn is_uuid(value: &str) -> bool {
    UUID.is_match(value)
}

const DATE: &str = r"(?:(?:[0-9][0-9][2468][048]|[0-9][0-9][13579][26]|[0-9][0-9]0[48]|[02468][048]00|[13579][26]00)-02-29|[0-9]{4}-(?:(?:0[13578]|1[02])-(?:0[1-9]|[12][0-9]|3[01])|(?:0[469]|11)-(?:0[1-9]|[12][0-9]|30)|(?:02)-(?:0[1-9]|1[0-9]|2[0-8])))";
const TIME: &str = r"(?:[01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9](?:\.[0-9]+)?";

static DATETIME_OFFSET: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!(r"^{DATE}T(?:{TIME}(?:Z|(?:[+-](?:[01][0-9]|2[0-3]):[0-5][0-9])))$")).unwrap());
static DATETIME_UTC: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!(r"^{DATE}T(?:{TIME}(?:Z))$")).unwrap());

/// zod 4 `z.iso.datetime({ offset: true })`.
pub fn is_timestamp(value: &str) -> bool {
    DATETIME_OFFSET.is_match(value)
}

/// zod 4 `z.iso.datetime()` (UTC `Z` only).
pub fn is_utc_timestamp(value: &str) -> bool {
    DATETIME_UTC.is_match(value)
}

/// `Date.parse` for the ISO forms the application stores: milliseconds since the epoch, with extra
/// fractional digits truncated like V8. Returns `None` for anything else.
pub fn parse_ms(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let text = value.get(range)?;
        if text.bytes().all(|b| b.is_ascii_digit()) { text.parse().ok() } else { None }
    };
    if bytes.len() < 17 || bytes.get(4) != Some(&b'-') || bytes.get(7) != Some(&b'-') || bytes.get(10) != Some(&b'T') || bytes.get(13) != Some(&b':') {
        return None;
    }
    let year = number(0..4)?;
    let month = number(5..7)?;
    let day = number(8..10)?;
    let hour = number(11..13)?;
    let minute = number(14..16)?;
    let mut index = 16;
    let mut second = 0;
    let mut millis = 0;
    if bytes.get(index) == Some(&b':') {
        second = number(index + 1..index + 3)?;
        index += 3;
        if bytes.get(index) == Some(&b'.') {
            index += 1;
            let start = index;
            while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
            if index == start {
                return None;
            }
            let digits = &value[start..index.min(start + 3)];
            millis = digits.parse::<i64>().ok()? * 10_i64.pow(3 - digits.len() as u32);
        }
    }
    let offset_minutes = match &value[index..] {
        "Z" => 0,
        rest if rest.len() == 6 && (rest.starts_with('+') || rest.starts_with('-')) && rest.as_bytes()[3] == b':' => {
            let hours = number(index + 1..index + 3)?;
            let minutes = number(index + 4..index + 6)?;
            let sign = if rest.starts_with('-') { -1 } else { 1 };
            sign * (hours * 60 + minutes)
        }
        _ => return None,
    };
    if hour > 24 || minute > 59 || second > 59 {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(year as i32, month as u32, day as u32)?;
    let days = date.signed_duration_since(NaiveDate::from_ymd_opt(1970, 1, 1)?).num_days();
    Some((((days * 24 + hour) * 60 + minute - offset_minutes) * 60 + second) * 1000 + millis)
}

/// `Date#toISOString` for epoch milliseconds.
pub fn iso_ms(ms: i64) -> HttpResult<String> {
    DateTime::<Utc>::from_timestamp_millis(ms).map(grok_bot_model::iso).ok_or_else(internal)
}

/// The response `handleError` produced for unexpected (non-HTTP) failures.
pub fn internal() -> HttpError {
    HttpError::new(500, "The operation failed. Check that the workspace and data directory are accessible.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn javascript_string_semantics() {
        assert_eq!(js_trim("\u{feff} a \u{3000}"), "a");
        assert_eq!(js_trim("\u{85}a"), "\u{85}a");
        assert_eq!(js_len("😀a"), 3);
        assert_eq!(js_slice("a😀b", 2), "a");
        assert_eq!(js_slice("a😀b", 3), "a😀");
    }

    #[test]
    fn zod_formats_and_date_parse() {
        assert!(is_uuid("7c9e6679-7425-40de-944b-e07fc1f90ae7"));
        assert!(!is_uuid("7c9e6679-7425-00de-944b-e07fc1f90ae7"));
        assert!(is_timestamp("2026-01-01T00:00:00.123456+09:00"));
        assert!(!is_timestamp("2026-01-02T12:00:00"));
        assert!(!is_timestamp("2026-01-02T12:00Z"));
        assert!(!is_timestamp("2025-02-29T00:00:00Z"));
        assert!(is_timestamp("2024-02-29T00:00:00Z"));
        assert!(!is_utc_timestamp("2026-01-01T00:00:00+00:00"));
        assert_eq!(parse_ms("2026-01-01T00:00:00.9999Z"), Some(1_767_225_600_999));
        assert_eq!(parse_ms("2026-01-01T00:00:00.1Z"), Some(1_767_225_600_100));
        assert_eq!(parse_ms("0000-01-01T00:00:00Z"), Some(-62_167_219_200_000));
        assert_eq!(iso_ms(parse_ms("2026-01-01T00:00:00.123456789123+09:00").unwrap()).unwrap(), "2025-12-31T15:00:00.123Z");
    }
}
