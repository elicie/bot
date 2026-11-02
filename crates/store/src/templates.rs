//! Bot templates ("Bot 템플릿"): export a bot's name, appearance, instructions, private skills,
//! and optionally its memories as JSON, and create a new bot from such a file.
//!
//! Decisions: templates never carry IDs, timestamps, revisions, conversations, routines,
//! credentials, or knowledge files (contents are per-installation data and can be large; names
//! alone would be misleading). Import validates every field with the same rules as direct edits,
//! rejects unknown fields, and always assigns fresh IDs. Exported text is redacted of the API key.

use grok_bot_model::{
    BOT_TEMPLATE_FORMAT, BOT_TEMPLATE_VERSION, Bot, BotTemplate, HttpError, HttpResult, SkillInput, TemplateBot, TemplateMemory, TemplateSkill, iso, new_id,
};

use crate::memory::template_memories;
use crate::skills::{new_skill, validate_skill_set};
use crate::validate::Issues;
use crate::{State, bot_field_issues};

/// Prefix every `field: message` issue of `error` with `path`.
fn nested(error: HttpError, path: &str) -> HttpError {
    let message = error
        .message
        .split("; ")
        .map(|issue| if issue.contains(": ") { format!("{path}.{issue}") } else { format!("{path}: {issue}") })
        .collect::<Vec<_>>()
        .join("; ");
    HttpError::new(error.status, message)
}

/// Strict template validation: exact format and version, bot fields like `POST /api/bots`, and
/// every skill and memory like their own endpoints. Returns the normalized template.
pub fn validate_template(mut template: BotTemplate) -> HttpResult<BotTemplate> {
    if template.format != BOT_TEMPLATE_FORMAT {
        return Err(HttpError::bad_request("This file is not a Grok Bot template."));
    }
    if template.version != BOT_TEMPLATE_VERSION {
        return Err(HttpError::bad_request("This template version is not supported."));
    }
    let mut issues = Issues::new();
    let bot = &mut template.bot;
    bot_field_issues(&mut issues, Some(&mut bot.name), Some(&bot.color), Some(&bot.instructions));
    issues.finish().map_err(|error| nested(error, "bot"))?;
    Ok(template)
}

impl State {
    /// The template of one bot. `include_memories` adds its memories.
    pub fn export_template(&self, bot_id: &str, include_memories: bool) -> HttpResult<BotTemplate> {
        let bot = self.bot(bot_id)?;
        let skills = bot
            .skills
            .iter()
            .flatten()
            .map(|skill| TemplateSkill {
                name: self.redact(&skill.name),
                description: self.redact(&skill.description),
                instructions: self.redact(&skill.instructions),
                enabled: skill.enabled,
            })
            .collect();
        let memories = if include_memories {
            bot.memories.iter().flatten().map(|memory| TemplateMemory { text: self.redact(&memory.text) }).collect()
        } else {
            Vec::new()
        };
        Ok(BotTemplate {
            format: BOT_TEMPLATE_FORMAT.into(),
            version: BOT_TEMPLATE_VERSION,
            bot: TemplateBot { name: self.redact(&bot.name), color: bot.color.clone(), instructions: self.redact(&bot.instructions), shape: bot.shape },
            skills,
            memories,
        })
    }

    /// Create a new bot from a validated template with fresh IDs for the bot, its skills, and its
    /// memories. The whole bot is admitted or nothing is written.
    pub fn add_bot_from_template(&mut self, template: BotTemplate) -> HttpResult<Bot> {
        let template = validate_template(template)?;
        self.assert_idle()?;
        let now = iso(self.now());
        let mut skills = Vec::with_capacity(template.skills.len());
        for (index, skill) in template.skills.into_iter().enumerate() {
            let input = SkillInput { name: skill.name, description: skill.description, instructions: skill.instructions };
            let mut created = new_skill(input, &now).map_err(|error| nested(error, &format!("skills.{index}")))?;
            created.enabled = skill.enabled;
            skills.push(created);
        }
        validate_skill_set(&skills)?;
        let memories = template_memories(template.memories.into_iter().map(|memory| memory.text).collect(), &now)?;
        let bot = Bot {
            id: new_id(),
            name: template.bot.name,
            color: template.bot.color,
            instructions: template.bot.instructions,
            shape: template.bot.shape,
            skills: (!skills.is_empty()).then_some(skills),
            pinned: None,
            hidden: None,
            created_at: now,
            memories: (!memories.is_empty()).then_some(memories),
            knowledge: None,
            unread: None,
            notify_on_updates: None,
        };
        {
            let mut prospect = self.prospect();
            prospect.bots.push(&bot);
            Self::check_capacity(&prospect)?;
        }
        self.disk_mut().bots.push(bot.clone());
        Ok(bot)
    }
}
