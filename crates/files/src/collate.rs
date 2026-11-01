//! Approximation of `String#localeCompare` (ICU root collation, the order Node uses for
//! directory listings) without an ICU dependency.
//!
//! Three levels like ICU's default strength: base characters (whitespace < punctuation < symbols
//! < digits < letters, letters case- and accent-insensitive), then accents, then case (lowercase
//! first). ASCII whitespace and punctuation use the exact CLDR root order. Non-ASCII symbols and
//! letters are ordered by code point within their class, which matches ICU for most scripts but
//! not every tailoring. Collation-equal names fall back to byte order so sorting stays total.

use std::cmp::Ordering;

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

const WHITESPACE: &str = "\t\n\u{b}\u{c}\r ";
const PUNCTUATION: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";

struct Key {
    primary: Vec<u64>,
    secondary: Vec<Vec<char>>,
    tertiary: Vec<bool>,
}

fn primary(character: char) -> Option<u64> {
    if let Some(index) = WHITESPACE.find(character) {
        return Some(index as u64);
    }
    if character.is_control() {
        return None;
    }
    if let Some(index) = PUNCTUATION.find(character) {
        return Some((1 << 32) | index as u64);
    }
    if let Some(digit) = character.to_digit(10) {
        return Some((3 << 32) | u64::from(digit));
    }
    if character.is_numeric() {
        return Some((3 << 32) | (16 + u64::from(u32::from(character))));
    }
    if character.is_alphabetic() {
        let lower = character.to_lowercase().next().unwrap_or(character);
        return Some((4 << 32) | u64::from(u32::from(lower)));
    }
    Some((2 << 32) | u64::from(u32::from(character)))
}

fn key(value: &str) -> Key {
    let mut key = Key { primary: Vec::new(), secondary: Vec::new(), tertiary: Vec::new() };
    for character in value.nfd() {
        if is_combining_mark(character) {
            if let Some(marks) = key.secondary.last_mut() {
                marks.push(character);
            }
            continue;
        }
        let Some(weight) = primary(character) else { continue };
        key.primary.push(weight);
        key.secondary.push(Vec::new());
        key.tertiary.push(character.is_uppercase());
    }
    key
}

pub(crate) fn compare(a: &str, b: &str) -> Ordering {
    let (left, right) = (key(a), key(b));
    left.primary
        .cmp(&right.primary)
        .then_with(|| left.secondary.cmp(&right.secondary))
        .then_with(|| left.tertiary.cmp(&right.tertiary))
        .then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::compare;

    #[test]
    fn matches_node_locale_compare_for_common_names() {
        // Expected order produced by Node 24 (`en-US`, full ICU): `names.sort((a, b) => a.localeCompare(b))`.
        let expected = [
            "_x", "-x", ",", ";", ":", "!", "?", ".x", "'", "\"", "(", ")", "[", "]", "{", "}", "@", "*", "/", "\\", "&", "#",
            "%", "`", "^", "+", "<", "=", ">", "|", "~", "$", "10", "9", "a", "A", "b", "B", "e", "é", "f", "x y", "x_y",
            "x-y", "x.y", "xy", "z", "Z", "α", "я", "가", "日",
        ];
        let mut names = expected.to_vec();
        names.reverse();
        names.sort_by(|a, b| compare(a, b));
        assert_eq!(names, expected);
    }

    #[test]
    fn orders_typical_file_names() {
        // Node 24 order for the same names.
        let expected = [
            "_config.yml", ".env", "a.txt", "a1", "A1", "a10", "a2", "B.md", "notes.txt", "Notes.txt", "Read_me", "read-me",
            "README.md", "readme.txt", "réadme.txt", "zeta",
        ];
        let mut names = expected.to_vec();
        names.reverse();
        names.sort_by(|a, b| compare(a, b));
        assert_eq!(names, expected);
    }
}
