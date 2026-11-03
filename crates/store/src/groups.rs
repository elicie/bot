//! Group chats and threads: data rules.
//!
//! A group lists 2–12 member bots in speaking order. A group conversation stores the group ID in
//! both `bot_id` and `group_id`, so code that resolves `bot_id` as a bot (routines, deliveries,
//! plain 1:1 runs) never mistakes it for one. Each user message opens a *round*: the members that
//! answer it, one turn at a time, through the speaking member bot's active-run slot.
//!
//! Speaker order: when the message mentions members as `@Name`, only those members answer;
//! otherwise every member answers. Either way they answer in member order, once per user message,
//! and member replies never trigger further member turns.
//!
//! A thread is a conversation linked to one message of a parent conversation. It inherits the
//! parent's `bot_id` and `group_id`, so a thread of a group conversation is answered by the group.
//! One message has at most one thread; threads cannot be nested.

use std::collections::HashSet;

use grok_bot_model::*;

use crate::State;
use crate::validate::{Issues, is_timestamp, is_uuid, js_len, js_trim};

pub const MAX_GROUPS: usize = 200;
pub const MIN_GROUP_MEMBERS: usize = 2;
pub const MAX_GROUP_MEMBERS: usize = 12;
pub const DEFAULT_GROUP_COLOR: &str = "#198aff";

const GROUP_BUSY: &str = "Wait for the group to finish answering or stop it first.";

fn color_valid(color: &str) -> bool {
    color.len() == 7 && color.starts_with('#') && color[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn field_issues(issues: &mut Issues, name: Option<&mut String>, color: Option<&str>, members: Option<&[String]>) {
    if let Some(name) = name {
        *name = js_trim(name).to_owned();
        issues.length("name", name, 1, 100);
    }
    if let Some(color) = color {
        issues.pattern("color", color_valid(color), r"/^#[\da-fA-F]{6}$/");
    }
    if let Some(members) = members {
        if !(MIN_GROUP_MEMBERS..=MAX_GROUP_MEMBERS).contains(&members.len()) {
            issues.push("memberBotIds", format!("Choose between {MIN_GROUP_MEMBERS} and {MAX_GROUP_MEMBERS} member bots."));
        }
        if members.iter().any(|id| !is_uuid(id)) {
            issues.push("memberBotIds", "Invalid UUID");
        }
        if members.iter().collect::<HashSet<_>>().len() != members.len() {
            issues.push("memberBotIds", "Each bot can be a member only once.");
        }
    }
}

/// Characters that may continue a name after `@Name` (so `@Al` does not match `@Alice`).
fn word_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// IDs of the members mentioned as `@Name` (case-insensitive, whole name, not inside a word such as
/// an email address), in member order.
pub fn mentioned_members(members: &[&Bot], text: &str) -> Vec<String> {
    let lowered = text.to_lowercase();
    members
        .iter()
        .filter(|bot| {
            let needle = format!("@{}", bot.name.to_lowercase());
            lowered.match_indices(&needle).any(|(index, _)| {
                lowered[..index].chars().next_back().is_none_or(|previous| !word_character(previous))
                    && lowered[index + needle.len()..].chars().next().is_none_or(|next| !word_character(next))
            })
        })
        .map(|bot| bot.id.clone())
        .collect()
}

/// The members that answer `text`, in speaking order: the mentioned members, or every member.
pub fn group_speakers(members: &[&Bot], text: &str) -> Vec<String> {
    let mentioned = mentioned_members(members, text);
    if mentioned.is_empty() { members.iter().map(|bot| bot.id.clone()).collect() } else { mentioned }
}

/// `state.json` checks for groups and the new conversation fields (formats only).
pub(crate) fn schema_valid(disk: &DiskState) -> bool {
    disk.groups.len() <= MAX_GROUPS
        && disk.groups.iter().all(|group| {
            is_uuid(&group.id)
                && (1..=100).contains(&js_len(&group.name))
                && color_valid(&group.color)
                && group.member_bot_ids.len() <= MAX_GROUP_MEMBERS
                && group.member_bot_ids.iter().all(|id| is_uuid(id))
                && is_timestamp(&group.created_at)
                && is_timestamp(&group.updated_at)
        })
        && disk.conversations.iter().all(|conversation| {
            conversation.group_id.as_deref().is_none_or(|id| is_uuid(id) && id == conversation.bot_id)
                && conversation.thread.as_ref().is_none_or(|link| is_uuid(&link.parent_conversation_id) && is_uuid(&link.parent_message_id))
                && conversation.group_round.as_ref().is_none_or(|round| {
                    is_uuid(&round.id)
                        && is_uuid(&round.user_message_id)
                        && round.pending_bot_ids.iter().all(|id| is_uuid(id))
                        && is_timestamp(&round.created_at)
                })
                && conversation.member_sessions.iter().flatten().all(|session| is_uuid(&session.bot_id))
                && conversation.messages.iter().all(|message| message.speaker_bot_id.as_deref().is_none_or(is_uuid))
        })
}

/// Load-time repair: drop members whose bot no longer exists (a bot deleted by an older build or
/// another feature keeps the group loadable). A crashed current turn is not replayed, but its
/// accepted pending members remain recoverable; only explicit Stop/failure discards the round.
pub(crate) fn normalize(disk: &mut DiskState) {
    let bots: HashSet<String> = disk.bots.iter().map(|bot| bot.id.clone()).collect();
    for group in &mut disk.groups {
        group.member_bot_ids.retain(|id| bots.contains(id));
    }
    for conversation in &mut disk.conversations {
        if matches!(conversation.run.status, RunStatus::Error | RunStatus::Cancelled) {
            conversation.group_round = None;
        }
        if let Some(round) = conversation.group_round.as_mut() {
            round.pending_bot_ids.retain(|id| bots.contains(id));
            if round.pending_bot_ids.is_empty() {
                conversation.group_round = None;
            }
        }
    }
}

/// Reference rules: group conversations name an existing group; group IDs are unique entities.
pub(crate) fn references_valid(disk: &DiskState) -> bool {
    let ids: HashSet<&str> = disk.groups.iter().map(|group| group.id.as_str()).collect();
    ids.len() == disk.groups.len()
        && !disk.bots.iter().any(|bot| ids.contains(bot.id.as_str()))
        && !disk.conversations.iter().any(|conversation| ids.contains(conversation.id.as_str()))
        && disk.conversations.iter().all(|conversation| conversation.group_id.as_deref().is_none_or(|id| ids.contains(id)))
}

/// Whether a conversation's `bot_id` names a group (and so may be absent from `bots`).
pub(crate) fn is_group_conversation(conversation: &Conversation) -> bool {
    conversation.group_id.as_deref() == Some(conversation.bot_id.as_str())
}

fn new_conversation(bot_id: String, group_id: Option<String>, thread: Option<ThreadLink>, title: &str, now: &str) -> Conversation {
    Conversation {
        id: new_id(),
        bot_id,
        title: title.into(),
        messages: Vec::new(),
        session_id: None,
        session_fingerprint: None,
        run: RunState::idle(),
        updated_at: now.to_owned(),
        group_id,
        thread,
        group_round: None,
        member_sessions: None,
        canvas: None,
    }
}

impl State {
    pub fn group(&self, id: &str) -> HttpResult<&Group> {
        self.disk.groups.iter().find(|group| group.id == id).ok_or_else(|| HttpError::not_found("Group does not exist."))
    }

    fn group_index(&self, id: &str) -> HttpResult<usize> {
        self.disk.groups.iter().position(|group| group.id == id).ok_or_else(|| HttpError::not_found("Group does not exist."))
    }

    /// Existing member bots of a group, in speaking order.
    pub fn group_members(&self, id: &str) -> HttpResult<Vec<&Bot>> {
        Ok(self.group(id)?.member_bot_ids.iter().filter_map(|member| self.disk.bots.iter().find(|bot| bot.id == *member)).collect())
    }

    fn assert_members_exist(&self, members: &[String]) -> HttpResult<()> {
        for member in members {
            self.bot(member)?;
        }
        Ok(())
    }

    /// Whether a group round in this conversation still has members to answer.
    pub fn group_round_pending(&self, conversation_id: &str) -> bool {
        self.conversation(conversation_id).is_ok_and(|conversation| conversation.group_round.as_ref().is_some_and(|round| !round.pending_bot_ids.is_empty()))
    }

    /// Accepted group work includes later member turns, not only the next member in the queue.
    /// Used by bot deletion; ordinary bot edits only need to wait for its live turn.
    pub(crate) fn queued_group_work_for_bot(&self, bot_id: &str) -> bool {
        self.disk
            .conversations
            .iter()
            .any(|conversation| conversation.group_round.as_ref().is_some_and(|round| round.pending_bot_ids.iter().any(|pending| pending == bot_id)))
            || self.disk.work_queue.iter().any(|work| {
                let WorkKind::User { text, .. } = &work.kind else { return false };
                if let Some(speakers) = &work.speaker_bot_ids {
                    return speakers.iter().any(|speaker| speaker == bot_id);
                }
                let Some(group_id) = self.conversation(&work.conversation_id).ok().and_then(|conversation| conversation.group_id.as_deref()) else {
                    return false;
                };
                self.group_members(group_id).is_ok_and(|members| group_speakers(&members, text).iter().any(|speaker| speaker == bot_id))
            })
    }

    fn assert_group_quiet(&self, group_id: &str) -> HttpResult<()> {
        if self.disk.conversations.iter().any(|conversation| {
            conversation.group_id.as_deref() == Some(group_id)
                && (conversation.run.status.is_active()
                    || conversation.group_round.is_some()
                    || self.disk.work_queue.iter().any(|work| work.conversation_id == conversation.id && matches!(work.kind, WorkKind::User { .. })))
        }) {
            return Err(HttpError::conflict(GROUP_BUSY));
        }
        Ok(())
    }

    pub fn add_group(&mut self, mut input: GroupInput) -> HttpResult<Group> {
        let mut issues = Issues::new();
        field_issues(&mut issues, Some(&mut input.name), input.color.as_deref(), Some(&input.member_bot_ids));
        issues.finish()?;
        self.assert_writable()?;
        if self.disk.groups.len() >= MAX_GROUPS {
            return Err(HttpError::conflict(format!("The application supports at most {MAX_GROUPS} groups.")));
        }
        self.assert_members_exist(&input.member_bot_ids)?;
        let now = iso(self.now());
        let group = Group {
            id: new_id(),
            name: input.name,
            color: input.color.unwrap_or_else(|| DEFAULT_GROUP_COLOR.into()),
            shape: input.shape,
            member_bot_ids: input.member_bot_ids,
            pinned: None,
            created_at: now.clone(),
            updated_at: now,
        };
        self.disk_mut().groups.push(group.clone());
        Ok(group)
    }

    pub fn update_group(&mut self, id: &str, mut input: GroupPatch) -> HttpResult<()> {
        let mut issues = Issues::new();
        field_issues(&mut issues, input.name.as_mut(), input.color.as_deref(), input.member_bot_ids.as_deref());
        issues.finish()?;
        if input == GroupPatch::default() {
            return Err(HttpError::bad_request("Provide a group field to update."));
        }
        self.assert_writable()?;
        let index = self.group_index(id)?;
        if let Some(members) = &input.member_bot_ids {
            self.assert_group_quiet(id)?;
            self.assert_members_exist(members)?;
        }
        let now = iso(self.now());
        let group = &mut self.disk_mut().groups[index];
        if let Some(name) = input.name {
            group.name = name;
        }
        if let Some(color) = input.color {
            group.color = color;
        }
        if input.shape.is_some() {
            group.shape = input.shape;
        }
        if let Some(members) = input.member_bot_ids {
            group.member_bot_ids = members;
        }
        if input.pinned.is_some() {
            group.pinned = input.pinned;
        }
        group.updated_at = now;
        Ok(())
    }

    /// Delete a group with its conversations and their threads. Member bots are kept. Returns the
    /// IDs of the removed conversations (threads included) so the caller can remove their files
    /// outside `state.json` after the commit.
    pub fn delete_group(&mut self, id: &str) -> HttpResult<Vec<String>> {
        self.assert_writable()?;
        self.group(id)?;
        self.assert_group_quiet(id)?;
        let disk = self.disk_mut();
        disk.groups.retain(|group| group.id != id);
        let group_conversations: HashSet<String> =
            disk.conversations.iter().filter(|conversation| conversation.group_id.as_deref() == Some(id)).map(|conversation| conversation.id.clone()).collect();
        let mut removed = Vec::new();
        disk.conversations.retain(|conversation| {
            let keep = !group_conversations.contains(&conversation.id)
                && conversation.thread.as_ref().is_none_or(|link| !group_conversations.contains(&link.parent_conversation_id));
            if !keep {
                removed.push(conversation.id.clone());
            }
            keep
        });
        Ok(removed)
    }

    /// Remove a bot from every group (for bot deletion). Groups keep their conversations; a group
    /// left with fewer than two members stays usable by its remaining member and can be edited.
    pub fn remove_bot_from_groups(&mut self, bot_id: &str) {
        if !self.disk.groups.iter().any(|group| group.member_bot_ids.iter().any(|id| id == bot_id))
            && !self.disk.conversations.iter().any(|c| c.group_round.as_ref().is_some_and(|round| round.pending_bot_ids.iter().any(|id| id == bot_id)))
        {
            return;
        }
        let disk = self.disk_mut();
        for group in &mut disk.groups {
            group.member_bot_ids.retain(|id| id != bot_id);
        }
        for conversation in &mut disk.conversations {
            if let Some(round) = conversation.group_round.as_mut() {
                round.pending_bot_ids.retain(|id| id != bot_id);
                if round.pending_bot_ids.is_empty() {
                    conversation.group_round = None;
                }
            }
            if let Some(sessions) = conversation.member_sessions.as_mut() {
                sessions.retain(|session| session.bot_id != bot_id);
            }
        }
    }

    pub fn add_group_conversation(&mut self, group_id: &str) -> HttpResult<Conversation> {
        let mut issues = Issues::new();
        issues.uuid("groupId", group_id);
        issues.finish()?;
        self.assert_writable()?;
        self.group(group_id)?;
        let conversation = new_conversation(group_id.to_owned(), Some(group_id.to_owned()), None, "New conversation", &iso(self.now()));
        self.disk_mut().conversations.push(conversation.clone());
        Ok(conversation)
    }

    /// The thread replying to `message_id`, created on first use (idempotent).
    pub fn create_thread(&mut self, conversation_id: &str, message_id: &str) -> HttpResult<Conversation> {
        let mut issues = Issues::new();
        issues.uuid("messageId", message_id);
        issues.finish()?;
        self.assert_writable()?;
        let parent = self.conversation(conversation_id)?;
        if self.worker_for_conversation(conversation_id).is_some()
            || self.disk.worker_batches.iter().any(|batch| batch.report_input_id.as_deref() == Some(message_id))
        {
            return Err(HttpError::conflict("Worker conversations and report inputs are private execution state."));
        }
        if parent.thread.is_some() {
            return Err(HttpError::bad_request("Threads cannot be nested. Reply in this thread instead."));
        }
        let message = parent.messages.iter().find(|message| message.id == message_id).ok_or_else(|| HttpError::not_found("Message does not exist."))?;
        if message.text.trim().is_empty() {
            return Err(HttpError::bad_request("Choose a message with text to start a thread."));
        }
        if let Some(existing) = self.thread_for(message_id) {
            return Ok(existing.clone());
        }
        let link = ThreadLink { parent_conversation_id: parent.id.clone(), parent_message_id: message_id.to_owned() };
        let title = crate::validate::js_slice(&format!("Thread: {}", message.text.split_whitespace().collect::<Vec<_>>().join(" ")), 70).to_owned();
        let thread = new_conversation(parent.bot_id.clone(), parent.group_id.clone(), Some(link), &title, &iso(self.now()));
        self.disk_mut().conversations.push(thread.clone());
        Ok(thread)
    }

    pub fn thread_for(&self, message_id: &str) -> Option<&Conversation> {
        self.disk.conversations.iter().find(|conversation| conversation.thread.as_ref().is_some_and(|link| link.parent_message_id == message_id))
    }

    /// Clear the pending members of a group round. `true` when no member run was active, so the
    /// conversation is settled as cancelled here; `false` when there was nothing to clear or a member
    /// run is active (the caller cancels that run).
    pub fn cancel_group_round(&mut self, conversation_id: &str) -> HttpResult<bool> {
        self.assert_writable()?;
        let conversation = self.conversation(conversation_id)?;
        if conversation.group_round.is_none() {
            return Ok(false);
        }
        let active = conversation.run.status.is_active();
        let timestamp = iso(self.now());
        let conversation = self.conversation_mut(conversation_id)?;
        conversation.group_round = None;
        if active {
            return Ok(false);
        }
        conversation.run.status = RunStatus::Cancelled;
        conversation.run.finished_at = Some(timestamp.clone());
        conversation.updated_at = timestamp;
        Ok(true)
    }

    /// A queued member turn could not start: end the round and show the error.
    pub fn fail_group_round(&mut self, conversation_id: &str, error: &str) -> HttpResult<()> {
        self.assert_writable()?;
        let error = crate::validate::js_slice(&self.redact(error), 1000).to_owned();
        let timestamp = iso(self.now());
        let conversation = self.conversation_mut(conversation_id)?;
        conversation.group_round = None;
        conversation.run = RunState {
            id: conversation.run.id.clone(),
            status: RunStatus::Error,
            bot_id: conversation.run.bot_id.clone(),
            error: Some(error),
            permission: None,
            todos: conversation.run.todos.clone(),
            started_at: conversation.run.started_at.clone(),
            finished_at: Some(timestamp.clone()),
        };
        conversation.updated_at = timestamp;
        Ok(())
    }
}
