//! Application-managed, same-bot worker conversations. The durable batch records own their hidden
//! conversation and report-message links; ordinary conversations and messages keep their old shape.

use serde::{Deserialize, Serialize};

use crate::{DeliveryReport, DeliveryStatus};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerTask {
    pub id: String,
    pub conversation_id: String,
    pub task: String,
    pub created_at: String,
    pub status: DeliveryStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Bounded final answer only, never the worker's private tool transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerBatch {
    pub id: String,
    pub bot_id: String,
    pub parent_conversation_id: String,
    pub source_message_id: String,
    pub source_run_id: String,
    pub created_at: String,
    /// Set only after the spawning parent turn completed successfully.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_at: Option<String>,
    pub cancelled: bool,
    pub workers: Vec<WorkerTask>,
    /// Exactly one grouped follow-up after all siblings settle; it is never retried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<DeliveryReport>,
    /// App-written user input, omitted from every public snapshot but retained on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_input_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_message_id: Option<String>,
}
