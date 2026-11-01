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
    /// Files the user attached to this message (metadata; contents live in private blobs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Attachment>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentKind {
    Text,
    Image,
}

/// A user attachment stored with its message. The bytes are in
/// `<data dir>/attachments/<conversation id>/<attachment id>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub kind: AttachmentKind,
    pub size: u64,
}

/// One file in a message request: `data` is standard base64.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttachmentUpload {
    pub name: String,
    pub mime_type: String,
    pub data: String,
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
    /// An assistant answer finished in one of this bot's conversations and nobody opened it yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unread: Option<bool>,
    /// Show a browser notification when this bot's runs finish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_on_updates: Option<bool>,
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
