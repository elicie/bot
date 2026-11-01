//! Private skill rules. Port of `server/skills.ts`. CONTRACT (store agent).

use grok_bot_model::{HttpResult, Skill, SkillInput};

pub const MAX_SKILLS_PER_BOT: usize = 100;
pub const MAX_SKILL_RESPONSE_BYTES: usize = 18 * 1024;
pub const MAX_ENABLED_SKILL_BYTES: usize = 128 * 1024;

/// Bounded JSON text returned to the model for skill tool results.
pub fn skill_response_text(value: &serde_json::Value) -> String {
    let _ = value;
    todo!("store agent")
}

/// Stable fingerprint of a bot's skills; part of the engine session fingerprint.
pub fn skill_fingerprint(skills: &[Skill]) -> String {
    let _ = skills;
    todo!("store agent")
}

/// Validate a model- or user-proposed skill (size, reviewability).
pub fn validate_skill_input(input: SkillInput) -> HttpResult<SkillInput> {
    let _ = input;
    todo!("store agent")
}

/// Parse a `SKILL.md` document (YAML front matter `name`, `description`; body = instructions).
pub fn import_skill_markdown(markdown: &str) -> HttpResult<SkillInput> {
    let _ = markdown;
    todo!("store agent")
}
