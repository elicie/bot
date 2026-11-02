//! One conversation-owned document and its bounded revision snapshots.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CanvasSource {
    User,
    Bot,
    Restore,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CanvasVersion {
    pub revision: u64,
    pub title: String,
    pub language: String,
    pub content: String,
    pub updated_at: String,
    pub source: CanvasSource,
    /// Public projection only: secret masking makes this version read-only.
    #[serde(default, skip_serializing_if = "is_false")]
    pub redacted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Canvas {
    pub revision: u64,
    pub title: String,
    pub language: String,
    pub content: String,
    pub updated_at: String,
    pub source: CanvasSource,
    /// Old snapshots in ascending revision order, newest last.
    #[serde(default)]
    pub history: Vec<CanvasVersion>,
    /// Describes the current version only; history entries carry their own marker.
    #[serde(default, skip_serializing_if = "is_false")]
    pub redacted: bool,
}

impl Canvas {
    pub fn version(&self) -> CanvasVersion {
        CanvasVersion {
            revision: self.revision,
            title: self.title.clone(),
            language: self.language.clone(),
            content: self.content.clone(),
            updated_at: self.updated_at.clone(),
            source: self.source,
            redacted: self.redacted,
        }
    }
}

/// A user save: revision 0 creates an absent canvas, otherwise compare-and-swap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CanvasInput {
    pub revision: u64,
    pub title: String,
    pub language: String,
    pub content: String,
}
