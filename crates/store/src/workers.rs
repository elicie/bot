//! Bounded, read-only same-bot workers. Each approved task owns one hidden conversation. A batch
//! releases only after its source turn succeeds and produces one grouped, non-retried report.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use grok_bot_model::*;

use crate::State;
use crate::validate::{Issues, is_timestamp, is_uuid, js_len, js_slice, js_trim};

pub const MAX_WORKERS_PER_BATCH: usize = 3;
pub const MAX_WORKER_BATCHES: usize = 1000;
pub const MAX_WORKER_TASK_CHARACTERS: usize = 4000;
pub const MAX_WORKER_TASK_BYTES: usize = 16 * 1024;
pub const MAX_WORKER_ANSWER_BYTES: usize = 48 * 1024;

pub fn worker_terminal(status: DeliveryStatus) -> bool {
    !matches!(status, DeliveryStatus::Queued | DeliveryStatus::Running)
}

pub fn worker_batch_pending(batch: &WorkerBatch) -> bool {
    batch.workers.iter().any(|worker| !worker_terminal(worker.status)) || batch.report.as_ref().is_some_and(|report| !report.status.is_terminal())
}

pub(crate) fn bounded_answer(value: &str) -> String {
    let mut end = value.len().min(MAX_WORKER_ANSWER_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn worker_status(status: RunStatus) -> Option<DeliveryStatus> {
    match status {
        RunStatus::Idle => None,
        RunStatus::Starting | RunStatus::Running | RunStatus::AwaitingPermission => Some(DeliveryStatus::Running),
        RunStatus::Completed => Some(DeliveryStatus::Completed),
        RunStatus::Cancelled => Some(DeliveryStatus::Cancelled),
        RunStatus::Error => Some(DeliveryStatus::Error),
        RunStatus::Interrupted => Some(DeliveryStatus::Interrupted),
    }
}

fn report_status(status: RunStatus) -> Option<ReportStatus> {
    match status {
        RunStatus::Completed => Some(ReportStatus::Completed),
        RunStatus::Cancelled => Some(ReportStatus::Cancelled),
        RunStatus::Error => Some(ReportStatus::Error),
        RunStatus::Interrupted => Some(ReportStatus::Interrupted),
        _ => None,
    }
}

fn cancel_queued(batch: &mut WorkerBatch, conversations: &mut [Conversation], now: &str) {
    batch.cancelled = true;
    for worker in batch.workers.iter_mut().filter(|worker| worker.status == DeliveryStatus::Queued) {
        worker.status = DeliveryStatus::Cancelled;
        worker.finished_at = Some(now.to_owned());
        if let Some(conversation) = conversations.iter_mut().find(|conversation| conversation.id == worker.conversation_id) {
            conversation.run.status = RunStatus::Cancelled;
            conversation.run.permission = None;
            conversation.run.finished_at = Some(now.to_owned());
            conversation.updated_at = now.to_owned();
        }
    }
    if batch.report.as_ref().is_some_and(|report| report.status == ReportStatus::Queued) {
        batch.report = Some(DeliveryReport { status: ReportStatus::Skipped, finished_at: Some(now.to_owned()), ..DeliveryReport::queued() });
    }
}

/// Project durable execution state after every mutation, including startup interruption recovery.
/// Only newly terminal tasks are captured: later parent turns cannot overwrite an old answer.
pub(crate) fn reconcile(batches: &mut [WorkerBatch], conversations: &mut [Conversation], now: DateTime<Utc>) {
    let timestamp = iso(now);
    for batch in batches {
        if batch.released_at.is_none()
            && !batch.cancelled
            && let Some(parent) = conversations.iter().find(|conversation| conversation.id == batch.parent_conversation_id)
            && parent.run.id == batch.source_run_id
        {
            if parent.run.status == RunStatus::Completed {
                batch.released_at = Some(parent.run.finished_at.clone().unwrap_or_else(|| timestamp.clone()));
            } else if parent.run.status.is_terminal() {
                cancel_queued(batch, conversations, &timestamp);
            }
        }
        for worker in batch.workers.iter_mut().filter(|worker| !worker_terminal(worker.status)) {
            let Some(conversation) = conversations.iter().find(|conversation| conversation.id == worker.conversation_id) else { continue };
            if worker.execution_run_id.as_deref().is_some_and(|id| id != conversation.run.id) {
                continue;
            }
            let Some(status) = worker_status(conversation.run.status) else { continue };
            worker.status = status;
            if !conversation.run.id.is_empty() {
                worker.execution_run_id = Some(conversation.run.id.clone());
            }
            worker.started_at = conversation.run.started_at.clone();
            if worker_terminal(status) {
                worker.finished_at = Some(conversation.run.finished_at.clone().unwrap_or_else(|| timestamp.clone()));
                worker.error = conversation.run.error.as_deref().map(|error| js_slice(error, 1000).to_owned());
                if status == DeliveryStatus::Completed {
                    worker.answer =
                        conversation.messages.iter().rev().find(|message| message.role == Role::Assistant).map(|message| bounded_answer(&message.text));
                }
            }
        }
        if batch.released_at.is_some() && !batch.cancelled && batch.report.is_none() && batch.workers.iter().all(|worker| worker_terminal(worker.status)) {
            batch.report = Some(DeliveryReport::queued());
        }
        if let Some(report) = batch.report.as_mut().filter(|report| report.status == ReportStatus::Running)
            && let Some(parent) = conversations.iter().find(|conversation| conversation.id == batch.parent_conversation_id)
            && report.run_id.as_deref() == Some(parent.run.id.as_str())
            && let Some(status) = report_status(parent.run.status)
        {
            report.status = status;
            report.finished_at = Some(parent.run.finished_at.clone().unwrap_or_else(|| timestamp.clone()));
            report.error = parent.run.error.as_deref().map(|error| js_slice(error, 1000).to_owned());
        }
    }
}

fn task_valid(task: &str) -> bool {
    task == js_trim(task) && (1..=MAX_WORKER_TASK_CHARACTERS).contains(&js_len(task)) && task.len() <= MAX_WORKER_TASK_BYTES
}

fn optional_timestamp(value: &Option<String>) -> bool {
    value.as_deref().is_none_or(is_timestamp)
}

pub(crate) fn schema_valid(disk: &DiskState) -> bool {
    disk.worker_batches.len() <= MAX_WORKER_BATCHES
        && disk.worker_batches.iter().all(|batch| {
            [&batch.id, &batch.bot_id, &batch.parent_conversation_id, &batch.source_message_id, &batch.source_run_id].iter().all(|id| is_uuid(id))
                && is_timestamp(&batch.created_at)
                && optional_timestamp(&batch.released_at)
                && batch.report_input_id.as_deref().is_none_or(is_uuid)
                && batch.report_message_id.as_deref().is_none_or(is_uuid)
                && (1..=MAX_WORKERS_PER_BATCH).contains(&batch.workers.len())
                && batch.workers.iter().all(|worker| {
                    is_uuid(&worker.id)
                        && is_uuid(&worker.conversation_id)
                        && task_valid(&worker.task)
                        && is_timestamp(&worker.created_at)
                        && worker.execution_run_id.as_deref().is_none_or(is_uuid)
                        && optional_timestamp(&worker.started_at)
                        && optional_timestamp(&worker.finished_at)
                        && worker.error.as_deref().is_none_or(|error| js_len(error) <= 1000)
                        && worker.answer.as_deref().is_none_or(|answer| answer.len() <= MAX_WORKER_ANSWER_BYTES)
                        && match worker.status {
                            DeliveryStatus::Queued => {
                                worker.execution_run_id.is_none()
                                    && worker.started_at.is_none()
                                    && worker.finished_at.is_none()
                                    && worker.error.is_none()
                                    && worker.answer.is_none()
                            }
                            DeliveryStatus::Running => {
                                worker.execution_run_id.is_some()
                                    && worker.started_at.is_some()
                                    && worker.finished_at.is_none()
                                    && worker.error.is_none()
                                    && worker.answer.is_none()
                            }
                            DeliveryStatus::Completed => {
                                worker.execution_run_id.is_some()
                                    && worker.started_at.is_some()
                                    && worker.finished_at.is_some()
                                    && worker.error.is_none()
                                    && worker.answer.is_some()
                            }
                            _ => worker.finished_at.is_some() && worker.answer.is_none(),
                        }
                })
                && batch.report.as_ref().is_none_or(|report| {
                    report.run_id.as_deref().is_none_or(is_uuid)
                        && optional_timestamp(&report.finished_at)
                        && report.error.as_deref().is_none_or(|error| js_len(error) <= 1000)
                        && match report.status {
                            ReportStatus::Queued => report.run_id.is_none() && report.finished_at.is_none() && report.error.is_none(),
                            ReportStatus::Running => report.run_id.is_some() && report.finished_at.is_none() && report.error.is_none(),
                            ReportStatus::Skipped => report.run_id.is_none() && report.finished_at.is_some() && batch.cancelled,
                            ReportStatus::Error => report.finished_at.is_some(),
                            _ => report.run_id.is_some() && report.finished_at.is_some(),
                        }
                })
        })
}

fn plain_parent(disk: &DiskState, parent: &Conversation) -> bool {
    parent.group_id.is_none()
        && parent.thread.is_none()
        && !disk.routine_runs.iter().any(|run| run.conversation_id == parent.id)
        && !disk.bot_deliveries.iter().any(|delivery| delivery.recipient_conversation_id == parent.id)
        && !disk.worker_batches.iter().flat_map(|batch| &batch.workers).any(|worker| worker.conversation_id == parent.id)
}

pub(crate) fn references_valid(disk: &DiskState) -> bool {
    let hidden: HashSet<&str> = disk.worker_batches.iter().flat_map(|batch| batch.workers.iter().map(|worker| worker.conversation_id.as_str())).collect();
    let mut ids: HashSet<&str> = disk
        .bots
        .iter()
        .map(|bot| bot.id.as_str())
        .chain(disk.conversations.iter().map(|conversation| conversation.id.as_str()))
        .chain(disk.routines.iter().map(|routine| routine.id.as_str()))
        .chain(disk.routine_runs.iter().map(|run| run.id.as_str()))
        .chain(disk.bot_deliveries.iter().map(|delivery| delivery.id.as_str()))
        .chain(disk.bot_exchanges.iter().map(|exchange| exchange.id.as_str()))
        .chain(disk.groups.iter().map(|group| group.id.as_str()))
        .chain(disk.permission_rules.iter().map(|rule| rule.id.as_str()))
        .collect();
    let mut sources = HashSet::new();
    let mut source_runs = HashSet::new();
    let mut children = HashSet::new();
    let mut reports = HashSet::new();
    let mut running = 0;
    for batch in &disk.worker_batches {
        let Some(parent) = disk.conversations.iter().find(|conversation| conversation.id == batch.parent_conversation_id) else { return false };
        let Some(source) = parent.messages.iter().find(|message| message.id == batch.source_message_id) else { return false };
        if !ids.insert(&batch.id)
            || !sources.insert((&batch.parent_conversation_id, &batch.source_message_id))
            || !source_runs.insert((&batch.parent_conversation_id, &batch.source_run_id))
            || !disk.bots.iter().any(|bot| bot.id == batch.bot_id)
            || parent.bot_id != batch.bot_id
            || !plain_parent(disk, parent)
            || (batch.released_at.is_some() && parent.run.id == batch.source_run_id && parent.run.status != RunStatus::Completed)
            || source.role != Role::Assistant
            || disk.worker_batches.iter().any(|other| other.report_message_id.as_deref() == Some(source.id.as_str()))
            || parent
                .messages
                .iter()
                .position(|message| message.id == source.id)
                .and_then(|index| index.checked_sub(1))
                .is_some_and(|index| parent.messages[index].report_delivery_ids.is_some())
            || (batch.released_at.is_none() && !batch.cancelled && (parent.run.id != batch.source_run_id || !parent.run.status.is_active()))
        {
            return false;
        }
        let mut tasks = HashSet::new();
        for worker in &batch.workers {
            let Some(child) = disk.conversations.iter().find(|conversation| conversation.id == worker.conversation_id) else { return false };
            if !ids.insert(&worker.id)
                || !children.insert(&worker.conversation_id)
                || !tasks.insert(&worker.task)
                || child.bot_id != batch.bot_id
                || child.group_id.is_some()
                || child.thread.is_some()
                || child.group_round.is_some()
                || child.member_sessions.is_some()
                || disk.routine_runs.iter().any(|run| run.conversation_id == child.id)
                || disk.bot_deliveries.iter().any(|delivery| delivery.recipient_conversation_id == child.id || delivery.source_conversation_id == child.id)
                || disk.conversations.iter().any(|conversation| conversation.thread.as_ref().is_some_and(|link| link.parent_conversation_id == child.id))
            {
                return false;
            }
            if let Some(run_id) = &worker.execution_run_id {
                if child.run.id != *run_id
                    || child.run.started_at != worker.started_at
                    || child.messages.len() != 2
                    || child.messages[0].role != Role::User
                    || child.messages[0].text != worker.task
                    || child.messages[0].attachments.is_some()
                    || child.messages[0].source_bot_id.is_some()
                    || child.messages[1].role != Role::Assistant
                    || child
                        .messages
                        .iter()
                        .any(|message| message.bot_exchange_ids.is_some() || message.report_delivery_ids.is_some() || message.speaker_bot_id.is_some())
                {
                    return false;
                }
            } else if !child.run.id.is_empty() || !child.messages.is_empty() || worker.started_at.is_some() {
                return false;
            }
            if worker.status == DeliveryStatus::Queued {
                if child.run.status != RunStatus::Idle || (batch.cancelled || (batch.released_at.is_none() && !parent.run.status.is_active())) {
                    return false;
                }
            } else if worker.status == DeliveryStatus::Running {
                running += 1;
                if !child.run.status.is_active() || batch.released_at.is_none() {
                    return false;
                }
            } else if worker_status(child.run.status) != Some(worker.status) || child.run.finished_at != worker.finished_at {
                return false;
            }
            if worker.status == DeliveryStatus::Completed && child.messages.last().map(|message| bounded_answer(&message.text)) != worker.answer {
                return false;
            }
        }
        if let Some(report) = &batch.report {
            if !batch.workers.iter().all(|worker| worker_terminal(worker.status)) || (batch.released_at.is_none() && report.status != ReportStatus::Skipped) {
                return false;
            }
            if let Some(run_id) = &report.run_id {
                let (Some(input_id), Some(answer_id)) = (&batch.report_input_id, &batch.report_message_id) else { return false };
                let Some(index) = parent.messages.iter().position(|message| message.id == *input_id) else { return false };
                let input = &parent.messages[index];
                if !reports.insert(input_id)
                    || !reports.insert(answer_id)
                    || parent.messages[..index].iter().all(|message| message.id != batch.source_message_id)
                    || input.role != Role::User
                    || input.bot_exchange_ids.is_some()
                    || input.speaker_bot_id.is_some()
                    || input.attachments.is_some()
                    || input.source_bot_id.is_some()
                    || input.report_delivery_ids.is_some()
                    || parent.messages.get(index + 1).is_none_or(|message| message.id != *answer_id || message.role != Role::Assistant)
                    || disk.conversations.iter().flat_map(|conversation| &conversation.messages).filter(|message| message.id == *input_id).count() != 1
                    || disk.conversations.iter().flat_map(|conversation| &conversation.messages).filter(|message| message.id == *answer_id).count() != 1
                    || (report.status == ReportStatus::Running && (parent.run.id != *run_id || !parent.run.status.is_active()))
                    || (parent.run.id == *run_id && report.status != ReportStatus::Running && report_status(parent.run.status) != Some(report.status))
                {
                    return false;
                }
            } else if batch.report_input_id.is_some() || batch.report_message_id.is_some() {
                return false;
            }
            if batch.cancelled && report.status == ReportStatus::Queued {
                return false;
            }
        } else if batch.report_input_id.is_some() || batch.report_message_id.is_some() {
            return false;
        }
    }
    running <= 1 && hidden.len() == children.len()
}

impl State {
    pub fn worker_batch(&self, batch_id: &str) -> HttpResult<&WorkerBatch> {
        self.disk.worker_batches.iter().find(|batch| batch.id == batch_id).ok_or_else(|| HttpError::not_found("Worker batch does not exist."))
    }

    pub fn worker_for_conversation(&self, conversation_id: &str) -> Option<(&WorkerBatch, &WorkerTask)> {
        self.disk
            .worker_batches
            .iter()
            .find_map(|batch| batch.workers.iter().find(|worker| worker.conversation_id == conversation_id).map(|worker| (batch, worker)))
    }

    pub fn find_existing_worker(
        &self,
        parent_conversation_id: &str,
        source_message_id: &str,
        source_run_id: &str,
        task: &str,
    ) -> Option<(&WorkerBatch, &WorkerTask)> {
        self.disk
            .worker_batches
            .iter()
            .filter(|batch| {
                batch.parent_conversation_id == parent_conversation_id && batch.source_message_id == source_message_id && batch.source_run_id == source_run_id
            })
            .find_map(|batch| batch.workers.iter().find(|worker| worker.task == js_trim(task)).map(|worker| (batch, worker)))
    }

    pub fn validate_worker_source(&self, parent_conversation_id: &str, source_message_id: &str, source_run_id: &str, task: &str) -> HttpResult<()> {
        self.assert_writable()?;
        let mut issues = Issues::new();
        issues.uuid("sourceMessageId", source_message_id);
        issues.uuid("sourceRunId", source_run_id);
        issues.length("task", js_trim(task), 1, MAX_WORKER_TASK_CHARACTERS);
        issues.finish()?;
        if js_trim(task).len() > MAX_WORKER_TASK_BYTES {
            return Err(HttpError::too_large("Worker tasks must be at most 16 KiB of UTF-8 text."));
        }
        let parent = self.conversation(parent_conversation_id)?;
        let latest = parent.messages.iter().rfind(|message| message.role == Role::Assistant);
        if !plain_parent(&self.disk, parent)
            || self.disk.worker_batches.iter().any(|batch| batch.report_message_id.as_deref() == Some(source_message_id))
            || parent
                .messages
                .iter()
                .position(|message| message.id == source_message_id)
                .and_then(|index| index.checked_sub(1))
                .is_some_and(|index| parent.messages[index].report_delivery_ids.is_some())
        {
            return Err(HttpError::conflict("Workers can be started only by a user-requested plain one-to-one parent turn (depth limit: one)."));
        }
        if parent.run.id != source_run_id || !parent.run.status.is_active() || latest.is_none_or(|message| message.id != source_message_id) {
            return Err(HttpError::conflict("The spawning parent run is no longer active."));
        }
        if self.find_existing_worker(parent_conversation_id, source_message_id, source_run_id, task).is_some() {
            return Ok(());
        }
        if self
            .disk
            .worker_batches
            .iter()
            .any(|batch| batch.parent_conversation_id == parent_conversation_id && batch.source_message_id != source_message_id && worker_batch_pending(batch))
        {
            return Err(HttpError::conflict("Wait for or cancel the existing worker batch first."));
        }
        if let Some(batch) = self.disk.worker_batches.iter().find(|batch| batch.source_message_id == source_message_id) {
            if batch.source_run_id != source_run_id || batch.parent_conversation_id != parent_conversation_id || batch.cancelled || batch.released_at.is_some()
            {
                return Err(HttpError::conflict("The worker batch is already sealed."));
            }
            if batch.workers.len() >= MAX_WORKERS_PER_BATCH {
                return Err(HttpError::conflict("A parent turn may start at most three workers."));
            }
        } else if self.disk.worker_batches.len() >= MAX_WORKER_BATCHES {
            return Err(HttpError::conflict("Worker history is full. Existing history is retained."));
        }
        Ok(())
    }

    pub fn spawn_worker(
        &mut self,
        parent_conversation_id: &str,
        source_message_id: &str,
        source_run_id: &str,
        task: &str,
    ) -> HttpResult<(WorkerBatch, WorkerTask)> {
        self.validate_worker_source(parent_conversation_id, source_message_id, source_run_id, task)?;
        if let Some((batch, worker)) = self.find_existing_worker(parent_conversation_id, source_message_id, source_run_id, task) {
            return Ok((batch.clone(), worker.clone()));
        }
        let parent = self.conversation(parent_conversation_id)?;
        let timestamp = iso(self.now());
        let conversation = Conversation {
            id: new_id(),
            bot_id: parent.bot_id.clone(),
            title: "Worker task".into(),
            messages: Vec::new(),
            session_id: None,
            session_fingerprint: None,
            run: RunState::idle(),
            updated_at: timestamp.clone(),
            group_id: None,
            thread: None,
            group_round: None,
            member_sessions: None,
        };
        let worker = WorkerTask {
            id: new_id(),
            conversation_id: conversation.id.clone(),
            task: js_trim(task).to_owned(),
            created_at: timestamp.clone(),
            status: DeliveryStatus::Queued,
            execution_run_id: None,
            started_at: None,
            finished_at: None,
            error: None,
            answer: None,
        };
        let existing = self.disk.worker_batches.iter().position(|batch| batch.source_message_id == source_message_id);
        let mut batch = existing.map(|index| self.disk.worker_batches[index].clone()).unwrap_or_else(|| WorkerBatch {
            id: new_id(),
            bot_id: parent.bot_id.clone(),
            parent_conversation_id: parent_conversation_id.into(),
            source_message_id: source_message_id.into(),
            source_run_id: source_run_id.into(),
            created_at: timestamp,
            released_at: None,
            cancelled: false,
            workers: Vec::new(),
            report: None,
            report_input_id: None,
            report_message_id: None,
        });
        batch.workers.push(worker.clone());
        let mut prospect = self.prospect();
        prospect.conversations.push(&conversation);
        match existing {
            Some(index) => prospect.worker_batches[index] = &batch,
            None => prospect.worker_batches.push(&batch),
        }
        Self::check_capacity(&prospect)?;
        let disk = self.disk_mut();
        disk.conversations.push(conversation);
        match existing {
            Some(index) => disk.worker_batches[index] = batch.clone(),
            None => disk.worker_batches.push(batch.clone()),
        }
        Ok((batch, worker))
    }

    pub fn bind_worker_execution(&mut self, worker_id: &str, run_id: &str) -> HttpResult<()> {
        self.assert_writable()?;
        let (batch_index, worker_index) = self
            .disk
            .worker_batches
            .iter()
            .enumerate()
            .find_map(|(i, batch)| batch.workers.iter().position(|worker| worker.id == worker_id).map(|j| (i, j)))
            .ok_or_else(|| HttpError::not_found("Worker does not exist."))?;
        let batch = &self.disk.worker_batches[batch_index];
        let worker = &batch.workers[worker_index];
        let child = self.conversation(&worker.conversation_id)?;
        if batch.cancelled
            || batch.released_at.is_none()
            || worker.status != DeliveryStatus::Queued
            || child.run.id != run_id
            || !child.run.status.is_active()
            || self.disk.worker_batches.iter().flat_map(|batch| &batch.workers).any(|other| other.id != worker_id && other.status == DeliveryStatus::Running)
        {
            return Err(HttpError::conflict("The worker is no longer ready to start."));
        }
        let worker = &mut self.disk_mut().worker_batches[batch_index].workers[worker_index];
        worker.execution_run_id = Some(run_id.to_owned());
        Ok(())
    }

    pub fn bind_worker_report(&mut self, batch_id: &str, run_id: &str, input_id: &str, assistant_id: &str) -> HttpResult<()> {
        self.assert_writable()?;
        let batch = self.worker_batch(batch_id)?;
        let parent = self.conversation(&batch.parent_conversation_id)?;
        if batch.cancelled
            || batch.report.as_ref().is_none_or(|report| report.status != ReportStatus::Queued)
            || parent.run.id != run_id
            || !parent.run.status.is_active()
            || parent.messages.len() < 2
            || parent.messages[parent.messages.len() - 2].id != input_id
            || parent.messages[parent.messages.len() - 2].role != Role::User
            || parent.messages.last().is_none_or(|message| message.id != assistant_id || message.role != Role::Assistant)
        {
            return Err(HttpError::conflict("The grouped worker follow-up is no longer ready to start."));
        }
        let batch = self.disk_mut().worker_batches.iter_mut().find(|batch| batch.id == batch_id).expect("validated above");
        batch.report = Some(DeliveryReport { status: ReportStatus::Running, run_id: Some(run_id.into()), ..DeliveryReport::queued() });
        batch.report_input_id = Some(input_id.into());
        batch.report_message_id = Some(assistant_id.into());
        Ok(())
    }

    pub fn cancel_worker_batches(&mut self, parent_id: &str) -> HttpResult<Vec<String>> {
        self.assert_writable()?;
        self.conversation(parent_id)?;
        let timestamp = iso(self.now());
        let pending: Vec<usize> = self
            .disk
            .worker_batches
            .iter()
            .enumerate()
            .filter(|(_, batch)| batch.parent_conversation_id == parent_id && worker_batch_pending(batch))
            .map(|(index, _)| index)
            .collect();
        let active = pending
            .iter()
            .flat_map(|index| &self.disk.worker_batches[*index].workers)
            .filter(|worker| worker.status == DeliveryStatus::Running)
            .map(|worker| worker.conversation_id.clone())
            .collect();
        if !pending.is_empty() {
            let disk = self.disk_mut();
            for index in pending {
                cancel_queued(&mut disk.worker_batches[index], &mut disk.conversations, &timestamp);
            }
        }
        Ok(active)
    }

    pub fn fail_worker(&mut self, worker_id: &str, error: &str) -> HttpResult<()> {
        self.assert_writable()?;
        let worker = self
            .disk
            .worker_batches
            .iter()
            .flat_map(|batch| &batch.workers)
            .find(|worker| worker.id == worker_id)
            .ok_or_else(|| HttpError::not_found("Worker does not exist."))?;
        if worker_terminal(worker.status) {
            return Ok(());
        }
        let id = worker.conversation_id.clone();
        let error = js_slice(&self.redact(error), 1000).to_owned();
        let timestamp = iso(self.now());
        let conversation = self.conversation_mut(&id)?;
        conversation.run.status = RunStatus::Error;
        conversation.run.error = Some(error);
        conversation.run.permission = None;
        conversation.run.finished_at = Some(timestamp.clone());
        conversation.updated_at = timestamp;
        Ok(())
    }

    pub fn fail_worker_report(&mut self, batch_id: &str, error: &str) -> HttpResult<()> {
        self.assert_writable()?;
        if self.worker_batch(batch_id)?.report.as_ref().is_none_or(|report| report.status != ReportStatus::Queued) {
            return Ok(());
        }
        let error = js_slice(&self.redact(error), 1000).to_owned();
        let timestamp = iso(self.now());
        let batch = self.disk_mut().worker_batches.iter_mut().find(|batch| batch.id == batch_id).expect("validated above");
        batch.report = Some(DeliveryReport { status: ReportStatus::Error, run_id: None, finished_at: Some(timestamp), error: Some(error) });
        Ok(())
    }
}
