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
//!
//! Design: `std::sync::Mutex<State>` guards the data and is never held across `.await`. A commit
//! serializes `state.json` and the public snapshot under the lock and hands both to a dedicated
//! writer thread through an ordered channel, so writes land and subscribers are notified in
//! revision order. `mutate` resolves after its write is durable.
//!
//! Domain methods take the inputs the HTTP layer deserializes and perform the zod validation the
//! Node routes did (`field: message` errors, status 400) before any other check.

pub mod attachments;
mod bot_activity;
pub mod bot_messages;
pub mod canvas;
mod cron;
mod disk;
pub mod generated_images;
pub mod groups;
pub mod knowledge;
pub mod main_bot;
pub mod memory;
pub mod permissions;
mod redact;
pub mod schedule;
pub mod skills;
pub mod templates;
pub mod validate;
pub mod vault;
pub mod webhooks;

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};

use chrono::{DateTime, Utc};
use grok_bot_model::*;
use serde::Serialize;
use tokio::sync::{broadcast, oneshot};

use crate::bot_messages::{
    BOT_CHAIN_CAPACITY_ERROR, MAX_BOT_DELIVERIES, MAX_BOT_DELIVERIES_PER_CHAIN, MAX_BOT_HOPS, MAX_BOT_MESSAGE_BYTES, MAX_BOT_REPLY_CHARACTERS, chain_parent,
    find_existing_bot_delivery, reconcile_bot_deliveries, validate_bot_message_input,
};
use crate::schedule::{latest_routine_time, next_routine_time, validate_routine_input};
use crate::skills::{new_skill, next_skill, validate_skill_set};
use crate::validate::{Issues, internal, js_slice, js_trim, parse_ms};

pub const MAX_STATE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_ROUTINES: usize = 1000;
pub const MAX_ROUTINE_RUNS: usize = 10000;

// Successful delivery: assistant + exchange. Failed turn: assistant + run error. Each 8 KiB
// metadata allowance covers a bounded error receipt (1000 UTF-16 units), IDs and times; a finished
// delivery can also introduce a queued report, whose separate allowance must not grow the total.
const BOT_REPORT_METADATA_RESERVE: usize = 8192;
const BOT_TURN_TEXT_RESERVE: usize = 2 * MAX_BOT_REPLY_CHARACTERS * 6 + 2 * BOT_REPORT_METADATA_RESERVE;

pub const DEFAULT_BOT_COLOR: &str = "#8b5cf6";
const WRITE_FAILED: &str = "State could not be persisted. Restart after checking the data directory.";
const SAVE_FAILED_RUNTIME: &str = "Application state could not be saved. Check the data directory before continuing.";
const IDLE_REQUIRED: &str = "Stop the active run before changing settings, bots, or files.";
const LOAD_FAILED: &str = "Cannot load application state. The existing state.json was not replaced.";

/// Validate and canonicalize a provider base URL (`http`/`https`, no credentials, query, or
/// fragment; trailing slashes removed).
pub fn base_url(input: &str) -> HttpResult<String> {
    let url = url::Url::parse(input).map_err(|_| HttpError::bad_request("Base URL must be an absolute HTTP or HTTPS URL."))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some_and(|password| !password.is_empty())
        || url.query().is_some_and(|query| !query.is_empty())
        || url.fragment().is_some_and(|fragment| !fragment.is_empty())
    {
        return Err(HttpError::bad_request("Base URL must use HTTP or HTTPS without credentials, query parameters, or fragments."));
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

pub fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// `new URL(value).hostname` (IPv6 hosts keep their brackets).
fn url_hostname(value: &str) -> Option<String> {
    url::Url::parse(value).ok()?.host_str().map(str::to_owned)
}

fn url_origin(value: &str) -> Option<String> {
    Some(url::Url::parse(value).ok()?.origin().ascii_serialization())
}

enum Job {
    /// A committed revision: the `state.json` bytes plus the non-persisted parts of the snapshot.
    Write {
        bytes: Vec<u8>,
        runtime: RuntimeInfo,
        routine_scheduling_error: Option<String>,
        /// The bytes contain bot report fields, which binaries without reports cannot load.
        reports: bool,
        /// `waiting_children`/`chainTermination` and recipient reports cannot be read by PR9.
        bot_chains: bool,
        /// Older report-aware binaries require a run ID on cancelled reports.
        unstarted_report_cancellations: bool,
        done: Option<oneshot::Sender<bool>>,
    },
    Flush(oneshot::Sender<()>),
}

/// One-time copy of a `state.json` from before bot reports, kept next to it.
pub const PRE_REPORTS_BACKUP: &str = "state.json.pre-bot-reports.bak";
/// Separate rollback point for the upgrade from bot reports to nested bot chains.
pub const PRE_BOT_CHAINS_BACKUP: &str = "state.json.pre-bot-chains.bak";
/// One-time copy before a cancelled report without a run ID is first persisted. Report-aware older
/// binaries cannot load that shape; the pre-reports backup may not exist on an upgraded install.
pub const PRE_UNSTARTED_REPORT_CANCELLATION_BACKUP: &str = "state.json.pre-unstarted-report-cancellation.bak";

fn uses_bot_chains(disk: &DiskState) -> bool {
    if disk.bot_deliveries.iter().any(|delivery| delivery.status == DeliveryStatus::WaitingChildren || delivery.chain_termination.is_some()) {
        return true;
    }
    // Recipient reports/transcripts are also incompatible with PR9/PR24, even if their owning
    // delivery became terminal before a waiting state or root latch was ever written.
    let recipients: HashSet<&str> = disk.bot_deliveries.iter().map(|delivery| delivery.recipient_conversation_id.as_str()).collect();
    disk.bot_deliveries.iter().any(|delivery| delivery.report.is_some() && recipients.contains(delivery.source_conversation_id.as_str()))
        || disk.conversations.iter().any(|conversation| recipients.contains(conversation.id.as_str()) && conversation.messages.len() > 2)
}

fn uses_reports(disk: &DiskState) -> bool {
    disk.bot_deliveries
        .iter()
        .any(|delivery| delivery.report.is_some() || delivery.status == DeliveryStatus::WaitingChildren || delivery.chain_termination.is_some())
        || disk.conversations.iter().flat_map(|conversation| &conversation.messages).any(|message| message.report_delivery_ids.is_some())
}

fn uses_unstarted_report_cancellations(disk: &DiskState) -> bool {
    disk.bot_deliveries
        .iter()
        .any(|delivery| delivery.report.as_ref().is_some_and(|report| report.status == ReportStatus::Cancelled && report.run_id.is_none()))
}

/// Called on the ordered writer thread before the first new-shape write. Keep the previous
/// `state.json` only when it does not already use that shape, with private permissions and no
/// overwrite. A restart never creates a misleading rollback copy of an already-new shape.
fn backup_before_shape(path: &Path, name: &str, uses_shape: fn(&DiskState) -> bool) -> std::io::Result<()> {
    let mut source = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    let meta = source.metadata()?;
    if !meta.is_file() || meta.nlink() != 1 || meta.len() > MAX_STATE_BYTES as u64 {
        return Err(std::io::Error::other("Invalid application state file."));
    }
    let mut existing = Vec::new();
    source.read_to_end(&mut existing)?;
    let mut disk = serde_json::from_slice::<DiskState>(&existing)?;
    if !disk::schema_valid(&mut disk) {
        return Err(std::io::Error::other("Application state does not match the expected schema."));
    }
    disk::references_valid(&disk).map_err(std::io::Error::other)?;
    if uses_shape(&disk) {
        return Ok(());
    }
    let backup = path.with_file_name(name);
    match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&backup) {
        Ok(file) => {
            let meta = file.metadata()?;
            if !meta.is_file() || meta.nlink() != 1 || meta.permissions().mode() & 0o077 != 0 || meta.len() > MAX_STATE_BYTES as u64 {
                return Err(std::io::Error::other("The state backup must be a private regular state file."));
            }
            // A previous crash may have left a truncated copy. Never overwrite it, but do not
            // treat it (or an already-incompatible copy) as a usable rollback backup either.
            // Parsing JSON alone is insufficient: this must also pass the real store loader's
            // schema and reference checks before it can make an incompatible write safe.
            let mut saved = serde_json::from_reader(file)?;
            if !disk::schema_valid(&mut saved) {
                return Err(std::io::Error::other("The state backup does not match the expected schema."));
            }
            disk::references_valid(&saved).map_err(std::io::Error::other)?;
            if uses_shape(&saved) {
                return Err(std::io::Error::other("The state backup already uses the incompatible shape."));
            }
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&backup)?;
    let written = {
        use std::io::Write as _;
        file.write_all(&existing).and_then(|()| file.sync_all())
    };
    if let Err(error) = written {
        // Only the file created above is removed; a partial copy is not a backup.
        let _ = std::fs::remove_file(&backup);
        return Err(error);
    }
    // Persist the new backup directory entry before state.json can be atomically replaced.
    std::fs::File::open(path.parent().ok_or_else(|| std::io::Error::other("State has no parent directory."))?)?.sync_all()
}

/// `Store.snapshot()`: the public state with the API key removed and redacted.
fn public_snapshot(disk: DiskState, runtime: RuntimeInfo, routine_scheduling_error: Option<String>, external_keys: &[String]) -> AppState {
    let mut keys = vault::redaction_keys(&disk);
    keys.extend(external_keys.iter().filter(|key| !key.is_empty()).cloned());
    keys.sort_by_key(|key| std::cmp::Reverse(key.len()));
    keys.dedup();
    let (secrets, mcp_servers) = vault::public_vault(&disk);
    let DiskState {
        revision,
        settings,
        bots,
        conversations,
        mut routines,
        routine_runs,
        bot_exchanges,
        bot_deliveries,
        permission_rules,
        groups,
        main_bot_id,
        ..
    } = disk;
    webhooks::strip_webhook_secrets(&mut routines);
    let mut state = AppState {
        revision,
        bots,
        conversations,
        routines,
        routine_runs,
        bot_exchanges,
        bot_deliveries,
        permission_rules,
        groups,
        routine_scheduling_error: routine_scheduling_error.filter(|value| !value.is_empty()),
        settings: PublicSettings {
            base_url: settings.base_url,
            model: settings.model,
            has_api_key: !settings.api_key.is_empty(),
            workspace: settings.workspace,
        },
        runtime,
        secrets,
        mcp_servers,
        main_bot_id,
    };
    redact::redact_state(&mut state, &keys);
    state
}

struct Shared {
    state: Mutex<State>,
    events: broadcast::Sender<Arc<AppState>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub struct Store {
    shared: Arc<Shared>,
    writer: mpsc::Sender<Job>,
    data_dir: PathBuf,
    clock: Clock,
}

fn writer_loop(shared: Arc<Shared>, path: PathBuf, jobs: mpsc::Receiver<Job>) {
    let mut reports_backed_up = false;
    let mut bot_chains_backed_up = false;
    let mut unstarted_cancellations_backed_up = false;
    for job in jobs {
        match job {
            Job::Flush(done) => {
                let _ = done.send(());
            }
            Job::Write { bytes, runtime, routine_scheduling_error, reports, bot_chains, unstarted_report_cancellations, done } => {
                if reports && !reports_backed_up {
                    if let Err(error) = backup_before_shape(&path, PRE_REPORTS_BACKUP, uses_reports) {
                        // Preserve the original pre-reports backup's best-effort behavior.
                        eprintln!("Could not keep a backup of state.json before bot reports: {error}");
                    }
                    reports_backed_up = true;
                }
                let done = |ok: bool| {
                    if let Some(done) = done {
                        let _ = done.send(ok);
                    }
                };
                if shared.lock().write_failed {
                    done(false);
                    continue;
                }
                let result = if bytes.len() > MAX_STATE_BYTES {
                    Err(std::io::Error::other("Application state exceeds the 64 MiB limit."))
                } else {
                    (|| {
                        // Each incompatible shape keeps its own immediately previous rollback
                        // point. Required backup failures leave compatible state.json untouched.
                        if bot_chains && !bot_chains_backed_up {
                            backup_before_shape(&path, PRE_BOT_CHAINS_BACKUP, uses_bot_chains)?;
                            bot_chains_backed_up = true;
                        }
                        if unstarted_report_cancellations && !unstarted_cancellations_backed_up {
                            backup_before_shape(&path, PRE_UNSTARTED_REPORT_CANCELLATION_BACKUP, uses_unstarted_report_cancellations)?;
                            unstarted_cancellations_backed_up = true;
                        }
                        grok_bot_files::atomic_private_file(&path, &bytes)
                    })()
                };
                match result {
                    Ok(()) => {
                        if shared.events.receiver_count() > 0 {
                            match serde_json::from_slice::<DiskState>(&bytes) {
                                Ok(disk) => {
                                    let keys = shared.lock().external_redaction_keys.clone();
                                    let _ = shared.events.send(Arc::new(public_snapshot(disk, runtime, routine_scheduling_error, &keys)));
                                }
                                Err(_) => debug_assert!(false, "committed state must deserialize"),
                            }
                        }
                        done(true);
                    }
                    Err(_) => {
                        let snapshot = {
                            let mut state = shared.lock();
                            state.write_failed = true;
                            state.runtime = RuntimeInfo {
                                available: false,
                                bot_message_chains: state.runtime.bot_message_chains,
                                version: state.runtime.version.clone(),
                                error: Some(SAVE_FAILED_RUNTIME.into()),
                            };
                            Arc::new(state.snapshot())
                        };
                        let _ = shared.events.send(snapshot);
                        done(false);
                    }
                }
            }
        }
    }
}

fn default_state(now: DateTime<Utc>) -> DiskState {
    DiskState {
        version: 1,
        revision: 0,
        settings: PrivateSettings::default(),
        bots: vec![Bot {
            id: new_id(),
            name: "Grok".into(),
            color: DEFAULT_BOT_COLOR.into(),
            instructions: String::new(),
            shape: None,
            skills: None,
            pinned: None,
            hidden: None,
            unread: None,
            notify_on_updates: None,
            created_at: iso(now),
            memories: None,
            knowledge: None,
        }],
        conversations: Vec::new(),
        routines: Vec::new(),
        routine_runs: Vec::new(),
        bot_exchanges: Vec::new(),
        bot_deliveries: Vec::new(),
        permission_rules: Vec::new(),
        groups: Vec::new(),
        secrets: Vec::new(),
        mcp_servers: Vec::new(),
        main_bot_id: None,
    }
}

/// Read and validate `state.json`. `Ok(None)` when it does not exist.
fn load(path: &Path) -> anyhow::Result<Option<DiskState>> {
    let mut file = match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(anyhow::Error::new(error).context(LOAD_FAILED)),
    };
    let result = (|| -> anyhow::Result<DiskState> {
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > MAX_STATE_BYTES as u64 {
            anyhow::bail!("Invalid application state file.");
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        let mut disk: DiskState = serde_json::from_str(&text)?;
        if !disk::schema_valid(&mut disk) {
            anyhow::bail!("Application state does not match the expected schema.");
        }
        disk::references_valid(&disk).map_err(anyhow::Error::msg)?;
        Ok(disk)
    })();
    result.map(Some).map_err(|error| error.context(LOAD_FAILED))
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("data_dir", &self.data_dir).finish_non_exhaustive()
    }
}

impl Store {
    /// Load `data_dir/state.json` (validating references like the Node store) or create the default
    /// state with one `Grok` bot. Active runs from a previous process become `interrupted`.
    pub async fn open(data_dir: &Path, now: Clock) -> anyhow::Result<Arc<Store>> {
        let input = data_dir.to_path_buf();
        let (root, disk) = tokio::task::spawn_blocking(move || -> anyhow::Result<(PathBuf, Option<DiskState>)> {
            let root = grok_bot_files::private_directory(&input).map_err(|error| anyhow::anyhow!(error.message))?;
            let disk = load(&root.join("state.json"))?;
            Ok((root, disk))
        })
        .await??;
        let current = now();
        let mut disk = disk.unwrap_or_else(|| default_state(current));
        for conversation in &mut disk.conversations {
            if !conversation.run.status.is_active() {
                continue;
            }
            conversation.run.status = RunStatus::Interrupted;
            conversation.run.error = Some("The server stopped before this run finished.".into());
            conversation.run.finished_at = Some(iso(now()));
            conversation.run.permission = None;
            for tool in conversation.messages.iter_mut().flat_map(|message| message.tools.iter_mut()) {
                if matches!(tool.status, ToolStatus::InProgress | ToolStatus::Pending) {
                    tool.status = ToolStatus::Failed;
                }
            }
        }
        let state = State {
            disk,
            data_dir: root.clone(),
            clock: now.clone(),
            runtime: RuntimeInfo { available: false, bot_message_chains: None, version: None, error: Some("Checking Grok Build.".into()) },
            routine_scheduling_error: None,
            external_redaction_keys: Vec::new(),
            dirty: false,
            write_failed: false,
        };
        let (events, _) = broadcast::channel(256);
        let shared = Arc::new(Shared { state: Mutex::new(state), events });
        let (sender, jobs) = mpsc::channel();
        let path = root.join("state.json");
        let writer_shared = shared.clone();
        std::thread::Builder::new().name("grok-bot-state-writer".into()).spawn(move || writer_loop(writer_shared, path, jobs))?;
        let store = Arc::new(Store { shared, writer: sender, data_dir: root, clock: now });
        store
            .mutate(|state| {
                state.dirty = true;
                Ok(())
            })
            .await
            .map_err(|_| anyhow::anyhow!(SAVE_FAILED_RUNTIME))?;
        store.prune_knowledge().await;
        Ok(store)
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    pub fn read<T>(&self, f: impl FnOnce(&State) -> T) -> T {
        f(&self.shared.lock())
    }

    fn enqueue(&self, job: Job) -> HttpResult<()> {
        self.writer.send(job).map_err(|_| internal())
    }

    /// Apply `f` under the store lock. On `Ok` with a dirty state: run projections, bump revision,
    /// write atomically, then notify subscribers; resolves after the write. Write failures put the
    /// store into the failed state (`assert_writable` then returns 503) and mark the runtime
    /// unavailable, like the Node store. The failed mutation itself resolves with the 500 response
    /// the Node error handler produced for persistence errors.
    pub async fn mutate<T>(&self, f: impl FnOnce(&mut State) -> HttpResult<T>) -> HttpResult<T> {
        let (value, done) = {
            let mut state = self.shared.lock();
            state.dirty = false;
            let result = f(&mut state);
            let dirty = std::mem::take(&mut state.dirty);
            let value = result?;
            if !dirty {
                return Ok(value);
            }
            let mut job = state.commit();
            let (done, wait) = oneshot::channel();
            if let Job::Write { done: slot, .. } = &mut job {
                *slot = Some(done);
            }
            // Enqueued under the state lock so writes stay in revision order.
            self.enqueue(job)?;
            (value, wait)
        };
        if done.await.unwrap_or(false) { Ok(value) } else { Err(internal()) }
    }

    /// Redacted public snapshot: API key removed and every occurrence replaced with `[redacted]`
    /// except in identifier fields (see `snapshot()` in store.ts).
    pub fn snapshot(&self) -> AppState {
        self.shared.lock().snapshot()
    }

    /// Register server-only credentials held outside DiskState (for example the encrypted OAuth
    /// vault). Values remain in memory only; callers retain retired values to scrub delayed echoes.
    /// The writer reads this list immediately before publishing each committed SSE snapshot.
    pub fn set_external_redaction_keys(&self, mut keys: Vec<String>) {
        keys.retain(|key| !key.is_empty());
        keys.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        keys.dedup();
        self.shared.lock().external_redaction_keys = keys;
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<AppState>> {
        self.shared.events.subscribe()
    }

    /// Wait for pending writes; error if persistence failed.
    pub async fn flush(&self) -> HttpResult<()> {
        let (done, wait) = oneshot::channel();
        self.enqueue(Job::Flush(done))?;
        let _ = wait.await;
        self.shared.lock().assert_writable()
    }
}

pub struct State {
    disk: DiskState,
    data_dir: PathBuf,
    clock: Clock,
    runtime: RuntimeInfo,
    routine_scheduling_error: Option<String>,
    /// Never serialized to state.json or exposed in public DTOs.
    external_redaction_keys: Vec<String>,
    dirty: bool,
    write_failed: bool,
}

/// Counts serialized bytes without allocating.
struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_bytes(value: &impl Serialize) -> usize {
    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, value).map_or(usize::MAX, |()| counter.0)
}

/// The prospective state `checkCapacity` measures, borrowing current and candidate entities.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Prospect<'a> {
    version: u8,
    revision: u64,
    settings: &'a PrivateSettings,
    bots: Vec<&'a Bot>,
    conversations: Vec<&'a Conversation>,
    routines: Vec<&'a Routine>,
    routine_runs: Vec<&'a RoutineRun>,
    bot_exchanges: Vec<&'a BotExchange>,
    bot_deliveries: Vec<&'a BotDelivery>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    permission_rules: &'a Vec<PermissionRule>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    groups: &'a Vec<Group>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    secrets: &'a Vec<VaultSecret>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    mcp_servers: &'a Vec<McpServerRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    main_bot_id: &'a Option<String>,
}

fn truthy(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(|text| !text.is_empty())
}

fn routine_result_status(status: RunStatus) -> Option<RoutineResultStatus> {
    match status {
        RunStatus::Completed => Some(RoutineResultStatus::Completed),
        RunStatus::Cancelled => Some(RoutineResultStatus::Cancelled),
        RunStatus::Error => Some(RoutineResultStatus::Error),
        RunStatus::Interrupted => Some(RoutineResultStatus::Interrupted),
        _ => None,
    }
}

fn bot_field_issues(issues: &mut Issues, name: Option<&mut String>, color: Option<&str>, instructions: Option<&str>) {
    if let Some(name) = name {
        *name = js_trim(name).to_owned();
        issues.length("name", name, 1, 100);
    }
    if let Some(color) = color {
        let valid = color.len() == 7 && color.starts_with('#') && color[1..].bytes().all(|b| b.is_ascii_hexdigit());
        issues.pattern("color", valid, r"/^#[\da-fA-F]{6}$/");
    }
    if let Some(instructions) = instructions {
        issues.max_length("instructions", instructions, 30000);
    }
}

fn clear_session(conversation: &mut Conversation) {
    conversation.session_id = None;
    conversation.session_fingerprint = None;
}

impl State {
    fn prospect(&self) -> Prospect<'_> {
        Prospect {
            version: self.disk.version,
            revision: self.disk.revision + 1,
            settings: &self.disk.settings,
            bots: self.disk.bots.iter().collect(),
            conversations: self.disk.conversations.iter().collect(),
            routines: self.disk.routines.iter().collect(),
            routine_runs: self.disk.routine_runs.iter().collect(),
            bot_exchanges: self.disk.bot_exchanges.iter().collect(),
            bot_deliveries: self.disk.bot_deliveries.iter().collect(),
            permission_rules: &self.disk.permission_rules,
            groups: &self.disk.groups,
            secrets: &self.disk.secrets,
            mcp_servers: &self.disk.mcp_servers,
            main_bot_id: &self.disk.main_bot_id,
        }
    }

    /// `checkCapacity`: counts, then the serialized prospective state plus bytes reserved for queued
    /// dispatch (prompt copies and bounded replies).
    fn check_capacity(prospect: &Prospect<'_>) -> HttpResult<()> {
        if prospect.routines.len() > MAX_ROUTINES {
            return Err(HttpError::conflict("The application supports at most 1000 routines."));
        }
        if prospect.routine_runs.len() > MAX_ROUTINE_RUNS {
            return Err(HttpError::conflict("Routine history is full. Existing history and conversations are retained."));
        }
        if prospect.bot_deliveries.len() > MAX_BOT_DELIVERIES {
            return Err(HttpError::conflict("Bot delivery history is full. Existing exchanges and conversations are retained."));
        }
        let queued: HashSet<&str> = prospect
            .conversations
            .iter()
            .filter(|conversation| conversation.run.status == RunStatus::Idle)
            .map(|conversation| conversation.id.as_str())
            .collect();
        let runs: usize = prospect
            .routine_runs
            .iter()
            .filter(|run| run.result.is_none() && !truthy(&run.execution_run_id) && queued.contains(run.conversation_id.as_str()))
            .map(|run| json_bytes(&run.prompt) + 4096)
            .sum();
        let deliveries: usize = prospect
            .bot_deliveries
            .iter()
            .map(|delivery| {
                // A public text is capped at 200k UTF-16 units, each at most six JSON bytes.
                // A recipient retains its assistant plus either the final exchange copy or a run
                // error. Keep this reserve while waiting: it also covers an internal report's
                // final answer copied to the inbound exchange. Later report prompts are checked
                // from their ACTUAL JSON bytes before starting, not speculatively reserved here.
                if delivery.status.is_pending() {
                    (if delivery.status == DeliveryStatus::Queued { json_bytes(&delivery.text) } else { 0 }) + BOT_TURN_TEXT_RESERVE
                } else if delivery.report.as_ref().is_some_and(|report| report.status == ReportStatus::Running) {
                    // A failed root report can retain both its assistant and its public run error.
                    BOT_TURN_TEXT_RESERVE
                } else if delivery.report.as_ref().is_some_and(|report| report.status == ReportStatus::Queued) {
                    // A failed start records an error receipt without starting another turn.
                    BOT_REPORT_METADATA_RESERVE
                } else {
                    0
                }
            })
            .sum();
        if json_bytes(prospect).saturating_add(runs).saturating_add(deliveries) > MAX_STATE_BYTES {
            return Err(HttpError::too_large(
                "Application state and reserved queued work would exceed the 64 MiB limit. Existing history and conversations are retained.",
            ));
        }
        Ok(())
    }

    /// Check an automatic report before any turn mutation. Its hidden input contains nested JSON:
    /// a 48 KiB raw answer plus an 8 KiB request can expand to about 400 KiB on disk, and legacy bot
    /// names/redaction can make metadata larger still. Measure the actual prompt rather than guess
    /// its size at delivery admission. Retain every other pending-work reserve, including the
    /// inbound delivery's final exchange copy, and reserve the report's bounded answer/error growth.
    pub fn check_bot_report_capacity(&self, conversation_id: &str, prompt: &str, delivery_ids: &[String]) -> HttpResult<()> {
        self.assert_writable()?;
        let source_index = self
            .disk
            .conversations
            .iter()
            .position(|conversation| conversation.id == conversation_id)
            .ok_or_else(|| HttpError::not_found("Conversation does not exist."))?;
        let ids: HashSet<&str> = delivery_ids.iter().map(String::as_str).collect();
        if ids.is_empty()
            || ids.len() != delivery_ids.len()
            || !ids.iter().all(|id| {
                self.disk.bot_deliveries.iter().any(|delivery| {
                    delivery.id == *id
                        && delivery.source_conversation_id == conversation_id
                        && delivery.report.as_ref().is_some_and(|report| report.status == ReportStatus::Queued)
                })
            })
        {
            return Err(HttpError::conflict("No bot reply is waiting for a follow-up in this conversation."));
        }
        let now = iso(self.now());
        let run_id = new_id();
        let mut conversation = self.disk.conversations[source_index].clone();
        for (role, text, report_delivery_ids) in [(Role::User, prompt, Some(delivery_ids.to_vec())), (Role::Assistant, "", None)] {
            conversation.messages.push(Message {
                id: new_id(),
                role,
                text: text.to_owned(),
                created_at: now.clone(),
                tools: Vec::new(),
                bot_exchange_ids: None,
                source_bot_id: None,
                attachments: None,
                speaker_bot_id: None,
                report_delivery_ids,
            });
        }
        conversation.updated_at = now.clone();
        conversation.run = RunState { id: run_id.clone(), status: RunStatus::Starting, started_at: Some(now), ..RunState::idle() };
        let claimed: Vec<_> = self
            .disk
            .bot_deliveries
            .iter()
            .filter(|delivery| delivery_ids.contains(&delivery.id))
            .map(|delivery| {
                let mut delivery = delivery.clone();
                delivery.report = Some(DeliveryReport { status: ReportStatus::Running, run_id: Some(run_id.clone()), ..DeliveryReport::queued() });
                delivery
            })
            .collect();
        let mut prospect = self.prospect();
        prospect.conversations[source_index] = &conversation;
        for delivery in &claimed {
            let index = self.disk.bot_deliveries.iter().position(|existing| existing.id == delivery.id).expect("claimed delivery exists");
            prospect.bot_deliveries[index] = delivery;
        }
        Self::check_capacity(&prospect)
    }

    /// `changed()` up to the write: projections, revision bump, serialization. The public snapshot
    /// for subscribers is rebuilt from these bytes on the writer thread, off the state lock.
    fn commit(&mut self) -> Job {
        let now = self.now();
        let DiskState { routine_runs, conversations, bot_deliveries, bot_exchanges, .. } = &mut self.disk;
        for routine_run in routine_runs.iter_mut() {
            if routine_run.result.is_some() {
                continue;
            }
            let Some(conversation) = conversations.iter().find(|item| item.id == routine_run.conversation_id) else { continue };
            let execution = &conversation.run;
            if truthy(&routine_run.execution_run_id) && routine_run.execution_run_id.as_deref() != Some(execution.id.as_str()) {
                continue;
            }
            if !execution.id.is_empty() {
                routine_run.execution_run_id = Some(execution.id.clone());
            }
            if let Some(status) = routine_result_status(execution.status) {
                routine_run.result = Some(RoutineResult {
                    status,
                    started_at: execution.started_at.clone().filter(|value| !value.is_empty()),
                    finished_at: execution.finished_at.clone().unwrap_or_else(|| iso(now)),
                    error: execution.error.clone().filter(|value| !value.is_empty()),
                });
            }
        }
        let routine_conversations: HashSet<&str> = routine_runs.iter().map(|run| run.conversation_id.as_str()).collect();
        reconcile_bot_deliveries(bot_deliveries, bot_exchanges, conversations, &routine_conversations, now);
        self.disk.revision += 1;
        let bytes = serde_json::to_vec(&self.disk).unwrap_or_default();
        Job::Write {
            bytes,
            runtime: self.runtime.clone(),
            routine_scheduling_error: self.routine_scheduling_error.clone(),
            reports: uses_reports(&self.disk),
            bot_chains: uses_bot_chains(&self.disk),
            unstarted_report_cancellations: uses_unstarted_report_cancellations(&self.disk),
            done: None,
        }
    }

    fn snapshot(&self) -> AppState {
        public_snapshot(self.disk.clone(), self.runtime.clone(), self.routine_scheduling_error.clone(), &self.external_redaction_keys)
    }

    pub fn disk(&self) -> &DiskState {
        &self.disk
    }
    /// Marks the state dirty.
    pub fn disk_mut(&mut self) -> &mut DiskState {
        self.dirty = true;
        &mut self.disk
    }
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
    pub fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }
    pub fn runtime(&self) -> &RuntimeInfo {
        &self.runtime
    }
    /// Marks the state dirty.
    pub fn set_runtime(&mut self, runtime: RuntimeInfo) {
        self.dirty = true;
        self.runtime = runtime;
    }
    pub fn settings(&self) -> &PrivateSettings {
        &self.disk.settings
    }
    /// Server-owned credentials stored outside DiskState, used at the provider boundary before
    /// the native engine can persist a response. This slice is never part of a public DTO.
    pub fn external_redaction_keys(&self) -> &[String] {
        &self.external_redaction_keys
    }

    /// `[redacted]` replacement for the stored API key and every vault secret value.
    pub fn redact(&self, value: &str) -> String {
        let mut value = value.to_owned();
        for key in self.redaction_keys() {
            if value.contains(key.as_str()) {
                value = value.replace(key.as_str(), "[redacted]");
            }
        }
        value
    }

    pub fn assert_writable(&self) -> HttpResult<()> {
        if self.write_failed {
            return Err(HttpError::unavailable(WRITE_FAILED));
        }
        Ok(())
    }
    /// 409 "Stop the active run before changing settings, bots, or files." when any run is active.
    pub fn assert_idle(&self) -> HttpResult<()> {
        self.assert_writable()?;
        if self.active().is_some() {
            return Err(HttpError::conflict(IDLE_REQUIRED));
        }
        Ok(())
    }
    pub fn active(&self) -> Option<&Conversation> {
        self.disk.conversations.iter().find(|conversation| conversation.run.status.is_active())
    }
    pub fn conversation(&self, id: &str) -> HttpResult<&Conversation> {
        self.disk.conversations.iter().find(|conversation| conversation.id == id).ok_or_else(|| HttpError::not_found("Conversation does not exist."))
    }
    /// Marks the state dirty.
    pub fn conversation_mut(&mut self, id: &str) -> HttpResult<&mut Conversation> {
        let conversation = self
            .disk
            .conversations
            .iter_mut()
            .find(|conversation| conversation.id == id)
            .ok_or_else(|| HttpError::not_found("Conversation does not exist."))?;
        self.dirty = true;
        Ok(conversation)
    }
    pub fn bot(&self, id: &str) -> HttpResult<&Bot> {
        self.disk.bots.iter().find(|bot| bot.id == id).ok_or_else(|| HttpError::not_found("Bot does not exist."))
    }
    fn bot_index(&self, id: &str) -> HttpResult<usize> {
        self.disk.bots.iter().position(|bot| bot.id == id).ok_or_else(|| HttpError::not_found("Bot does not exist."))
    }
    pub fn routine(&self, id: &str) -> HttpResult<&Routine> {
        self.disk.routines.iter().find(|routine| routine.id == id).ok_or_else(|| HttpError::not_found("Routine does not exist."))
    }
    fn routine_index(&self, id: &str) -> HttpResult<usize> {
        self.disk.routines.iter().position(|routine| routine.id == id).ok_or_else(|| HttpError::not_found("Routine does not exist."))
    }
    pub fn skill(&self, bot_id: &str, id: &str) -> HttpResult<&Skill> {
        self.bot(bot_id)?
            .skills
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find(|skill| skill.id == id)
            .ok_or_else(|| HttpError::not_found("Private skill does not exist for this bot."))
    }
    pub fn is_reserved_conversation(&self, id: &str) -> bool {
        self.disk.bot_deliveries.iter().any(|delivery| delivery.recipient_conversation_id == id)
            || self.disk.routine_runs.iter().any(|run| run.conversation_id == id && run.result.is_none() && !truthy(&run.execution_run_id))
    }

    pub fn update_settings(&mut self, mut input: SettingsInput) -> HttpResult<()> {
        let mut issues = Issues::new();
        issues.max_length("baseUrl", &input.base_url, 2048);
        input.model = js_trim(&input.model).to_owned();
        issues.length("model", &input.model, 1, 256);
        issues.max_length("workspace", &input.workspace, 4096);
        if let Some(api_key) = input.api_key.as_deref() {
            issues.max_length("apiKey", api_key, 16384);
            if api_key.contains(['\r', '\n', '\0']) {
                issues.push("apiKey", "API key contains invalid characters.");
            }
        }
        issues.finish()?;
        self.assert_idle()?;
        let canonical_url = base_url(js_trim(&input.base_url))?;
        let model = js_trim(&input.model);
        if model.is_empty() {
            return Err(HttpError::bad_request("Choose a model ID."));
        }
        let workspace = js_trim(&input.workspace);
        let workspace = if workspace.is_empty() { String::new() } else { grok_bot_files::canonical_directory(workspace)?.to_string_lossy().into_owned() };
        let data_dir = self.data_dir.to_string_lossy();
        if !workspace.is_empty()
            && (workspace == data_dir || workspace.starts_with(&format!("{data_dir}/")) || data_dir.starts_with(&format!("{workspace}/")) || workspace == "/")
        {
            return Err(HttpError::bad_request("Workspace must not contain the application data directory or be the filesystem root."));
        }
        let same_provider = !self.disk.settings.base_url.is_empty() && url_origin(&self.disk.settings.base_url) == url_origin(&canonical_url);
        let api_key = if input.clear_api_key == Some(true) {
            String::new()
        } else {
            match input.api_key.as_deref().map(js_trim).filter(|key| !key.is_empty()) {
                Some(key) => key.to_owned(),
                None if same_provider => self.disk.settings.api_key.clone(),
                None => String::new(),
            }
        };
        if api_key.is_empty() && !url_hostname(&canonical_url).is_some_and(|host| is_loopback_host(&host)) {
            return Err(HttpError::bad_request("An API key is required for non-loopback providers."));
        }
        let model = model.to_owned();
        let disk = self.disk_mut();
        disk.settings = PrivateSettings { base_url: canonical_url, model, workspace, api_key };
        disk.conversations.iter_mut().for_each(clear_session);
        Ok(())
    }

    pub fn add_bot(&mut self, input: BotInput) -> HttpResult<Bot> {
        self.insert_bot(input, false)
    }

    /// Add a bot proposed by the model during an active run, after the user approved it.
    pub fn add_bot_from_run(&mut self, input: BotInput) -> HttpResult<Bot> {
        self.insert_bot(input, true)
    }

    fn insert_bot(&mut self, mut input: BotInput, within_run: bool) -> HttpResult<Bot> {
        let mut issues = Issues::new();
        bot_field_issues(&mut issues, Some(&mut input.name), input.color.as_deref(), input.instructions.as_deref());
        issues.finish()?;
        if within_run {
            self.assert_writable()?;
        } else {
            self.assert_idle()?;
        }
        let bot = Bot {
            id: new_id(),
            name: input.name,
            color: input.color.unwrap_or_else(|| DEFAULT_BOT_COLOR.into()),
            instructions: input.instructions.unwrap_or_default(),
            shape: input.shape,
            skills: None,
            pinned: input.pinned,
            hidden: None,
            unread: None,
            notify_on_updates: None,
            created_at: iso(self.now()),
            memories: None,
            knowledge: None,
        };
        self.disk_mut().bots.push(bot.clone());
        Ok(bot)
    }

    pub fn update_bot(&mut self, id: &str, mut input: BotPatch) -> HttpResult<()> {
        let mut issues = Issues::new();
        bot_field_issues(&mut issues, input.name.as_mut(), input.color.as_deref(), input.instructions.as_deref());
        issues.finish()?;
        if input == BotPatch::default() {
            return Err(HttpError::bad_request("Provide a bot field to update."));
        }
        self.assert_idle()?;
        let index = self.bot_index(id)?;
        let disk = self.disk_mut();
        let bot = &mut disk.bots[index];
        let instructions_changed = input.instructions.as_ref().is_some_and(|instructions| *instructions != bot.instructions);
        if let Some(name) = input.name {
            bot.name = name;
        }
        if let Some(color) = input.color {
            bot.color = color;
        }
        if let Some(instructions) = input.instructions {
            bot.instructions = instructions;
        }
        if input.shape.is_some() {
            bot.shape = input.shape;
        }
        if input.pinned.is_some() {
            bot.pinned = input.pinned;
        }
        if input.hidden.is_some() {
            bot.hidden = input.hidden;
        }
        if instructions_changed {
            disk.conversations.iter_mut().filter(|conversation| conversation.bot_id == id).for_each(clear_session);
        }
        Ok(())
    }

    pub fn add_conversation(&mut self, bot_id: &str) -> HttpResult<Conversation> {
        let mut issues = Issues::new();
        issues.uuid("botId", bot_id);
        issues.finish()?;
        self.assert_writable()?;
        self.bot(bot_id)?;
        let conversation = Conversation {
            id: new_id(),
            bot_id: bot_id.to_owned(),
            title: "New conversation".into(),
            messages: Vec::new(),
            session_id: None,
            session_fingerprint: None,
            run: RunState::idle(),
            updated_at: iso(self.now()),
            group_id: None,
            thread: None,
            group_round: None,
            member_sessions: None,
            canvas: None,
        };
        self.disk_mut().conversations.push(conversation.clone());
        Ok(conversation)
    }

    /// Replace one bot's skills after the set and capacity checks.
    fn commit_skills(&mut self, index: usize, skills: Vec<Skill>) -> HttpResult<()> {
        validate_skill_set(&skills)?;
        let mut candidate = self.disk.bots[index].clone();
        candidate.skills = Some(skills);
        {
            let mut prospect = self.prospect();
            prospect.bots[index] = &candidate;
            Self::check_capacity(&prospect)?;
        }
        self.disk_mut().bots[index] = candidate;
        Ok(())
    }

    /// `within_run` allows a model-proposed skill while a run is active.
    pub fn add_skill(&mut self, bot_id: &str, input: SkillInput, within_run: bool) -> HttpResult<Skill> {
        let input = skills::validate_skill_input(input)?;
        if within_run {
            self.assert_writable()?
        } else {
            self.assert_idle()?
        }
        let index = self.bot_index(bot_id)?;
        let skill = new_skill(input, &iso(self.now()))?;
        let mut skills = self.disk.bots[index].skills.clone().unwrap_or_default();
        skills.push(skill.clone());
        self.commit_skills(index, skills)?;
        Ok(skill)
    }

    pub fn update_skill(&mut self, bot_id: &str, id: &str, input: SkillPatch) -> HttpResult<()> {
        let input = skills::validate_skill_patch(input)?;
        self.assert_idle()?;
        let index = self.bot_index(bot_id)?;
        let next = next_skill(self.skill(bot_id, id)?, input, &iso(self.now()))?;
        let skills =
            self.disk.bots[index].skills.as_deref().unwrap_or_default().iter().map(|skill| if skill.id == id { next.clone() } else { skill.clone() }).collect();
        self.commit_skills(index, skills)
    }

    pub fn delete_skill(&mut self, bot_id: &str, id: &str, revision: u64) -> HttpResult<()> {
        let mut issues = Issues::new();
        issues.number("revision", revision, 1, u64::MAX);
        issues.finish()?;
        self.assert_idle()?;
        if self.skill(bot_id, id)?.revision != revision {
            return Err(HttpError::conflict("The skill changed since it was opened. Reload before deleting."));
        }
        let index = self.bot_index(bot_id)?;
        if let Some(skills) = self.disk_mut().bots[index].skills.as_mut() {
            skills.retain(|skill| skill.id != id);
        }
        Ok(())
    }

    fn pending_routine(&self, id: &str) -> Option<&RoutineRun> {
        self.disk.routine_runs.iter().find(|run| {
            if run.routine_id != id || run.result.is_some() {
                return false;
            }
            let Ok(conversation) = self.conversation(&run.conversation_id) else { return false };
            let execution = &conversation.run;
            if truthy(&run.execution_run_id) && run.execution_run_id.as_deref() != Some(execution.id.as_str()) {
                return false;
            }
            execution.status == RunStatus::Idle || execution.status.is_active()
        })
    }

    fn assert_routine_idle(&self, id: &str) -> HttpResult<()> {
        self.assert_writable()?;
        self.routine(id)?;
        if self.pending_routine(id).is_some() {
            return Err(HttpError::conflict("Wait for or cancel this routine’s queued or active run first."));
        }
        Ok(())
    }

    /// `routineFields`: bot exists, a one-time schedule is in the future, `nextRunAt` when enabled.
    fn routine_fields(&self, input: &RoutineInput, now: DateTime<Utc>) -> HttpResult<Option<String>> {
        self.bot(&input.bot_id)?;
        if let RoutineSchedule::Once { at } = &input.schedule
            && parse_ms(at).is_none_or(|at| at <= now.timestamp_millis())
        {
            return Err(HttpError::bad_request("A one-time schedule must be in the future."));
        }
        if input.enabled { next_routine_time(&input.schedule, now, None).map(Some) } else { Ok(None) }
    }

    pub fn add_routine(&mut self, input: RoutineInput) -> HttpResult<Routine> {
        let input = validate_routine_input(input)?;
        self.assert_writable()?;
        let now = self.now();
        let next_run_at = self.routine_fields(&input, now)?;
        let routine = Routine {
            id: new_id(),
            bot_id: input.bot_id,
            name: input.name,
            prompt: input.prompt,
            schedule: input.schedule,
            enabled: input.enabled,
            next_run_at,
            created_at: iso(now),
            updated_at: iso(now),
            webhook: None,
        };
        let mut prospect = self.prospect();
        prospect.routines.push(&routine);
        Self::check_capacity(&prospect)?;
        self.disk_mut().routines.push(routine.clone());
        Ok(routine)
    }

    pub fn update_routine(&mut self, id: &str, input: RoutineInput) -> HttpResult<()> {
        let input = validate_routine_input(input)?;
        self.assert_routine_idle(id)?;
        let index = self.routine_index(id)?;
        let now = self.now();
        let next_run_at = self.routine_fields(&input, now)?;
        let current = &self.disk.routines[index];
        let updated = Routine {
            id: current.id.clone(),
            bot_id: input.bot_id,
            name: input.name,
            prompt: input.prompt,
            schedule: input.schedule,
            enabled: input.enabled,
            next_run_at: if input.enabled { next_run_at } else { None },
            created_at: current.created_at.clone(),
            updated_at: iso(now),
            webhook: current.webhook.clone(),
        };
        let mut prospect = self.prospect();
        prospect.routines[index] = &updated;
        Self::check_capacity(&prospect)?;
        self.disk_mut().routines[index] = updated;
        Ok(())
    }

    pub fn set_routine_enabled(&mut self, id: &str, enabled: bool) -> HttpResult<()> {
        self.assert_writable()?;
        let index = self.routine_index(id)?;
        let routine = &self.disk.routines[index];
        if routine.enabled == enabled {
            return Ok(());
        }
        let now = self.now();
        let mut updated = Routine { enabled, updated_at: iso(now), ..routine.clone() };
        if enabled {
            if let RoutineSchedule::Once { at } = &routine.schedule
                && parse_ms(at).is_none_or(|at| at <= now.timestamp_millis())
            {
                return Err(HttpError::bad_request("A one-time schedule must be in the future. Edit its time before enabling it."));
            }
            updated.next_run_at = Some(next_routine_time(&routine.schedule, now, None)?);
        } else {
            updated.next_run_at = None;
        }
        let mut prospect = self.prospect();
        prospect.routines[index] = &updated;
        Self::check_capacity(&prospect)?;
        self.disk_mut().routines[index] = updated;
        Ok(())
    }

    pub fn delete_routine(&mut self, id: &str) -> HttpResult<()> {
        self.assert_routine_idle(id)?;
        self.disk_mut().routines.retain(|routine| routine.id != id);
        Ok(())
    }

    fn routine_claim(routine: &Routine, trigger: RoutineTrigger, scheduled_for: String, now: DateTime<Utc>) -> (RoutineRun, Conversation) {
        let created_at = iso(now);
        let conversation = Conversation {
            id: new_id(),
            bot_id: routine.bot_id.clone(),
            title: routine.name.clone(),
            messages: Vec::new(),
            session_id: None,
            session_fingerprint: None,
            run: RunState::idle(),
            updated_at: created_at.clone(),
            group_id: None,
            thread: None,
            group_round: None,
            member_sessions: None,
            canvas: None,
        };
        let run = RoutineRun {
            id: new_id(),
            routine_id: routine.id.clone(),
            routine_name: routine.name.clone(),
            bot_id: routine.bot_id.clone(),
            conversation_id: conversation.id.clone(),
            prompt: routine.prompt.clone(),
            trigger,
            scheduled_for,
            created_at,
            execution_run_id: None,
            result: None,
        };
        (run, conversation)
    }

    pub fn run_routine(&mut self, id: &str, now: DateTime<Utc>) -> HttpResult<RoutineRun> {
        self.claim_routine_now(id, now, RoutineTrigger::Manual, None)
    }

    /// Claim an immediate run (manual or webhook) through the same admission checks. `prompt`
    /// replaces the routine prompt for this run only.
    fn claim_routine_now(&mut self, id: &str, now: DateTime<Utc>, trigger: RoutineTrigger, prompt: Option<String>) -> HttpResult<RoutineRun> {
        self.assert_routine_idle(id)?;
        let index = self.routine_index(id)?;
        let routine = &self.disk.routines[index];
        let (mut run, conversation) = Self::routine_claim(routine, trigger, iso(now), now);
        if let Some(prompt) = prompt {
            run.prompt = prompt;
        }
        let mut updated = routine.clone();
        // A manual run consumes a one-time schedule; a webhook run is independent of the schedule.
        if trigger == RoutineTrigger::Manual && matches!(routine.schedule, RoutineSchedule::Once { .. }) {
            updated.enabled = false;
            updated.next_run_at = None;
            updated.updated_at = iso(now);
        }
        let mut prospect = self.prospect();
        prospect.routines[index] = &updated;
        prospect.routine_runs.push(&run);
        prospect.conversations.push(&conversation);
        Self::check_capacity(&prospect)?;
        let disk = self.disk_mut();
        disk.routines[index] = updated;
        disk.routine_runs.push(run.clone());
        disk.conversations.push(conversation);
        Ok(run)
    }

    pub fn claim_due_routines(&mut self, now: DateTime<Utc>) -> HttpResult<()> {
        self.assert_writable()?;
        let at = now.timestamp_millis();
        let mut due: Vec<(usize, i64)> = self
            .disk
            .routines
            .iter()
            .enumerate()
            .filter(|(_, routine)| routine.enabled && truthy(&routine.next_run_at))
            .filter_map(|(index, routine)| parse_ms(routine.next_run_at.as_deref()?).filter(|next| *next <= at).map(|next| (index, next)))
            .collect();
        if due.is_empty() {
            return Ok(());
        }
        due.sort_by_key(|(_, next)| *next);
        let mut updates: HashMap<usize, Routine> = HashMap::new();
        let mut runs = Vec::new();
        let mut conversations = Vec::new();
        for (index, _) in due {
            let routine = &self.disk.routines[index];
            let next_run_at = routine.next_run_at.clone().unwrap_or_default();
            let pending = self.pending_routine(&routine.id).is_some();
            let previous = self.disk.routine_runs.iter().rev().find(|run| run.routine_id == routine.id);
            let covered = match previous {
                Some(previous) => {
                    let finished = match &previous.result {
                        Some(result) => Some(result.finished_at.as_str()),
                        None => self.conversation(&previous.conversation_id)?.run.finished_at.as_deref(),
                    }
                    .unwrap_or(previous.created_at.as_str());
                    parse_ms(finished).is_some_and(|finished| latest_routine_time(&routine.schedule, &next_run_at, now) <= finished)
                }
                None => false,
            };
            if !pending && !covered {
                let (run, conversation) = Self::routine_claim(routine, RoutineTrigger::Scheduled, next_run_at.clone(), now);
                runs.push(run);
                conversations.push(conversation);
            }
            let mut updated = Routine { updated_at: iso(now), ..routine.clone() };
            if matches!(routine.schedule, RoutineSchedule::Once { .. }) {
                updated.enabled = false;
                updated.next_run_at = None;
            } else {
                updated.next_run_at = Some(next_routine_time(&routine.schedule, now, Some(&next_run_at))?);
            }
            updates.insert(index, updated);
        }
        {
            let mut prospect = self.prospect();
            for (index, updated) in &updates {
                prospect.routines[*index] = updated;
            }
            prospect.routine_runs.extend(runs.iter());
            prospect.conversations.extend(conversations.iter());
            Self::check_capacity(&prospect)?;
        }
        let disk = self.disk_mut();
        for (index, updated) in updates {
            disk.routines[index] = updated;
        }
        disk.routine_runs.extend(runs);
        disk.conversations.extend(conversations);
        Ok(())
    }

    pub fn routine_scheduling_error(&self) -> Option<&str> {
        self.routine_scheduling_error.as_deref()
    }
    /// Marks dirty only when the value changes.
    pub fn set_routine_scheduling_error(&mut self, error: Option<String>) {
        if self.routine_scheduling_error == error {
            return;
        }
        self.routine_scheduling_error = error;
        self.dirty = true;
    }
    /// Returns `true` when queued routine or delivery work was cancelled.
    pub fn cancel_queued_work(&mut self, conversation_id: &str) -> HttpResult<bool> {
        self.assert_writable()?;
        if self.cancel_group_round(conversation_id)? {
            return Ok(true);
        }
        let conversation = self.conversation(conversation_id)?;
        let queued = self.disk.routine_runs.iter().any(|run| run.result.is_none() && !truthy(&run.execution_run_id) && run.conversation_id == conversation_id)
            || self
                .disk
                .bot_deliveries
                .iter()
                .any(|delivery| delivery.status == DeliveryStatus::Queued && delivery.recipient_conversation_id == conversation_id);
        if conversation.run.status != RunStatus::Idle || !queued {
            return Ok(false);
        }
        let timestamp = iso(self.now());
        let conversation = self.conversation_mut(conversation_id)?;
        conversation.run.status = RunStatus::Cancelled;
        conversation.run.finished_at = Some(timestamp.clone());
        conversation.updated_at = timestamp;
        Ok(true)
    }
    pub fn fail_routine_run(&mut self, run_id: &str, error: &str, now: DateTime<Utc>) -> HttpResult<()> {
        self.assert_writable()?;
        let run = self.disk.routine_runs.iter().find(|run| run.id == run_id).ok_or_else(|| HttpError::not_found("Routine run does not exist."))?;
        let (run_id, conversation_id) = (run.id.clone(), run.conversation_id.clone());
        let error = js_slice(&self.redact(error), 1000).to_owned();
        let timestamp = iso(now);
        let conversation = self.conversation_mut(&conversation_id)?;
        conversation.run = RunState {
            id: if conversation.run.id.is_empty() { run_id } else { conversation.run.id.clone() },
            status: RunStatus::Error,
            error: Some(error),
            permission: None,
            todos: conversation.run.todos.clone(),
            started_at: Some(conversation.run.started_at.clone().unwrap_or_else(|| timestamp.clone())),
            finished_at: Some(timestamp.clone()),
        };
        conversation.updated_at = timestamp;
        Ok(())
    }
    pub fn queue_bot_message(
        &mut self,
        conversation_id: &str,
        execution_run_id: &str,
        source_message_id: &str,
        input: BotMessageInput,
    ) -> HttpResult<BotDelivery> {
        self.assert_writable()?;
        let input = validate_bot_message_input(input)?;
        if input.text.len() > MAX_BOT_MESSAGE_BYTES {
            return Err(HttpError::too_large("Bot messages must be at most 160 KiB of UTF-8 text so the complete message can be reviewed."));
        }
        let source = self.conversation(conversation_id)?;
        if source.group_id.is_some() {
            return Err(HttpError::conflict("Bot messages are unavailable in group conversations."));
        }
        let message = source.messages.iter().rposition(|message| message.role == Role::Assistant);
        if source.run.id != execution_run_id
            || !source.run.status.is_active()
            || message.map(|index| source.messages[index].id.as_str()) != Some(source_message_id)
        {
            return Err(HttpError::conflict("The sending run is no longer active."));
        }
        let message = message.ok_or_else(internal)?;
        if let Some(existing) = find_existing_bot_delivery(&self.disk.bot_deliveries, source_message_id, &input)? {
            self.assert_bot_chain_open(&existing.chain_id, self.now())?;
            return Ok(existing.clone());
        }
        let recipient = self.bot(&input.to_bot_id)?;
        if recipient.id == source.bot_id {
            return Err(HttpError::bad_request("Choose another bot as the recipient."));
        }
        // A recipient conversation continues its delivery; the answer of a report turn continues the
        // reported delivery (so report turns cannot restart a chain at hop 1).
        let parent = chain_parent(&self.disk.bot_deliveries, source, source_message_id);
        if let Some(parent) = parent {
            self.assert_bot_chain_open(&parent.chain_id, self.now())?;
            if self.disk.bot_deliveries.iter().filter(|delivery| delivery.chain_id == parent.chain_id).count() >= MAX_BOT_DELIVERIES_PER_CHAIN {
                return Err(HttpError::conflict(BOT_CHAIN_CAPACITY_ERROR));
            }
        }
        let hop = parent.map_or(0, |parent| parent.hop) + 1;
        if hop > MAX_BOT_HOPS {
            return Err(HttpError::conflict("Bot message chains are limited to four hops."));
        }
        let timestamp = iso(self.now());
        let title = format!("Message from {}", self.bot(&source.bot_id)?.name);
        let recipient_conversation = Conversation {
            id: new_id(),
            bot_id: recipient.id.clone(),
            title: js_slice(&title, 100).to_owned(),
            messages: Vec::new(),
            session_id: None,
            session_fingerprint: None,
            run: RunState::idle(),
            updated_at: timestamp.clone(),
            group_id: None,
            thread: None,
            group_round: None,
            member_sessions: None,
            canvas: None,
        };
        let exchange = BotExchange {
            id: new_id(),
            bot_ids: [source.bot_id.clone(), recipient.id.clone()],
            messages: vec![BotExchangeMessage { id: new_id(), bot_id: source.bot_id.clone(), text: input.text.clone(), created_at: timestamp.clone() }],
            source_conversation_id: source.id.clone(),
            source_message_id: source_message_id.to_owned(),
            updated_at: timestamp.clone(),
        };
        let id = new_id();
        let delivery = BotDelivery {
            id: id.clone(),
            exchange_id: exchange.id.clone(),
            source_conversation_id: source.id.clone(),
            source_message_id: source_message_id.to_owned(),
            recipient_conversation_id: recipient_conversation.id.clone(),
            from_bot_id: source.bot_id.clone(),
            to_bot_id: recipient.id.clone(),
            text: input.text,
            chain_id: parent.map_or(id, |parent| parent.chain_id.clone()),
            hop,
            created_at: timestamp,
            status: DeliveryStatus::Queued,
            request_id: input.request_id.filter(|request_id| !request_id.is_empty()),
            execution_run_id: None,
            started_at: None,
            finished_at: None,
            error: None,
            report: None,
            chain_termination: None,
        };
        let source_index = self.disk.conversations.iter().position(|conversation| conversation.id == conversation_id).ok_or_else(internal)?;
        // Measure with the source message linked to the exchange, then undo on rejection.
        let previous_links = self.disk.conversations[source_index].messages[message].bot_exchange_ids.replace(vec![exchange.id.clone()]);
        let capacity = {
            let mut prospect = self.prospect();
            prospect.conversations.push(&recipient_conversation);
            prospect.bot_exchanges.push(&exchange);
            prospect.bot_deliveries.push(&delivery);
            Self::check_capacity(&prospect)
        };
        if let Err(error) = capacity {
            self.disk.conversations[source_index].messages[message].bot_exchange_ids = previous_links;
            return Err(error);
        }
        let disk = self.disk_mut();
        disk.conversations.push(recipient_conversation);
        disk.bot_exchanges.push(exchange);
        disk.bot_deliveries.push(delivery.clone());
        Ok(delivery)
    }
    pub fn fail_bot_delivery(&mut self, delivery_id: &str, error: &str, now: DateTime<Utc>) -> HttpResult<()> {
        self.assert_writable()?;
        let delivery =
            self.disk.bot_deliveries.iter().find(|delivery| delivery.id == delivery_id).ok_or_else(|| HttpError::not_found("Bot delivery does not exist."))?;
        // A scheduler claim may have lost a race to chain Stop/expiry or another dispatch. Only
        // a still-queued receipt can fail to start; never rewrite a settled or active recipient.
        if delivery.status != DeliveryStatus::Queued {
            return Ok(());
        }
        let conversation_id = delivery.recipient_conversation_id.clone();
        let error = js_slice(&self.redact(error), 1000).to_owned();
        let timestamp = iso(now);
        let conversation = self.conversation_mut(&conversation_id)?;
        conversation.run = RunState {
            id: conversation.run.id.clone(),
            status: RunStatus::Error,
            error: Some(error),
            permission: None,
            todos: conversation.run.todos.clone(),
            started_at: None,
            finished_at: Some(timestamp.clone()),
        };
        conversation.updated_at = timestamp;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_matches_whatwg_url() {
        assert_eq!(base_url("HTTP://LocalHost:80/a//").unwrap(), "http://localhost/a");
        assert_eq!(base_url("http://127.0.0.1:12345/v1/").unwrap(), "http://127.0.0.1:12345/v1");
        assert_eq!(base_url("https://example.com").unwrap(), "https://example.com");
        assert_eq!(base_url("http://a/b?").unwrap(), "http://a/b?");
        assert_eq!(base_url("nope").unwrap_err().message, "Base URL must be an absolute HTTP or HTTPS URL.");
        assert_eq!(base_url("ftp://a/").unwrap_err().status, 400);
        assert!(base_url("http://user@a/").is_err());
        assert!(base_url("http://a/?x=1").is_err());
        assert!(base_url("http://a/#x").is_err());
        assert_eq!(url_hostname("http://[::1]:8080/").as_deref(), Some("[::1]"));
        assert_eq!(url_origin("https://ex.com:443/v1/").as_deref(), Some("https://ex.com"));
    }
}
