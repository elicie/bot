//! Per-bot activity state and bot deletion (`setAgentUnread`, `setAgentNotifyOnUpdates`,
//! `deleteAgent` in Grok Bot).
//!
//! Read state is stored on the bot in `state.json`, so it is shared by every browser connected to
//! this server and survives reloads and restarts. The server marks a bot unread when an assistant
//! answer finishes in one of its conversations; the browser that is showing that bot marks it read
//! again (see `web/activity.ts`).
//!
//! Deleting a bot removes, in one commit:
//! - the bot itself and its private skills,
//! - all of its conversations (and, after the commit, their attachment blobs and engine sessions),
//! - its routines and the routine history recorded for its conversations,
//! - every bot message delivery and pair exchange it sent or received, the deliveries chained from
//!   those (they cannot stay valid without their parent, including messages sent from the report
//!   turn of a removed delivery), and the read-only recipient conversations those deliveries
//!   created for other bots; report records of removed deliveries are dropped with them,
//! - links from surviving source messages to removed exchanges, and removed delivery IDs from
//!   surviving report messages (the message stays marked as a report).
//!
//! Deleting the main bot is allowed: the first remaining bot becomes the main bot
//! (`crate::main_bot`).
//!
//! Refused while this bot is active, while active or queued work depends on the affected history,
//! and for the last remaining bot. Unrelated bots may keep running.

use std::collections::HashSet;

use grok_bot_model::{HttpError, HttpResult, WorkKind};

use crate::State;
use crate::bot_messages::chain_parent;

const LAST_BOT: &str = "Keep at least one bot. Add another bot before deleting this one.";
const QUEUED_WORK: &str = "Wait for or cancel this bot’s queued work before deleting it.";
const QUEUED_ROUTINES: &str = "Wait for or cancel this bot’s queued routine runs before deleting it.";
const QUEUED_DELIVERIES: &str = "Wait for or cancel the queued bot messages of this bot before deleting it.";

impl State {
    /// Mark a bot read or unread. Allowed during runs; a no-op write is skipped.
    pub fn set_bot_unread(&mut self, id: &str, unread: bool) -> HttpResult<()> {
        self.assert_writable()?;
        let index = self.bot_index(id)?;
        if self.disk.bots[index].unread.unwrap_or(false) != unread {
            self.disk_mut().bots[index].unread = unread.then_some(true);
        }
        Ok(())
    }

    /// Turn browser notifications for this bot's finished runs on or off. Allowed during runs.
    pub fn set_bot_notify_on_updates(&mut self, id: &str, enabled: bool) -> HttpResult<()> {
        self.assert_writable()?;
        let index = self.bot_index(id)?;
        if self.disk.bots[index].notify_on_updates.unwrap_or(false) != enabled {
            self.disk_mut().bots[index].notify_on_updates = enabled.then_some(true);
        }
        Ok(())
    }

    /// An assistant answer finished in this conversation: its bot becomes unread.
    pub fn mark_conversation_unread(&mut self, conversation_id: &str) -> HttpResult<()> {
        let bot_id = self.conversation(conversation_id)?.bot_id.clone();
        self.set_bot_unread(&bot_id, true)
    }

    /// Delete a bot with its history (see the module documentation). Returns the IDs of the removed
    /// conversations so the caller can remove their files outside `state.json` after the commit.
    pub fn delete_bot(&mut self, id: &str) -> HttpResult<Vec<String>> {
        self.assert_bot_idle(id)?;
        let index = self.bot_index(id)?;
        if self.disk.bots.len() <= 1 {
            return Err(HttpError::conflict(LAST_BOT));
        }
        let disk = &self.disk;
        let owned: HashSet<&str> = disk.conversations.iter().filter(|c| c.bot_id == id).map(|c| c.id.as_str()).collect();
        let routine_ids: HashSet<&str> = disk.routines.iter().filter(|r| r.bot_id == id).map(|r| r.id.as_str()).collect();
        if disk.routine_runs.iter().any(|run| run.result.is_none() && (run.bot_id == id || routine_ids.contains(run.routine_id.as_str()))) {
            return Err(HttpError::conflict(QUEUED_ROUTINES));
        }
        // Deliveries sent or received by the bot, then everything chained from them.
        let mut deliveries: HashSet<&str> = HashSet::new();
        let mut conversations: HashSet<&str> = owned.clone();
        loop {
            let before = deliveries.len();
            for delivery in &disk.bot_deliveries {
                // A message sent from a report turn's answer is chained to the reported delivery.
                let report_child = || {
                    let source = disk.conversations.iter().find(|c| c.id == delivery.source_conversation_id);
                    source
                        .and_then(|source| chain_parent(&disk.bot_deliveries, source, &delivery.source_message_id))
                        .is_some_and(|parent| deliveries.contains(parent.id.as_str()))
                };
                if delivery.from_bot_id == id
                    || delivery.to_bot_id == id
                    || conversations.contains(delivery.source_conversation_id.as_str())
                    || conversations.contains(delivery.recipient_conversation_id.as_str())
                    || report_child()
                {
                    deliveries.insert(delivery.id.as_str());
                    conversations.insert(delivery.recipient_conversation_id.as_str());
                }
            }
            if deliveries.len() == before {
                break;
            }
        }
        if disk.bot_deliveries.iter().any(|d| {
            deliveries.contains(d.id.as_str())
                && (d.status.is_pending() || d.report.as_ref().is_some_and(|report| report.status == grok_bot_model::ReportStatus::Running))
        }) || disk.conversations.iter().any(|conversation| conversations.contains(conversation.id.as_str()) && conversation.run.status.is_active())
        {
            return Err(HttpError::conflict(QUEUED_DELIVERIES));
        }
        if self.queued_group_work_for_bot(id)
            || disk.bot_deliveries.iter().any(|delivery| {
                deliveries.contains(delivery.id.as_str())
                    && delivery.report.as_ref().is_some_and(|report| report.status == grok_bot_model::ReportStatus::Queued)
            })
            || disk.work_queue.iter().any(|work| {
                work.bot_id == id
                    || conversations.contains(work.conversation_id.as_str())
                    || matches!(&work.kind, WorkKind::Report { delivery_id } if deliveries.contains(delivery_id.as_str()))
            })
        {
            return Err(HttpError::conflict(QUEUED_WORK));
        }
        let exchanges: HashSet<String> = disk.bot_deliveries.iter().filter(|d| deliveries.contains(d.id.as_str())).map(|d| d.exchange_id.clone()).collect();
        let deliveries: HashSet<String> = deliveries.into_iter().map(str::to_owned).collect();
        let conversations: HashSet<String> = conversations.into_iter().map(str::to_owned).collect();

        let mut candidate = self.disk.clone();
        candidate.bots.remove(index);
        if candidate.main_bot_id.as_deref() == Some(id) {
            candidate.main_bot_id = None;
        }
        candidate.conversations.retain(|c| !conversations.contains(&c.id));
        for message in candidate.conversations.iter_mut().flat_map(|c| c.messages.iter_mut()) {
            if let Some(ids) = message.bot_exchange_ids.as_mut() {
                ids.retain(|exchange| !exchanges.contains(exchange));
                if ids.is_empty() {
                    message.bot_exchange_ids = None;
                }
            }
            // A surviving report message keeps its (possibly empty) marker so it stays hidden.
            if let Some(ids) = message.report_delivery_ids.as_mut() {
                ids.retain(|delivery| !deliveries.contains(delivery));
            }
        }
        candidate.routines.retain(|r| r.bot_id != id);
        candidate.routine_runs.retain(|run| run.bot_id != id && !conversations.contains(&run.conversation_id));
        candidate.bot_deliveries.retain(|d| !deliveries.contains(&d.id));
        candidate.bot_exchanges.retain(|e| !exchanges.contains(&e.id) && !e.bot_ids.iter().any(|bot| bot == id));
        // Rules scoped to this bot would otherwise linger as "삭제된 Bot".
        candidate.permission_rules.retain(|rule| rule.bot_id.as_deref() != Some(id));
        crate::vault::remove_bot(&mut candidate, id);
        // The same reference rules `state.json` loading applies; never commit a state that could not
        // be loaded again.
        crate::disk::references_valid(&candidate).map_err(|_| HttpError::conflict("This bot is still linked to other history and cannot be deleted."))?;
        *self.disk_mut() = candidate;
        // Groups keep their conversations; the deleted bot leaves their members and pending rounds.
        self.remove_bot_from_groups(id);
        let mut removed: Vec<String> = conversations.into_iter().collect();
        removed.sort();
        Ok(removed)
    }
}
