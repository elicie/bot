//! Shared data contract. Mirrors `shared/types.ts` and the persisted `state.json` schema exactly;
//! the React frontend and existing state files depend on these JSON shapes.

use std::fmt;
use std::sync::Arc;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(Utc::now)
}

/// JavaScript `Date#toISOString` format: millisecond precision, `Z` suffix.
pub fn iso(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Error surfaced to HTTP clients as `{ "error": message }` with `status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpError {
    pub status: u16,
    pub message: String,
}

impl HttpError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(403, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, message)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(409, message)
    }
    pub fn too_large(message: impl Into<String>) -> Self {
        Self::new(413, message)
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(503, message)
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.status)
    }
}

impl std::error::Error for HttpError {}

pub type HttpResult<T> = Result<T, HttpError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Idle,
    Starting,
    Running,
    AwaitingPermission,
    Completed,
    Cancelled,
    Error,
    Interrupted,
}

impl RunStatus {
    /// `starting`, `running`, `awaiting_permission`.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::AwaitingPermission)
    }
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Error | Self::Interrupted)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolActivity {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub status: ToolStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_only: Option<bool>,
    /// ID of the user's permission rule that approved this action without a card
    /// ("내 규칙으로 자동 승인됨").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_by_rule: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub text: String,
    pub created_at: String,
    pub tools: Vec<ToolActivity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_exchange_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_bot_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BotExchangeMessage {
    pub id: String,
    pub bot_id: String,
    pub text: String,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BotExchange {
    pub id: String,
    pub bot_ids: [String; 2],
    pub messages: Vec<BotExchangeMessage>,
    pub source_conversation_id: String,
    pub source_message_id: String,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Queued,
    Running,
    Completed,
    Cancelled,
    Error,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BotDelivery {
    pub id: String,
    pub exchange_id: String,
    pub source_conversation_id: String,
    pub source_message_id: String,
    pub recipient_conversation_id: String,
    pub from_bot_id: String,
    pub to_bot_id: String,
    pub text: String,
    pub chain_id: String,
    pub hop: u32,
    pub created_at: String,
    pub status: DeliveryStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionOption {
    pub id: String,
    pub label: String,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingPermission {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub description: String,
    pub options: Vec<PermissionOption>,
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_only: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunState {
    /// Empty string until the first run starts.
    pub id: String,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<PendingPermission>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
}

impl RunState {
    pub fn idle() -> Self {
        Self { id: String::new(), status: RunStatus::Idle, error: None, permission: None, started_at: None, finished_at: None }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Conversation {
    pub id: String,
    pub bot_id: String,
    pub title: String,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_fingerprint: Option<String>,
    pub run: RunState,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AvatarShape {
    Blob,
    Cloud,
    Square,
    Spark,
    Clover,
    Heart,
    Flower,
    Droplet,
    Pill,
    Wedge,
    House,
    Star,
    Hexagon,
    Pumpkin,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub enabled: bool,
    pub revision: u64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Bot {
    pub id: String,
    pub name: String,
    pub color: String,
    pub instructions: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<AvatarShape>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<Skill>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    pub created_at: String,
}

/// Persisted provider settings, including the secret API key. Never sent to browsers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrivateSettings {
    pub base_url: String,
    pub model: String,
    pub workspace: String,
    pub api_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicSettings {
    pub base_url: String,
    pub model: String,
    pub has_api_key: bool,
    pub workspace: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettingsInput {
    pub base_url: String,
    pub model: String,
    pub workspace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_api_key: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeInfo {
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum RoutineSchedule {
    Once { at: String },
    Interval { minutes: u32 },
    Daily { time: String, timezone: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Routine {
    pub id: String,
    pub bot_id: String,
    pub name: String,
    pub prompt: String,
    pub schedule: RoutineSchedule,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Webhook trigger. Only a SHA-256 hash of the key is stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<RoutineWebhook>,
}

/// Webhook trigger of a routine. `key_hash` (hex SHA-256 of the key) is persisted but removed from
/// the public snapshot; `key_hint` is the last characters of the key for recognition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoutineWebhook {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_hash: String,
    pub key_hint: String,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoutineInput {
    pub bot_id: String,
    pub name: String,
    pub prompt: String,
    pub schedule: RoutineSchedule,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineResultStatus {
    Completed,
    Cancelled,
    Error,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoutineResult {
    pub status: RoutineResultStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    pub finished_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutineTrigger {
    Manual,
    Scheduled,
    Webhook,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoutineRun {
    pub id: String,
    pub routine_id: String,
    pub routine_name: String,
    pub bot_id: String,
    pub conversation_id: String,
    pub prompt: String,
    pub trigger: RoutineTrigger,
    pub scheduled_for: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<RoutineResult>,
}

/// `state.json`. Field order and names match the Node implementation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiskState {
    /// Always 1.
    pub version: u8,
    #[serde(default)]
    pub revision: u64,
    pub settings: PrivateSettings,
    pub bots: Vec<Bot>,
    pub conversations: Vec<Conversation>,
    #[serde(default)]
    pub routines: Vec<Routine>,
    #[serde(default)]
    pub routine_runs: Vec<RoutineRun>,
    #[serde(default)]
    pub bot_exchanges: Vec<BotExchange>,
    #[serde(default)]
    pub bot_deliveries: Vec<BotDelivery>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permission_rules: Vec<PermissionRule>,
}

/// Public snapshot sent to browsers over HTTP and SSE.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppState {
    pub revision: u64,
    pub bots: Vec<Bot>,
    pub conversations: Vec<Conversation>,
    pub routines: Vec<Routine>,
    pub routine_runs: Vec<RoutineRun>,
    pub bot_exchanges: Vec<BotExchange>,
    pub bot_deliveries: Vec<BotDelivery>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permission_rules: Vec<PermissionRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routine_scheduling_error: Option<String>,
    pub settings: PublicSettings,
    pub runtime: RuntimeInfo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileEntryType {
    Directory,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub entry_type: FileEntryType,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileListing {
    pub path: String,
    pub entries: Vec<FileEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDocument {
    pub path: String,
    pub content: String,
    pub hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCheck {
    pub ok: bool,
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `POST /api/bots` body. Validation (name 1..=100 trimmed, `#rrggbb` color, instructions <= 30000)
/// happens in the store.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BotInput {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<AvatarShape>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
}

/// `PATCH /api/bots/:id` body.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BotPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<AvatarShape>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillInput {
    pub name: String,
    pub description: String,
    pub instructions: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// `workspace__send_message` arguments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BotMessageInput {
    pub to_bot_id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// Kind of model-initiated action a permission rule governs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionCategory {
    /// `workspace__write_file`
    FileWrite,
    /// `workspace__send_message`
    BotMessage,
    /// `workspace__create_bot`
    BotCreate,
    /// `workspace__create_skill`
    SkillSave,
    /// Bot memory saves.
    MemorySave,
}

impl PermissionCategory {
    pub const ALL: [Self; 5] = [Self::FileWrite, Self::BotMessage, Self::BotCreate, Self::SkillSave, Self::MemorySave];
}

/// "항상 묻기" (`ask`, the default without rules) or "묻지 않고 허용" (`allow`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAction {
    Ask,
    Allow,
}

/// A user rule: when a bot (all bots when `bot_id` is absent) attempts `category`, do `action`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionRule {
    pub id: String,
    pub category: PermissionCategory,
    pub action: PermissionAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// `POST /api/permission-rules` body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionRuleInput {
    pub category: PermissionCategory,
    pub action: PermissionAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_id: Option<String>,
}

/// `PATCH /api/permission-rules/:id` body ("규칙의 동작 편집").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionRulePatch {
    pub action: PermissionAction,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_and_status_shapes_match_typescript() {
        let schedule: RoutineSchedule = serde_json::from_str(r#"{"kind":"daily","time":"09:30","timezone":"Asia/Seoul"}"#).unwrap();
        assert_eq!(schedule, RoutineSchedule::Daily { time: "09:30".into(), timezone: "Asia/Seoul".into() });
        assert_eq!(serde_json::to_string(&RunStatus::AwaitingPermission).unwrap(), r#""awaiting_permission""#);
        assert_eq!(serde_json::to_string(&ToolStatus::InProgress).unwrap(), r#""in_progress""#);
        let entry = FileEntry { name: "a".into(), path: "a".into(), entry_type: FileEntryType::File };
        assert_eq!(serde_json::to_string(&entry).unwrap(), r#"{"name":"a","path":"a","type":"file"}"#);
        assert!(serde_json::from_str::<RoutineSchedule>(r#"{"kind":"interval","minutes":5,"extra":1}"#).is_err());
        assert_eq!(iso(DateTime::from_timestamp_millis(1_793_526_000_123).unwrap()), "2026-11-01T09:40:00.123Z");
    }
}
