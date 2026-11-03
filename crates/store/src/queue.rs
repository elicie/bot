//! Durable model-work admission and FIFO projection.
//!
//! `work_queue` order, not its timestamps, is authoritative. Specialized work keeps its receipt in
//! the existing routine/delivery/group/report records. Reconciliation retains valid queue entries
//! in place and appends newly ready pointers; only that new tail is sorted by historical time to
//! make legacy migration deterministic. A started specialized record is never ready again.
//!
//! A later user request in a group may wait behind the current round. Dispatch must skip that
//! dependency-blocked User entry (without reserving its bot) until `group_round` is absent; otherwise
//! the member turn that completes the round could deadlock behind it.

use std::collections::{HashMap, HashSet};

use grok_bot_model::*;

use crate::State;
use crate::validate::{Issues, is_timestamp, is_uuid, js_len, js_trim, parse_ms};

fn user_payload_valid(text: &str, attachments: &[Attachment]) -> bool {
    js_len(text) <= 100000
        && (!js_trim(text).is_empty() || !attachments.is_empty())
        && crate::attachments::attachments_valid(attachments)
        && attachments.iter().map(|attachment| attachment.id.as_str()).collect::<HashSet<_>>().len() == attachments.len()
        && attachments.iter().filter(|attachment| attachment.kind == AttachmentKind::Text).map(|attachment| attachment.size).sum::<u64>()
            <= crate::attachments::MAX_TEXT_ATTACHMENTS_TOTAL_BYTES as u64
}

pub(crate) fn schema_valid(disk: &DiskState) -> bool {
    let mut ids = HashSet::new();
    disk.work_queue.iter().all(|work| {
        ids.insert(work.id.as_str())
            && [&work.id, &work.bot_id, &work.conversation_id].iter().all(|id| is_uuid(id))
            && work.speaker_bot_ids.as_ref().is_none_or(|ids| {
                matches!(work.kind, WorkKind::User { .. })
                    && !ids.is_empty()
                    && ids.len() <= crate::groups::MAX_GROUP_MEMBERS
                    && ids.iter().all(|id| is_uuid(id))
                    && ids.iter().collect::<HashSet<_>>().len() == ids.len()
                    && ids.first() == Some(&work.bot_id)
            })
            && is_timestamp(&work.created_at)
            && match &work.kind {
                WorkKind::User { text, attachments } => user_payload_valid(text, attachments),
                WorkKind::Routine { run_id } => is_uuid(run_id),
                WorkKind::Delivery { delivery_id } | WorkKind::Report { delivery_id } => is_uuid(delivery_id),
                WorkKind::Group { round_id } => is_uuid(round_id),
                WorkKind::Worker { worker_id } => is_uuid(worker_id),
                WorkKind::WorkerReport { batch_id } => is_uuid(batch_id),
            }
    })
}

/// Reject lost durable user work rather than quietly dropping it on load. Stale specialized
/// pointers are repairable projections, so reconciliation discards those instead.
pub(crate) fn references_valid(disk: &DiskState) -> bool {
    disk.work_queue.iter().all(|work| {
        let WorkKind::User { text, .. } = &work.kind else { return true };
        disk.conversations.iter().find(|conversation| conversation.id == work.conversation_id).is_some_and(|conversation| {
            (match (conversation.group_id.as_deref(), work.speaker_bot_ids.as_ref()) {
                (Some(group_id), Some(speakers)) => disk.groups.iter().find(|group| group.id == group_id).is_some_and(|group| {
                    speakers.first() == Some(&work.bot_id)
                        && speakers.iter().all(|id| group.member_bot_ids.contains(id) && disk.bots.iter().any(|bot| bot.id == *id))
                }),
                // Legacy group queue entries predate accepted speaker lists. Reconcile once using
                // current membership/mentions; never reinterpret a frozen list after migration.
                (Some(_), None) => disk.bots.iter().any(|bot| bot.id == work.bot_id) && user_owner(disk, conversation, text).is_some(),
                (None, None) => user_owner(disk, conversation, text).as_deref() == Some(work.bot_id.as_str()),
                (None, Some(_)) => false,
            }) && !disk.bot_deliveries.iter().any(|delivery| delivery.recipient_conversation_id == conversation.id)
                && !disk.worker_batches.iter().flat_map(|batch| &batch.workers).any(|worker| worker.conversation_id == conversation.id)
                && !disk.routine_runs.iter().any(|run| {
                    run.conversation_id == conversation.id && run.result.is_none() && !run.execution_run_id.as_deref().is_some_and(|id| !id.is_empty())
                })
        })
    })
}

/// Resolve the first actual speaker of a new user request. In particular, a mention can select a
/// later group member; assigning all group requests to the group's first member is incorrect.
fn user_speakers(disk: &DiskState, conversation: &Conversation, text: &str) -> Vec<String> {
    if let Some(group_id) = conversation.group_id.as_deref() {
        let Some(group) = disk.groups.iter().find(|group| group.id == group_id) else { return Vec::new() };
        let members: Vec<&Bot> = group.member_bot_ids.iter().filter_map(|id| disk.bots.iter().find(|bot| bot.id == *id)).collect();
        crate::groups::group_speakers(&members, text)
    } else {
        disk.bots.iter().filter(|bot| bot.id == conversation.bot_id).map(|bot| bot.id.clone()).collect()
    }
}

fn user_owner(disk: &DiskState, conversation: &Conversation, text: &str) -> Option<String> {
    user_speakers(disk, conversation, text).into_iter().next()
}

/// `(source kind, conversation, actual owner, source receipt)` identifies a projection, independently
/// of its queue-entry ID and historical timestamp.
type PointerKey = (u8, String, String, String);

fn pointer_key(work: &QueuedWork) -> Option<PointerKey> {
    let (kind, source) = match &work.kind {
        WorkKind::User { .. } => return None,
        WorkKind::Routine { run_id } => (0, run_id),
        WorkKind::Delivery { delivery_id } => (1, delivery_id),
        WorkKind::Group { round_id } => (2, round_id),
        WorkKind::Report { delivery_id } => (3, delivery_id),
        WorkKind::Worker { worker_id } => (4, worker_id),
        WorkKind::WorkerReport { batch_id } => (5, batch_id),
    };
    Some((kind, work.conversation_id.clone(), work.bot_id.clone(), source.clone()))
}

fn pointer(bot_id: &str, conversation_id: &str, created_at: &str, kind: WorkKind) -> QueuedWork {
    QueuedWork {
        id: String::new(),
        bot_id: bot_id.to_owned(),
        speaker_bot_ids: None,
        conversation_id: conversation_id.to_owned(),
        created_at: created_at.to_owned(),
        kind,
    }
}

fn queued(bot_id: &str, conversation_id: &str, created_at: &str, kind: WorkKind) -> QueuedWork {
    QueuedWork { id: new_id(), ..pointer(bot_id, conversation_id, created_at, kind) }
}

fn ready_pointers(disk: &DiskState, existing: &[QueuedWork]) -> Vec<QueuedWork> {
    let conversations: HashMap<&str, &Conversation> = disk.conversations.iter().map(|conversation| (conversation.id.as_str(), conversation)).collect();
    let bots: HashSet<&str> = disk.bots.iter().map(|bot| bot.id.as_str()).collect();
    let terminated: HashSet<&str> =
        disk.bot_deliveries.iter().filter(|delivery| delivery.chain_termination.is_some()).map(|delivery| delivery.chain_id.as_str()).collect();
    let mut ready = Vec::new();
    for run in &disk.routine_runs {
        if run.result.is_none()
            && !run.execution_run_id.as_deref().is_some_and(|id| !id.is_empty())
            && bots.contains(run.bot_id.as_str())
            && conversations
                .get(run.conversation_id.as_str())
                .is_some_and(|conversation| conversation.run.status == RunStatus::Idle && conversation.run.id.is_empty() && conversation.bot_id == run.bot_id)
        {
            ready.push(pointer(&run.bot_id, &run.conversation_id, &run.created_at, WorkKind::Routine { run_id: run.id.clone() }));
        }
    }
    let mut reports: HashMap<&str, Vec<&BotDelivery>> = HashMap::new();
    for delivery in &disk.bot_deliveries {
        if terminated.contains(delivery.chain_id.as_str()) {
            continue;
        }
        if delivery.status == DeliveryStatus::Queued
            && delivery.execution_run_id.is_none()
            && delivery.started_at.is_none()
            && bots.contains(delivery.to_bot_id.as_str())
            && conversations.get(delivery.recipient_conversation_id.as_str()).is_some_and(|conversation| {
                conversation.run.status == RunStatus::Idle && conversation.run.id.is_empty() && conversation.bot_id == delivery.to_bot_id
            })
        {
            ready.push(pointer(
                &delivery.to_bot_id,
                &delivery.recipient_conversation_id,
                &delivery.created_at,
                WorkKind::Delivery { delivery_id: delivery.id.clone() },
            ));
        }
        if delivery.status.is_terminal() && delivery.report.as_ref().is_some_and(|report| report.status == ReportStatus::Queued && report.run_id.is_none()) {
            reports.entry(&delivery.source_conversation_id).or_default().push(delivery);
        }
    }
    for conversation in &disk.conversations {
        // While a member is active, the next member is not ready yet and must not reserve a bot.
        // Stop/failure clears a round, but a server restart keeps accepted future members.
        if !matches!(conversation.run.status, RunStatus::Idle | RunStatus::Completed | RunStatus::Interrupted) {
            continue;
        }
        if let Some(round) = conversation.group_round.as_ref()
            && let Some(bot_id) = round.pending_bot_ids.first().filter(|id| bots.contains(id.as_str()))
        {
            ready.push(pointer(bot_id, &conversation.id, &round.created_at, WorkKind::Group { round_id: round.id.clone() }));
        }
    }
    // Keep the accepted report anchor even if a newer arrival carries an earlier wall clock.
    let mut anchors = HashMap::new();
    for work in existing {
        if let WorkKind::Report { delivery_id } = &work.kind {
            anchors.entry(work.conversation_id.as_str()).or_insert(delivery_id.as_str());
        }
    }
    for (conversation_id, reports) in reports {
        let Some(conversation) = conversations.get(conversation_id).filter(|conversation| bots.contains(conversation.bot_id.as_str())) else { continue };
        let anchor = anchors.get(conversation_id).and_then(|id| reports.iter().copied().find(|delivery| delivery.id == *id));
        let Some(first) =
            anchor.or_else(|| reports.iter().copied().min_by_key(|delivery| delivery.finished_at.as_deref().and_then(parse_ms).unwrap_or(i64::MAX)))
        else {
            continue;
        };
        ready.push(pointer(
            &conversation.bot_id,
            conversation_id,
            first.finished_at.as_deref().unwrap_or(&first.created_at),
            WorkKind::Report { delivery_id: first.id.clone() },
        ));
    }
    for batch in &disk.worker_batches {
        if batch.cancelled || batch.released_at.is_none() || !bots.contains(batch.bot_id.as_str()) {
            continue;
        }
        for worker in &batch.workers {
            if worker.status == DeliveryStatus::Queued
                && worker.execution_run_id.is_none()
                && worker.started_at.is_none()
                && conversations
                    .get(worker.conversation_id.as_str())
                    .is_some_and(|child| child.bot_id == batch.bot_id && child.run.status == RunStatus::Idle && child.run.id.is_empty())
            {
                ready.push(pointer(&batch.bot_id, &worker.conversation_id, &worker.created_at, WorkKind::Worker { worker_id: worker.id.clone() }));
            }
        }
        if batch.workers.iter().all(|worker| worker.status.is_terminal())
            && batch.report.as_ref().is_some_and(|report| report.status == ReportStatus::Queued && report.run_id.is_none())
            && conversations.get(batch.parent_conversation_id.as_str()).is_some_and(|parent| parent.bot_id == batch.bot_id)
        {
            let finished_at = batch
                .workers
                .iter()
                .filter_map(|worker| worker.finished_at.as_deref())
                .max_by_key(|timestamp| parse_ms(timestamp).unwrap_or(i64::MIN))
                .unwrap_or(&batch.created_at);
            ready.push(pointer(&batch.bot_id, &batch.parent_conversation_id, finished_at, WorkKind::WorkerReport { batch_id: batch.id.clone() }));
        }
    }
    let indices: HashMap<&str, usize> = disk.conversations.iter().enumerate().map(|(index, conversation)| (conversation.id.as_str(), index)).collect();
    ready.sort_by(|a, b| {
        parse_ms(&a.created_at)
            .unwrap_or(i64::MAX)
            .cmp(&parse_ms(&b.created_at).unwrap_or(i64::MAX))
            .then_with(|| indices.get(a.conversation_id.as_str()).cmp(&indices.get(b.conversation_id.as_str())))
            .then_with(|| pointer_key(a).cmp(&pointer_key(b)))
    });
    ready
}

impl State {
    /// The owner of a live turn, including legacy group runs without an explicit owner.
    pub fn run_bot_id<'a>(&self, conversation: &'a Conversation) -> &'a str {
        conversation
            .run
            .bot_id
            .as_deref()
            .or_else(|| {
                conversation.messages.iter().rev().find(|message| message.role == Role::Assistant).and_then(|message| message.speaker_bot_id.as_deref())
            })
            .unwrap_or(&conversation.bot_id)
    }

    pub fn active_for_bot(&self, bot_id: &str) -> Option<&Conversation> {
        self.disk.conversations.iter().find(|conversation| conversation.run.status.is_active() && self.run_bot_id(conversation) == bot_id)
    }

    pub fn active_bot_ids(&self) -> HashSet<&str> {
        self.disk.conversations.iter().filter(|conversation| conversation.run.status.is_active()).map(|conversation| self.run_bot_id(conversation)).collect()
    }

    /// WaitingChildren is a delivery aggregate, not an executing turn, and occupies no slot.
    pub fn active_count(&self) -> usize {
        self.disk.conversations.iter().filter(|conversation| conversation.run.status.is_active()).count()
    }

    /// Bot-local mutations block only on that bot's live turn, never on merely queued work.
    pub fn assert_bot_idle(&self, bot_id: &str) -> HttpResult<()> {
        self.assert_writable()?;
        if self.active_for_bot(bot_id).is_some() {
            return Err(HttpError::conflict("Stop this bot's active run before changing it."));
        }
        Ok(())
    }

    /// Slot eligibility only. Dispatch separately enforces FIFO and any group-round dependency.
    pub fn can_start(&self, bot_id: &str, conversation_id: &str) -> bool {
        !self.write_failed
            && self.disk.bots.iter().any(|bot| bot.id == bot_id)
            && self.active_count() < self.disk.settings.max_concurrent_runs()
            && self.active_for_bot(bot_id).is_none()
            && self.conversation(conversation_id).is_ok_and(|conversation| !conversation.run.status.is_active())
    }

    /// Accept a user request durably without starting a run, appending messages, or taking a slot.
    /// Attachment blobs must already exist; the caller owns writing/discarding those private blobs.
    pub fn enqueue_user_turn(&mut self, conversation_id: &str, text: &str, attachments: Vec<Attachment>) -> HttpResult<QueuedWork> {
        self.assert_writable()?;
        let text = js_trim(text).to_owned();
        let mut issues = Issues::new();
        issues.length("text", &text, if attachments.is_empty() { 1 } else { 0 }, 100000);
        if !user_payload_valid(&text, &attachments) {
            issues.push("attachments", "Invalid attachment metadata or message contents.");
        }
        issues.finish()?;
        let conversation = self.conversation(conversation_id)?;
        if self.is_reserved_conversation(conversation_id) {
            return Err(HttpError::conflict("This conversation is reserved for automatic work."));
        }
        if self.disk.settings.base_url.is_empty() || self.disk.settings.model.is_empty() {
            return Err(HttpError::bad_request("Configure a provider and model in settings first."));
        }
        let speakers = user_speakers(&self.disk, conversation, &text);
        let bot_id = speakers.first().ok_or_else(|| HttpError::bad_request("This group has no member bots. Add members before sending a message."))?;
        let mut work = queued(bot_id, conversation_id, &iso(self.now()), WorkKind::User { text, attachments });
        work.speaker_bot_ids = conversation.group_id.as_ref().map(|_| speakers);
        {
            let mut prospect = self.prospect();
            prospect.work_queue.push(&work);
            Self::check_capacity(&prospect)?;
        }
        // Include any specialized work accepted earlier in this same mutation before this user.
        self.sync_work_queue();
        self.disk_mut().work_queue.push(work.clone());
        Ok(work)
    }

    /// Retain accepted order; append newly ready specialized records. Returns whether it changed.
    /// Legacy group requests acquire a frozen speaker list once; admitted requests keep their lane.
    pub fn sync_work_queue(&mut self) -> bool {
        let mut queue = std::mem::take(&mut self.disk.work_queue);
        let ready = ready_pointers(&self.disk, &queue);
        let mut remaining: HashSet<PointerKey> = ready.iter().filter_map(pointer_key).collect();
        let mut ids = HashSet::new();
        let mut changed = false;
        queue.retain_mut(|work| {
            let valid = if !ids.insert(work.id.clone()) {
                false
            } else if let WorkKind::User { text, .. } = &work.kind {
                let conversation = self.disk.conversations.iter().find(|conversation| conversation.id == work.conversation_id);
                if let Some(conversation) = conversation.filter(|conversation| !self.is_reserved_conversation(&conversation.id)) {
                    if conversation.group_id.is_some() && work.speaker_bot_ids.is_none() {
                        let speakers = user_speakers(&self.disk, conversation, text);
                        if let Some(first) = speakers.first() {
                            work.bot_id = first.clone();
                            work.speaker_bot_ids = Some(speakers);
                            changed = true;
                        }
                    }
                    self.disk.bots.iter().any(|bot| bot.id == work.bot_id)
                } else {
                    false
                }
            } else {
                pointer_key(work).is_some_and(|key| remaining.remove(&key))
            };
            changed |= !valid;
            valid
        });
        for mut work in ready {
            if pointer_key(&work).is_some_and(|key| remaining.remove(&key)) {
                work.id = new_id();
                queue.push(work);
                changed = true;
            }
        }
        self.disk.work_queue = queue;
        self.dirty |= changed;
        changed
    }

    /// Remove a queue pointer during an atomic claim. The caller must mark specialized work as
    /// started/terminal in that same mutation, or reconciliation correctly reconstructs its pointer.
    pub fn remove_queued_work(&mut self, id: &str) -> Option<QueuedWork> {
        let index = self.disk.work_queue.iter().position(|work| work.id == id)?;
        self.dirty = true;
        Some(self.disk.work_queue.remove(index))
    }

    /// Cancel one unstarted user request; return its attachments for caller-side blob cleanup.
    pub fn cancel_user_turn(&mut self, conversation_id: &str, id: &str) -> HttpResult<QueuedWork> {
        self.assert_writable()?;
        self.conversation(conversation_id)?;
        if !self.disk.work_queue.iter().any(|work| work.id == id && work.conversation_id == conversation_id && matches!(work.kind, WorkKind::User { .. })) {
            return Err(HttpError::not_found("Queued user turn does not exist or has already started."));
        }
        Ok(self.remove_queued_work(id).expect("validated queued user turn"))
    }

    /// Cancel every unstarted user request for this conversation without touching a live turn.
    pub fn cancel_user_turns(&mut self, conversation_id: &str) -> HttpResult<Vec<QueuedWork>> {
        self.assert_writable()?;
        self.conversation(conversation_id)?;
        let mut removed = Vec::new();
        let mut retained = Vec::with_capacity(self.disk.work_queue.len());
        for work in std::mem::take(&mut self.disk.work_queue) {
            if work.conversation_id == conversation_id && matches!(work.kind, WorkKind::User { .. }) {
                removed.push(work);
            } else {
                retained.push(work);
            }
        }
        self.disk.work_queue = retained;
        self.dirty |= !removed.is_empty();
        Ok(removed)
    }

    /// Stop's local queued-work half. A live turn is stopped by Runtime; other conversations are
    /// never stopped here. Future results of still-pending outbound deliveries remain untouched.
    pub fn cancel_conversation_work(&mut self, conversation_id: &str) -> HttpResult<bool> {
        self.assert_writable()?;
        let conversation = self.conversation(conversation_id)?;
        let had_round = conversation.group_round.is_some();
        let active = conversation.run.status.is_active().then(|| (conversation.run.clone(), conversation.updated_at.clone()));
        let users = self.cancel_user_turns(conversation_id)?;
        let specialized = self.cancel_queued_work(conversation_id)?;
        let reports: Vec<String> = self
            .disk
            .bot_deliveries
            .iter()
            .filter(|delivery| {
                delivery.source_conversation_id == conversation_id && delivery.report.as_ref().is_some_and(|report| report.status == ReportStatus::Queued)
            })
            .map(|delivery| delivery.id.clone())
            .collect();
        let cancelled_reports = if reports.is_empty() { false } else { !self.cancel_bot_reports(conversation_id, &reports)?.is_empty() };
        if cancelled_reports && let Some((run, updated_at)) = active {
            // cancel_bot_reports settles a waiting recipient aggregate. Its currently executing
            // report turn must nevertheless keep its slot until Runtime confirms engine shutdown.
            let conversation = self.conversation_mut(conversation_id)?;
            conversation.run = run;
            conversation.updated_at = updated_at;
        }
        Ok(!users.is_empty() || had_round || specialized || cancelled_reports)
    }
}
