//! Durable application state. Port of `server/store.ts`, `server/skills.ts`,
//! `server/bot-messages.ts` (data rules), and `server/routines.ts` (schedule math).
//!
//! CONTRACT (owned by the store agent; signatures are fixed so other crates can build against them):
//! - `Store` serializes all mutations, persists `state.json` atomically, bumps `revision`, and
//!   broadcasts the redacted public snapshot after the write succeeds.
//! - `State` holds the in-memory data. Read through `disk()`. Every `&mut` accessor marks the state
//!   dirty; `Store::mutate` commits only when the closure returned `Ok` *and* the state is dirty.
//!   A closure that returns `Err` must not have mutated anything (validate first, then apply),
//!   exactly like the Node store.
//! - Commit runs the `changed()` projections first: routine results from conversation runs and
//!   bot delivery reconciliation.

pub mod schedule;
pub mod skills;
pub mod bot_messages;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use grok_bot_model::*;
use tokio::sync::broadcast;

pub const MAX_STATE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_ROUTINES: usize = 1000;
pub const MAX_ROUTINE_RUNS: usize = 10000;

/// Validate and canonicalize a provider base URL (`http`/`https`, no credentials, query, or
/// fragment; trailing slashes removed).
pub fn base_url(input: &str) -> HttpResult<String> {
    let _ = input;
    todo!("store agent")
}

pub fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

pub struct Store {
    _private: (),
}

impl Store {
    /// Load `data_dir/state.json` (validating references like the Node store) or create the default
    /// state with one `Grok` bot. Active runs from a previous process become `interrupted`.
    pub async fn open(data_dir: &Path, now: Clock) -> anyhow::Result<Arc<Store>> {
        let _ = (data_dir, now);
        todo!("store agent")
    }

    pub fn data_dir(&self) -> &Path {
        todo!("store agent")
    }

    pub fn now(&self) -> DateTime<Utc> {
        todo!("store agent")
    }

    pub fn read<T>(&self, f: impl FnOnce(&State) -> T) -> T {
        let _ = f;
        todo!("store agent")
    }

    /// Apply `f` under the store lock. On `Ok` with a dirty state: run projections, bump revision,
    /// write atomically, then notify subscribers; resolves after the write. Write failures put the
    /// store into the failed state (`assert_writable` then returns 503) and mark the runtime
    /// unavailable, like the Node store.
    pub async fn mutate<T>(&self, f: impl FnOnce(&mut State) -> HttpResult<T>) -> HttpResult<T> {
        let _ = f;
        todo!("store agent")
    }

    /// Redacted public snapshot: API key removed and every occurrence replaced with `[redacted]`
    /// except in identifier fields (see `snapshot()` in store.ts).
    pub fn snapshot(&self) -> AppState {
        todo!("store agent")
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<AppState>> {
        todo!("store agent")
    }

    /// Wait for pending writes; error if persistence failed.
    pub async fn flush(&self) -> HttpResult<()> {
        todo!("store agent")
    }
}

pub struct State {
    _private: (),
}

impl State {
    pub fn disk(&self) -> &DiskState {
        todo!("store agent")
    }
    /// Marks the state dirty.
    pub fn disk_mut(&mut self) -> &mut DiskState {
        todo!("store agent")
    }
    pub fn data_dir(&self) -> &Path {
        todo!("store agent")
    }
    pub fn now(&self) -> DateTime<Utc> {
        todo!("store agent")
    }
    pub fn runtime(&self) -> &RuntimeInfo {
        todo!("store agent")
    }
    /// Marks the state dirty.
    pub fn set_runtime(&mut self, runtime: RuntimeInfo) {
        let _ = runtime;
        todo!("store agent")
    }
    pub fn settings(&self) -> &PrivateSettings {
        todo!("store agent")
    }
    /// `[redacted]` replacement for the stored API key.
    pub fn redact(&self, value: &str) -> String {
        let _ = value;
        todo!("store agent")
    }

    pub fn assert_writable(&self) -> HttpResult<()> {
        todo!("store agent")
    }
    /// 409 "Stop the active run before changing settings, bots, or files." when any run is active.
    pub fn assert_idle(&self) -> HttpResult<()> {
        todo!("store agent")
    }
    pub fn active(&self) -> Option<&Conversation> {
        todo!("store agent")
    }
    pub fn conversation(&self, id: &str) -> HttpResult<&Conversation> {
        let _ = id;
        todo!("store agent")
    }
    /// Marks the state dirty.
    pub fn conversation_mut(&mut self, id: &str) -> HttpResult<&mut Conversation> {
        let _ = id;
        todo!("store agent")
    }
    pub fn bot(&self, id: &str) -> HttpResult<&Bot> {
        let _ = id;
        todo!("store agent")
    }
    pub fn routine(&self, id: &str) -> HttpResult<&Routine> {
        let _ = id;
        todo!("store agent")
    }
    pub fn skill(&self, bot_id: &str, id: &str) -> HttpResult<&Skill> {
        let _ = (bot_id, id);
        todo!("store agent")
    }
    pub fn is_reserved_conversation(&self, id: &str) -> bool {
        let _ = id;
        todo!("store agent")
    }

    pub fn update_settings(&mut self, input: SettingsInput) -> HttpResult<()> {
        let _ = input;
        todo!("store agent")
    }
    pub fn add_bot(&mut self, input: BotInput) -> HttpResult<Bot> {
        let _ = input;
        todo!("store agent")
    }
    pub fn update_bot(&mut self, id: &str, input: BotPatch) -> HttpResult<()> {
        let _ = (id, input);
        todo!("store agent")
    }
    pub fn add_conversation(&mut self, bot_id: &str) -> HttpResult<Conversation> {
        let _ = bot_id;
        todo!("store agent")
    }
    /// `within_run` allows a model-proposed skill while a run is active.
    pub fn add_skill(&mut self, bot_id: &str, input: SkillInput, within_run: bool) -> HttpResult<Skill> {
        let _ = (bot_id, input, within_run);
        todo!("store agent")
    }
    pub fn update_skill(&mut self, bot_id: &str, id: &str, input: SkillPatch) -> HttpResult<()> {
        let _ = (bot_id, id, input);
        todo!("store agent")
    }
    pub fn delete_skill(&mut self, bot_id: &str, id: &str, revision: u64) -> HttpResult<()> {
        let _ = (bot_id, id, revision);
        todo!("store agent")
    }
    pub fn add_routine(&mut self, input: RoutineInput) -> HttpResult<Routine> {
        let _ = input;
        todo!("store agent")
    }
    pub fn update_routine(&mut self, id: &str, input: RoutineInput) -> HttpResult<()> {
        let _ = (id, input);
        todo!("store agent")
    }
    pub fn set_routine_enabled(&mut self, id: &str, enabled: bool) -> HttpResult<()> {
        let _ = (id, enabled);
        todo!("store agent")
    }
    pub fn delete_routine(&mut self, id: &str) -> HttpResult<()> {
        let _ = id;
        todo!("store agent")
    }
    pub fn run_routine(&mut self, id: &str, now: DateTime<Utc>) -> HttpResult<RoutineRun> {
        let _ = (id, now);
        todo!("store agent")
    }
    pub fn claim_due_routines(&mut self, now: DateTime<Utc>) -> HttpResult<()> {
        let _ = now;
        todo!("store agent")
    }
    pub fn routine_scheduling_error(&self) -> Option<&str> {
        todo!("store agent")
    }
    /// Marks dirty only when the value changes.
    pub fn set_routine_scheduling_error(&mut self, error: Option<String>) {
        let _ = error;
        todo!("store agent")
    }
    /// Returns `true` when queued routine or delivery work was cancelled.
    pub fn cancel_queued_work(&mut self, conversation_id: &str) -> HttpResult<bool> {
        let _ = conversation_id;
        todo!("store agent")
    }
    pub fn fail_routine_run(&mut self, run_id: &str, error: &str, now: DateTime<Utc>) -> HttpResult<()> {
        let _ = (run_id, error, now);
        todo!("store agent")
    }
    pub fn queue_bot_message(
        &mut self,
        conversation_id: &str,
        execution_run_id: &str,
        source_message_id: &str,
        input: BotMessageInput,
    ) -> HttpResult<BotDelivery> {
        let _ = (conversation_id, execution_run_id, source_message_id, input);
        todo!("store agent")
    }
    pub fn fail_bot_delivery(&mut self, delivery_id: &str, error: &str, now: DateTime<Utc>) -> HttpResult<()> {
        let _ = (delivery_id, error, now);
        todo!("store agent")
    }
}

#[allow(dead_code)]
fn _unused(_: PathBuf) {}
