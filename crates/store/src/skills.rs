//! Private skill validation, import, capacity, and persistence rules.

use std::collections::HashSet;
use std::sync::LazyLock;

use grok_bot_model::{HttpError, HttpResult, Skill, SkillInput, SkillPatch, new_id};
use regex::Regex;
use serde::Serialize;
use sha2::Digest;

use crate::validate::{Issues, is_utc_timestamp, is_uuid, js_len, js_trim};

pub const MAX_SKILLS_PER_BOT: usize = 100;
pub const MAX_SKILL_RESPONSE_BYTES: usize = 18 * 1024;
pub const MAX_ENABLED_SKILL_BYTES: usize = 128 * 1024;
pub const MAX_SKILL_MARKDOWN_BYTES: usize = 20 * 1024;

const SIZE_ERROR: &str = "Skill text exceeds the 18 KiB serialized limit. Shorten it so the engine receives the complete instructions.";
const CONTROL_ERROR: &str = "Skill names cannot contain control characters.";
const INSTRUCTIONS_ERROR: &str = "Provide nonempty skill instructions without NUL bytes.";

/// Bounded JSON text returned to the model for skill tool results.
/// Upstream scans MCP text for data URIs before JSON reaches the model, so every `data:` (any case)
/// is written as `data:`.
pub fn skill_response_text(value: &serde_json::Value) -> String {
    let text = serde_json::to_string(value).unwrap_or_default();
    let mut output = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(index) = rest.as_bytes().windows(5).position(|window| window[..4].eq_ignore_ascii_case(b"data") && window[4] == b':') {
        output.push_str(&rest[..index + 4]);
        output.push_str("\\u003a");
        rest = &rest[index + 5..];
    }
    output.push_str(rest);
    output
}

#[derive(Serialize)]
struct Fields<'a> {
    name: &'a str,
    description: &'a str,
    instructions: &'a str,
}

fn reviewable(name: &str, description: &str, instructions: &str) -> bool {
    serde_json::to_vec(&Fields { name, description, instructions }).is_ok_and(|bytes| bytes.len() <= MAX_SKILL_RESPONSE_BYTES)
}

/// `skillSetBytes`: serialized size of the enabled skills the engine receives.
pub fn skill_set_bytes(skills: &[Skill]) -> usize {
    #[derive(Serialize)]
    struct Entry<'a> {
        id: &'a str,
        name: &'a str,
        description: &'a str,
        instructions: &'a str,
    }
    let entries: Vec<Entry<'_>> = skills
        .iter()
        .filter(|skill| skill.enabled)
        .map(|skill| Entry { id: &skill.id, name: &skill.name, description: &skill.description, instructions: &skill.instructions })
        .collect();
    serde_json::to_vec(&entries).map_or(usize::MAX, |bytes| bytes.len())
}

/// Stable fingerprint of a bot's skills; part of the engine session fingerprint.
pub fn skill_fingerprint(skills: &[Skill]) -> String {
    #[derive(Serialize)]
    struct Entry<'a> {
        id: &'a str,
        revision: u64,
        name: &'a str,
        description: &'a str,
        instructions: &'a str,
    }
    let entries: Vec<Entry<'_>> = skills
        .iter()
        .filter(|skill| skill.enabled)
        .map(|skill| Entry { id: &skill.id, revision: skill.revision, name: &skill.name, description: &skill.description, instructions: &skill.instructions })
        .collect();
    hex::encode(sha2::Sha256::digest(serde_json::to_vec(&entries).unwrap_or_default()))
}

fn has_control(value: &str) -> bool {
    value.chars().any(|c| matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'))
}

fn name_issues(issues: &mut Issues, name: &mut String) {
    *name = js_trim(name).to_owned();
    issues.length("name", name, 1, 100);
    if has_control(name) {
        issues.push("name", CONTROL_ERROR);
    }
}

fn description_issues(issues: &mut Issues, description: &mut String) {
    *description = js_trim(description).to_owned();
    issues.length("description", description, 1, 2000);
}

fn instructions_issues(issues: &mut Issues, instructions: &str) {
    issues.length("instructions", instructions, 1, 30000);
    if js_trim(instructions).is_empty() || instructions.contains('\0') {
        issues.push("instructions", INSTRUCTIONS_ERROR);
    }
}

/// Validate a model- or user-proposed skill (size, reviewability). `skillInputSchema`.
pub fn validate_skill_input(mut input: SkillInput) -> HttpResult<SkillInput> {
    let mut issues = Issues::new();
    name_issues(&mut issues, &mut input.name);
    description_issues(&mut issues, &mut input.description);
    instructions_issues(&mut issues, &input.instructions);
    if !reviewable(&input.name, &input.description, &input.instructions) {
        issues.push("", SIZE_ERROR);
    }
    issues.finish()?;
    Ok(input)
}

/// `skillPatchSchema`.
pub fn validate_skill_patch(mut input: SkillPatch) -> HttpResult<SkillPatch> {
    let mut issues = Issues::new();
    if let Some(name) = input.name.as_mut() {
        name_issues(&mut issues, name);
    }
    if let Some(description) = input.description.as_mut() {
        description_issues(&mut issues, description);
    }
    if let Some(instructions) = input.instructions.as_ref() {
        instructions_issues(&mut issues, instructions);
    }
    issues.number("revision", input.revision, 1, u64::MAX);
    issues.finish()?;
    Ok(input)
}

/// `skillSchema` for a persisted skill. Applies the schema's trim transforms.
pub(crate) fn persisted_skill_valid(skill: &mut Skill) -> bool {
    let mut issues = Issues::new();
    name_issues(&mut issues, &mut skill.name);
    description_issues(&mut issues, &mut skill.description);
    instructions_issues(&mut issues, &skill.instructions);
    issues.is_empty() && is_uuid(&skill.id) && skill.revision >= 1 && is_utc_timestamp(&skill.created_at) && is_utc_timestamp(&skill.updated_at)
}

/// `validateSkillSet`.
pub fn validate_skill_set(skills: &[Skill]) -> HttpResult<()> {
    if skills.iter().any(|skill| !reviewable(&skill.name, &skill.description, &skill.instructions)) {
        return Err(HttpError::too_large(SIZE_ERROR));
    }
    if skills.len() > MAX_SKILLS_PER_BOT {
        return Err(HttpError::conflict("A bot can contain at most 100 private skills."));
    }
    if skills.iter().map(|skill| skill.id.as_str()).collect::<HashSet<_>>().len() != skills.len() {
        return Err(HttpError::bad_request("Private skill IDs must be unique."));
    }
    if skills.iter().map(|skill| skill.name.to_lowercase()).collect::<HashSet<_>>().len() != skills.len() {
        return Err(HttpError::conflict("A skill with this name already exists for the bot."));
    }
    if skill_set_bytes(skills) > MAX_ENABLED_SKILL_BYTES {
        return Err(HttpError::too_large("Enabled private skill instructions exceed the 128 KiB per-bot limit. Disable a skill before adding more."));
    }
    Ok(())
}

/// `newSkill`.
pub fn new_skill(input: SkillInput, now: &str) -> HttpResult<Skill> {
    let input = validate_skill_input(input)?;
    Ok(Skill {
        id: new_id(),
        name: input.name,
        description: input.description,
        instructions: input.instructions,
        enabled: true,
        revision: 1,
        created_at: now.to_owned(),
        updated_at: now.to_owned(),
    })
}

/// `nextSkill`: optimistic revision check and field merge.
pub fn next_skill(current: &Skill, input: SkillPatch, now: &str) -> HttpResult<Skill> {
    let input = validate_skill_patch(input)?;
    if current.revision != input.revision {
        return Err(HttpError::conflict("The skill changed since it was opened. Reload its latest version before saving."));
    }
    if input.name.is_none() && input.description.is_none() && input.instructions.is_none() && input.enabled.is_none() {
        return Err(HttpError::bad_request("Provide a skill field to update."));
    }
    let mut next = current.clone();
    if let Some(name) = input.name {
        next.name = name;
    }
    if let Some(description) = input.description {
        next.description = description;
    }
    if let Some(instructions) = input.instructions {
        next.instructions = instructions;
    }
    if let Some(enabled) = input.enabled {
        next.enabled = enabled;
    }
    next.revision = current.revision + 1;
    next.updated_at = now.to_owned();
    if !reviewable(&next.name, &next.description, &next.instructions) {
        return Err(HttpError::too_large(SIZE_ERROR));
    }
    Ok(next)
}

static IMPORTED_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9]+(?:-[a-z0-9]+)*$").unwrap());

/// Parse a `SKILL.md` document (YAML front matter `name`, `description`; body = instructions).
pub fn import_skill_markdown(markdown: &str) -> HttpResult<SkillInput> {
    // `POST /api/bots/:botId/skills/import` body schema: `markdown: z.string().max(20 * 1024)`.
    let mut issues = Issues::new();
    issues.max_length("markdown", markdown, MAX_SKILL_MARKDOWN_BYTES);
    issues.finish()?;
    if markdown.len() > MAX_SKILL_MARKDOWN_BYTES || markdown.contains('\0') {
        return Err(HttpError::too_large("SKILL.md must be UTF-8 text of at most 20 KiB without NUL bytes."));
    }
    let text = markdown.strip_prefix('\u{feff}').unwrap_or(markdown);
    let Some((frontmatter, body)) = split_frontmatter(text).filter(|(frontmatter, _)| frontmatter.len() <= 4096) else {
        return Err(HttpError::bad_request("SKILL.md requires a YAML frontmatter block of at most 4096 bytes."));
    };
    let fields = yaml::frontmatter(frontmatter)?;
    let unsupported = || {
        HttpError::bad_request(
            "SKILL.md supports only name (lowercase letters, digits, hyphens; 64 characters) and description (1024 characters). Tool, script, model, and hook fields are unsupported.",
        )
    };
    let yaml::Value::Mapping(entries) = fields else { return Err(unsupported()) };
    let (mut name, mut description) = (None, None);
    for (key, value) in entries {
        let yaml::Value::String(value) = value else { return Err(unsupported()) };
        match key.as_deref() {
            Some("name") => name = Some(value),
            Some("description") => description = Some(value),
            _ => return Err(unsupported()),
        }
    }
    let (Some(name), Some(description)) = (name, description) else { return Err(unsupported()) };
    let description = js_trim(&description).to_owned();
    if js_len(&name) > 64 || !IMPORTED_NAME.is_match(&name) || js_len(&description) < 1 || js_len(&description) > 1024 {
        return Err(unsupported());
    }
    validate_skill_input(SkillInput { name, description, instructions: body.to_owned() })
}

/// `/^---\r?\n([\s\S]*?)\r?\n---\r?\n([\s\S]*)$/`
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n"))?;
    let mut search = 0;
    while let Some(found) = rest[search..].find("\n---") {
        let newline = search + found;
        let after = &rest[newline + 4..];
        let body = after.strip_prefix('\n').or_else(|| after.strip_prefix("\r\n"));
        if let Some(body) = body {
            let end = if newline > 0 && rest.as_bytes()[newline - 1] == b'\r' { newline - 1 } else { newline };
            // The lazy group prefers the shortest match; a preceding `\r` belongs to the separator.
            return Some((&rest[..end], body));
        }
        search = newline + 1;
    }
    None
}

/// Minimal YAML 1.2 core-schema reader matching the `yaml` package's strict, unique-key document
/// parsing for front matter: anything it reports as an error or warning (syntax, unresolved tags,
/// duplicate keys, multiple documents) is rejected, aliases are refused, and plain scalars resolve
/// to null/bool/int/float before the string check.
mod yaml {
    use std::collections::HashSet;
    use std::sync::LazyLock;

    use grok_bot_model::{HttpError, HttpResult};
    use regex::Regex;
    use yaml_rust2::parser::{Event, Parser, Tag};
    use yaml_rust2::scanner::TScalarStyle;

    pub enum Value {
        String(String),
        Other,
        Mapping(Vec<(Option<String>, Value)>),
    }

    fn invalid() -> HttpError {
        HttpError::bad_request("SKILL.md frontmatter must contain valid YAML with unique fields.")
    }

    fn aliases() -> HttpError {
        HttpError::bad_request("SKILL.md YAML aliases and custom values are unsupported.")
    }

    static NULL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:~|null|Null|NULL)?$").unwrap());
    static BOOL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:true|True|TRUE|false|False|FALSE)$").unwrap());
    static INT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:[-+]?[0-9]+|0o[0-7]+|0x[0-9a-fA-F]+)$").unwrap());
    static FLOAT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(?:[-+]?(?:\.[0-9]+|[0-9]+(?:\.[0-9]*)?)(?:[eE][-+]?[0-9]+)?|[-+]?\.(?:inf|Inf|INF)|\.nan|\.NaN|\.NAN)$").unwrap());

    struct Reader<'a> {
        parser: Parser<std::str::Chars<'a>>,
        alias: bool,
    }

    impl Reader<'_> {
        fn next(&mut self) -> HttpResult<Event> {
            // An alias to an undefined anchor fails in `toJS` (alias resolution), not in parsing.
            self.parser.next_token().map(|(event, _)| event).map_err(|error| if error.info().contains("unknown anchor") { aliases() } else { invalid() })
        }

        fn scalar(value: String, style: TScalarStyle, tag: Option<Tag>) -> HttpResult<Value> {
            let Some(tag) = tag else {
                if style != TScalarStyle::Plain || !(NULL.is_match(&value) || BOOL.is_match(&value) || INT.is_match(&value) || FLOAT.is_match(&value)) {
                    return Ok(Value::String(value));
                }
                return Ok(Value::Other);
            };
            if tag.handle.is_empty() && tag.suffix == "!" {
                return Ok(Value::String(value));
            }
            if tag.handle != "tag:yaml.org,2002:" {
                return Err(invalid());
            }
            let matches = match tag.suffix.as_str() {
                "str" => return Ok(Value::String(value)),
                "null" => NULL.is_match(&value),
                "bool" => BOOL.is_match(&value),
                "int" => INT.is_match(&value),
                "float" => FLOAT.is_match(&value) || INT.is_match(&value),
                "binary" | "timestamp" => true,
                _ => false,
            };
            if matches { Ok(Value::Other) } else { Err(invalid()) }
        }

        fn node(&mut self, event: Event) -> HttpResult<Value> {
            match event {
                Event::Alias(_) => {
                    self.alias = true;
                    Ok(Value::Other)
                }
                Event::Scalar(value, style, _, tag) => Self::scalar(value, style, tag),
                Event::SequenceStart(_, tag) => {
                    Self::collection_tag(tag, "seq")?;
                    loop {
                        match self.next()? {
                            Event::SequenceEnd => return Ok(Value::Other),
                            event => {
                                self.node(event)?;
                            }
                        }
                    }
                }
                Event::MappingStart(_, tag) => {
                    Self::collection_tag(tag, "map")?;
                    let mut entries = Vec::new();
                    let mut keys = HashSet::new();
                    loop {
                        let key = match self.next()? {
                            Event::MappingEnd => return Ok(Value::Mapping(entries)),
                            event => self.node(event)?,
                        };
                        let key = match key {
                            Value::String(key) => {
                                if !keys.insert(key.clone()) {
                                    return Err(invalid());
                                }
                                Some(key)
                            }
                            _ => None,
                        };
                        let event = self.next()?;
                        let value = self.node(event)?;
                        entries.push((key, value));
                    }
                }
                _ => Err(invalid()),
            }
        }

        fn collection_tag(tag: Option<Tag>, expected: &str) -> HttpResult<()> {
            match tag {
                None => Ok(()),
                Some(tag) if tag.handle == "tag:yaml.org,2002:" && tag.suffix == expected => Ok(()),
                Some(tag) if tag.handle.is_empty() && tag.suffix == "!" => Ok(()),
                Some(_) => Err(invalid()),
            }
        }
    }

    pub fn frontmatter(text: &str) -> HttpResult<Value> {
        // The `yaml` package reports TAB_AS_INDENT for a tab at the start of a line; yaml-rust2
        // treats it as separation whitespace there.
        if text.split('\n').any(|line| line.starts_with('\t')) {
            return Err(invalid());
        }
        let mut reader = Reader { parser: Parser::new_from_str(text), alias: false };
        if !matches!(reader.next()?, Event::StreamStart) {
            return Err(invalid());
        }
        let mut value = None;
        loop {
            match reader.next()? {
                Event::StreamEnd => break,
                Event::DocumentStart => {
                    if value.is_some() {
                        return Err(invalid());
                    }
                    let event = reader.next()?;
                    value = Some(reader.node(event)?);
                    if !matches!(reader.next()?, Event::DocumentEnd) {
                        return Err(invalid());
                    }
                }
                _ => return Err(invalid()),
            }
        }
        if reader.alias {
            return Err(aliases());
        }
        Ok(value.unwrap_or(Value::Other))
    }
}
