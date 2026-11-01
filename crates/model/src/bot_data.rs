//! Per-bot memories, knowledge files, and bot templates. Mirrors the additions in
//! `shared/types.ts`. All fields are new and optional on [`crate::Bot`], so state files written
//! before these features load unchanged.

use serde::{Deserialize, Serialize};

use crate::AvatarShape;

/// Who added a memory: the user in the details pane, the model through `save_memory` (after
/// approval), or a reviewed text import.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemorySource {
    User,
    Bot,
    Import,
}

/// One remembered fact ("이 Bot이 기억하는 내용").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Memory {
    pub id: String,
    pub text: String,
    pub source: MemorySource,
    pub created_at: String,
}

/// Metadata of one knowledge file ("지식"). The UTF-8 contents live in a private file under the
/// data directory, not in `state.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeFile {
    pub id: String,
    pub name: String,
    /// UTF-8 byte length of the stored contents.
    pub bytes: u64,
    /// SHA-256 (hex) of the stored contents.
    pub hash: String,
    pub created_at: String,
}

/// `POST /api/bots/:botId/memories` body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryInput {
    pub texts: Vec<String>,
    /// `true` when the texts come from the reviewed import flow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported: Option<bool>,
}

/// `POST /api/bots/:botId/knowledge` body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeInput {
    pub name: String,
    pub content: String,
}

pub const BOT_TEMPLATE_FORMAT: &str = "grok-bot.bot-template";
pub const BOT_TEMPLATE_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TemplateBot {
    pub name: String,
    pub color: String,
    pub instructions: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<AvatarShape>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TemplateSkill {
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TemplateMemory {
    pub text: String,
}

/// Portable bot template ("Bot 템플릿"). Carries no IDs, timestamps, conversations, routines,
/// credentials, or knowledge file contents; importing always creates fresh entities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BotTemplate {
    /// Always [`BOT_TEMPLATE_FORMAT`].
    pub format: String,
    /// Always [`BOT_TEMPLATE_VERSION`].
    pub version: u32,
    pub bot: TemplateBot,
    #[serde(default)]
    pub skills: Vec<TemplateSkill>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<TemplateMemory>,
}

/// `POST /api/bot-templates` body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TemplateImport {
    pub template: BotTemplate,
}
