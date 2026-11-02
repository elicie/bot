//! Bot-to-bot delivery data rules. Port of `server/bot-messages.ts`. CONTRACT (store agent).
//!
//! Follow-up ("report") turns: when a delivery sent from a plain 1:1 conversation finishes, its
//! `report` becomes queued and the dispatcher later runs one automatic turn of the sender bot in
//! that conversation with the recipient's result as data (`Message::report_delivery_ids` marks the
//! hidden input). Only one level is reported: deliveries sent from recipient, group, thread, or
//! routine conversations are not, and a chain gets at most [`MAX_BOT_REPORTS_PER_CHAIN`] reports.
//! A message the sender writes in the report turn continues the reported delivery's chain
//! ([`chain_parent`]), so report turns cannot reset the hop count.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use grok_bot_model::{
    BotDelivery, BotExchange, BotExchangeMessage, BotMessageInput, Conversation, DeliveryReport, DeliveryStatus, HttpError, HttpResult, ReportStatus, Role,
    RunStatus, iso,
};

use crate::validate::{Issues, is_timestamp, is_uuid, js_len, js_slice, js_trim};

pub const MAX_BOT_HOPS: u32 = 4;
/// Follow-up turns per delivery chain; later deliveries of the chain are delivered but not reported.
pub const MAX_BOT_REPORTS_PER_CHAIN: usize = 2;
pub const MAX_BOT_DELIVERIES: usize = 10000;
pub const MAX_BOT_MESSAGE_BYTES: usize = 160 * 1024;
pub const MAX_BOT_REPLY_CHARACTERS: usize = 200000;

fn request_id_valid(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// `botMessageInputSchema`: trims `text` and `requestId`.
pub fn validate_bot_message_input(mut input: BotMessageInput) -> HttpResult<BotMessageInput> {
    let mut issues = Issues::new();
    issues.uuid("toBotId", &input.to_bot_id);
    input.text = js_trim(&input.text).to_owned();
    issues.length("text", &input.text, 1, 100000);
    if let Some(request_id) = input.request_id.as_mut() {
        *request_id = js_trim(request_id).to_owned();
        issues.length("requestId", request_id, 1, 128);
        issues.pattern("requestId", request_id_valid(request_id), r"/^[\w.-]+$/");
    }
    issues.finish()?;
    Ok(input)
}

/// The durable receipt JSON returned to the sending model.
pub fn bot_delivery_receipt(delivery: &BotDelivery) -> serde_json::Value {
    serde_json::json!({
        "deliveryId": delivery.id,
        "exchangeId": delivery.exchange_id,
        "recipientConversationId": delivery.recipient_conversation_id,
        "status": delivery.status,
    })
}

/// An existing delivery for the same source message, target, and text (idempotent retry).
///
/// This contract signature cannot report the Node conflict; use [`find_existing_bot_delivery`],
/// which is the faithful port of `existingBotDelivery` (409 when the source message already sent a
/// different message).
pub fn existing_bot_delivery<'a>(deliveries: &'a [BotDelivery], source_message_id: &str, input: &BotMessageInput) -> Option<&'a BotDelivery> {
    find_existing_bot_delivery(deliveries, source_message_id, input).ok().flatten()
}

/// `existingBotDelivery`: `Ok(Some)` for an idempotent retry, `Ok(None)` when the source message has
/// not sent anything, 409 when it already sent a different message.
pub fn find_existing_bot_delivery<'a>(deliveries: &'a [BotDelivery], source_message_id: &str, input: &BotMessageInput) -> HttpResult<Option<&'a BotDelivery>> {
    let Some(existing) = deliveries.iter().find(|delivery| delivery.source_message_id == source_message_id) else { return Ok(None) };
    if existing.to_bot_id == input.to_bot_id && existing.text == input.text {
        return Ok(Some(existing));
    }
    Err(HttpError::conflict("A run may send only one distinct bot message. Repeating the same request returns its existing receipt."))
}

/// The delivery a message sent from `source_message_id` in `conversation` continues: the delivery
/// that created `conversation` (a recipient conversation), or, for the answer of a report turn, the
/// reported delivery with the highest hop. `None` starts a new chain.
pub fn chain_parent<'a>(deliveries: &'a [BotDelivery], conversation: &Conversation, source_message_id: &str) -> Option<&'a BotDelivery> {
    if let Some(parent) = deliveries.iter().find(|delivery| delivery.recipient_conversation_id == conversation.id) {
        return Some(parent);
    }
    let index = conversation.messages.iter().position(|message| message.id == source_message_id)?;
    let ids = conversation.messages.get(index.checked_sub(1)?)?.report_delivery_ids.as_deref()?;
    ids.iter().filter_map(|id| deliveries.iter().find(|delivery| delivery.id == *id)).fold(None, |best: Option<&BotDelivery>, delivery| match best {
        Some(best) if best.hop >= delivery.hop => Some(best),
        _ => Some(delivery),
    })
}

/// Results of deliveries sent from `conversation` are reported back to it: a plain 1:1 conversation,
/// not a recipient, group, thread, or routine conversation.
pub fn reports_to(deliveries: &[BotDelivery], conversation: &Conversation, routine_conversation_ids: &HashSet<&str>) -> bool {
    conversation.group_id.is_none()
        && conversation.thread.is_none()
        && !routine_conversation_ids.contains(conversation.id.as_str())
        && !deliveries.iter().any(|delivery| delivery.recipient_conversation_id == conversation.id)
}

fn report_run_status(status: ReportStatus) -> Option<RunStatus> {
    match status {
        ReportStatus::Completed => Some(RunStatus::Completed),
        ReportStatus::Cancelled => Some(RunStatus::Cancelled),
        ReportStatus::Error => Some(RunStatus::Error),
        ReportStatus::Interrupted => Some(RunStatus::Interrupted),
        ReportStatus::Queued | ReportStatus::Running | ReportStatus::Skipped => None,
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

/// Persisted report fields (`deliveryReportSchema`).
fn report_valid(report: &DeliveryReport) -> bool {
    report.run_id.as_deref().is_none_or(is_uuid)
        && optional_timestamp(&report.finished_at)
        && report.error.as_deref().is_none_or(|error| js_len(error) <= 1000)
        && match report.status {
            ReportStatus::Queued => report.run_id.is_none() && report.finished_at.is_none() && report.error.is_none(),
            ReportStatus::Running => report.run_id.is_some() && report.finished_at.is_none() && report.error.is_none(),
            ReportStatus::Skipped => report.run_id.is_none() && report.finished_at.is_some(),
            // A report can fail or be cancelled before the follow-up turn starts (no run).
            ReportStatus::Error | ReportStatus::Cancelled => report.finished_at.is_some(),
            ReportStatus::Completed | ReportStatus::Interrupted => report.run_id.is_some() && report.finished_at.is_some(),
        }
}

fn optional_timestamp(value: &Option<String>) -> bool {
    value.as_deref().is_none_or(is_timestamp)
}

/// `botExchangeSchema`.
pub(crate) fn exchange_valid(exchange: &BotExchange) -> bool {
    is_uuid(&exchange.id)
        && exchange.bot_ids.iter().all(|id| is_uuid(id))
        && (1..=2).contains(&exchange.messages.len())
        && exchange.messages.iter().all(|message| {
            is_uuid(&message.id) && is_uuid(&message.bot_id) && js_len(&message.text) <= MAX_BOT_REPLY_CHARACTERS && is_timestamp(&message.created_at)
        })
        && is_uuid(&exchange.source_conversation_id)
        && is_uuid(&exchange.source_message_id)
        && is_timestamp(&exchange.updated_at)
        && exchange.bot_ids[0] != exchange.bot_ids[1]
        && exchange.messages.iter().enumerate().all(|(index, message)| message.bot_id == exchange.bot_ids[index])
        && exchange.messages.iter().map(|message| message.id.as_str()).collect::<HashSet<_>>().len() == exchange.messages.len()
}

/// `botDeliverySchema`. Applies the schema's trim transforms to `text` and `requestId`.
pub(crate) fn delivery_valid(delivery: &mut BotDelivery) -> bool {
    delivery.text = js_trim(&delivery.text).to_owned();
    if let Some(request_id) = delivery.request_id.as_mut() {
        *request_id = js_trim(request_id).to_owned();
    }
    let fields = [
        &delivery.id,
        &delivery.exchange_id,
        &delivery.source_conversation_id,
        &delivery.source_message_id,
        &delivery.recipient_conversation_id,
        &delivery.from_bot_id,
        &delivery.to_bot_id,
        &delivery.chain_id,
    ]
    .iter()
    .all(|id| is_uuid(id))
        && (1..=100000).contains(&js_len(&delivery.text))
        && (1..=MAX_BOT_HOPS).contains(&delivery.hop)
        && is_timestamp(&delivery.created_at)
        && delivery.request_id.as_deref().is_none_or(|id| js_len(id) <= 128 && request_id_valid(id))
        && delivery.execution_run_id.as_deref().is_none_or(is_uuid)
        && optional_timestamp(&delivery.started_at)
        && optional_timestamp(&delivery.finished_at)
        && delivery.error.as_deref().is_none_or(|error| js_len(error) <= 1000)
        // Only finished deliveries are reported.
        && delivery.report.as_ref().is_none_or(|report| report_valid(report) && !matches!(delivery.status, DeliveryStatus::Queued | DeliveryStatus::Running));
    if !fields || delivery.from_bot_id == delivery.to_bot_id || delivery.text.len() > MAX_BOT_MESSAGE_BYTES {
        return false;
    }
    let present = |value: &Option<String>| value.as_deref().is_some_and(|text| !text.is_empty());
    match delivery.status {
        DeliveryStatus::Queued => {
            !present(&delivery.execution_run_id) && !present(&delivery.started_at) && !present(&delivery.finished_at) && !present(&delivery.error)
        }
        DeliveryStatus::Running => {
            present(&delivery.execution_run_id) && present(&delivery.started_at) && !present(&delivery.finished_at) && !present(&delivery.error)
        }
        status => {
            present(&delivery.finished_at) && (status != DeliveryStatus::Completed || (present(&delivery.execution_run_id) && present(&delivery.started_at)))
        }
    }
}

fn delivery_status(status: RunStatus) -> Option<DeliveryStatus> {
    match status {
        RunStatus::Completed => Some(DeliveryStatus::Completed),
        RunStatus::Cancelled => Some(DeliveryStatus::Cancelled),
        RunStatus::Error => Some(DeliveryStatus::Error),
        RunStatus::Interrupted => Some(DeliveryStatus::Interrupted),
        _ => None,
    }
}

fn run_status_name(status: DeliveryStatus) -> Option<RunStatus> {
    match status {
        DeliveryStatus::Completed => Some(RunStatus::Completed),
        DeliveryStatus::Cancelled => Some(RunStatus::Cancelled),
        DeliveryStatus::Error => Some(RunStatus::Error),
        DeliveryStatus::Interrupted => Some(RunStatus::Interrupted),
        DeliveryStatus::Queued | DeliveryStatus::Running => None,
    }
}

/// Project recipient conversation runs onto queued/running deliveries and copy the recipient's
/// final answer into the exchange when the delivery completes. A delivery that finishes now queues
/// its report when its source conversation is reported to (skipped once the chain used its
/// reports), and running reports follow the sender conversation's report run.
pub fn reconcile_bot_deliveries(
    deliveries: &mut [BotDelivery],
    exchanges: &mut [BotExchange],
    conversations: &[Conversation],
    routine_conversation_ids: &HashSet<&str>,
    now: DateTime<Utc>,
) {
    let mut finished = Vec::new();
    for (index, delivery) in deliveries.iter_mut().enumerate() {
        if !matches!(delivery.status, DeliveryStatus::Queued | DeliveryStatus::Running) {
            continue;
        }
        let Some(conversation) = conversations.iter().find(|item| item.id == delivery.recipient_conversation_id) else { continue };
        let execution = &conversation.run;
        if execution.status == RunStatus::Idle || delivery.execution_run_id.as_deref().is_some_and(|id| !id.is_empty() && id != execution.id) {
            continue;
        }
        if !execution.id.is_empty() {
            delivery.execution_run_id = Some(execution.id.clone());
        }
        if let Some(started_at) = execution.started_at.as_ref().filter(|value| !value.is_empty()) {
            delivery.started_at = Some(started_at.clone());
        }
        let Some(status) = delivery_status(execution.status) else {
            delivery.status = DeliveryStatus::Running;
            continue;
        };
        delivery.status = status;
        let finished_at = execution.finished_at.clone().unwrap_or_else(|| iso(now));
        delivery.finished_at = Some(finished_at.clone());
        if let Some(error) = execution.error.as_ref().filter(|value| !value.is_empty()) {
            delivery.error = Some(js_slice(error, 1000).to_owned());
        }
        finished.push(index);
        if status != DeliveryStatus::Completed {
            continue;
        }
        let answer = conversation.messages.iter().rev().find(|message| message.role == Role::Assistant);
        let Some(exchange) = exchanges.iter_mut().find(|item| item.id == delivery.exchange_id) else { continue };
        if let Some(answer) = answer
            && !exchange.messages.iter().any(|message| message.id == answer.id)
        {
            exchange.messages.push(BotExchangeMessage {
                id: answer.id.clone(),
                bot_id: delivery.to_bot_id.clone(),
                text: answer.text.clone(),
                created_at: answer.created_at.clone(),
            });
            exchange.updated_at = finished_at;
        }
    }
    for index in finished {
        let delivery = &deliveries[index];
        let Some(source) = conversations.iter().find(|item| item.id == delivery.source_conversation_id) else { continue };
        if delivery.report.is_some() || !reports_to(deliveries, source, routine_conversation_ids) {
            continue;
        }
        let used = deliveries
            .iter()
            .filter(|item| item.chain_id == delivery.chain_id && item.report.as_ref().is_some_and(|report| report.status != ReportStatus::Skipped))
            .count();
        let report = if used >= MAX_BOT_REPORTS_PER_CHAIN {
            DeliveryReport { status: ReportStatus::Skipped, finished_at: delivery.finished_at.clone(), ..DeliveryReport::queued() }
        } else {
            DeliveryReport::queued()
        };
        deliveries[index].report = Some(report);
    }
    for delivery in deliveries.iter_mut() {
        let Some(report) = delivery.report.as_mut().filter(|report| report.status == ReportStatus::Running) else { continue };
        let Some(source) = conversations.iter().find(|item| item.id == delivery.source_conversation_id) else { continue };
        let Some(status) = report_status(source.run.status).filter(|_| report.run_id.as_deref() == Some(source.run.id.as_str())) else { continue };
        report.status = status;
        report.finished_at = Some(source.run.finished_at.clone().unwrap_or_else(|| iso(now)));
        report.error =
            source.run.error.as_deref().filter(|error| !error.is_empty() && status != ReportStatus::Completed).map(|error| js_slice(error, 1000).to_owned());
    }
}

/// Queued reports of `conversation_id`, oldest first (FIFO by the time the delivery finished).
pub fn queued_reports<'a>(deliveries: &'a [BotDelivery], conversation_id: &str) -> Vec<&'a BotDelivery> {
    let mut queued: Vec<&BotDelivery> = deliveries
        .iter()
        .filter(|delivery| {
            delivery.source_conversation_id == conversation_id && delivery.report.as_ref().is_some_and(|report| report.status == ReportStatus::Queued)
        })
        .collect();
    queued.sort_by_key(|delivery| delivery.finished_at.as_deref().and_then(crate::validate::parse_ms).unwrap_or(i64::MAX));
    queued
}

/// A delivery's report agrees with its source conversation: only reportable sources, and a report
/// with a run has exactly one hidden report message there (running: the conversation's active run).
fn report_references_valid(
    deliveries: &[BotDelivery],
    delivery: &BotDelivery,
    report: &DeliveryReport,
    source: &Conversation,
    routine_conversation_ids: &HashSet<&str>,
) -> bool {
    if !reports_to(deliveries, source, routine_conversation_ids) {
        return false;
    }
    let messages = source.messages.iter().filter(|message| message.report_delivery_ids.as_ref().is_some_and(|ids| ids.contains(&delivery.id))).count();
    let Some(run_id) = report.run_id.as_deref() else { return messages == 0 };
    if messages != 1 {
        return false;
    }
    match report.status {
        ReportStatus::Running => source.run.id == run_id && source.run.status.is_active(),
        // The latest run of the conversation, when it is still the report run, ended the same way.
        status => source.run.id != run_id || report_run_status(status) == Some(source.run.status),
    }
}

/// Persisted bot exchange and delivery links are internally consistent (see `validBotMessageReferences`).
pub fn valid_bot_message_references(
    deliveries: &[BotDelivery],
    exchanges: &[BotExchange],
    conversations: &[Conversation],
    bot_ids: &HashSet<&str>,
    routine_conversation_ids: &HashSet<&str>,
) -> bool {
    let mut recipients = HashSet::new();
    let mut sources = HashSet::new();
    let mut exchange_ids = HashSet::new();
    let truthy = |value: &Option<String>| value.as_deref().is_some_and(|text| !text.is_empty());
    for delivery in deliveries {
        let exchange = exchanges.iter().find(|item| item.id == delivery.exchange_id);
        let source = conversations.iter().find(|item| item.id == delivery.source_conversation_id);
        let source_message = source.and_then(|source| source.messages.iter().find(|message| message.id == delivery.source_message_id));
        let recipient = conversations.iter().find(|item| item.id == delivery.recipient_conversation_id);
        let (Some(exchange), Some(source), Some(source_message), Some(recipient)) = (exchange, source, source_message, recipient) else { return false };
        let chained = match chain_parent(deliveries, source, &source_message.id) {
            Some(parent) => parent.id != delivery.id && delivery.chain_id == parent.chain_id && delivery.hop == parent.hop + 1,
            None => delivery.chain_id == delivery.id && delivery.hop == 1,
        };
        if let Some(report) = &delivery.report
            && !report_references_valid(deliveries, delivery, report, source, routine_conversation_ids)
        {
            return false;
        }
        if source_message.role != Role::Assistant
            || !bot_ids.contains(delivery.from_bot_id.as_str())
            || !bot_ids.contains(delivery.to_bot_id.as_str())
            || source.bot_id != delivery.from_bot_id
            || recipient.bot_id != delivery.to_bot_id
            || source.id == recipient.id
            || recipients.contains(recipient.id.as_str())
            || sources.contains(source_message.id.as_str())
            || exchange_ids.contains(exchange.id.as_str())
            || routine_conversation_ids.contains(recipient.id.as_str())
            || !source_message.bot_exchange_ids.as_ref().is_some_and(|ids| ids.contains(&exchange.id))
            || exchange.source_conversation_id != source.id
            || exchange.source_message_id != source_message.id
            || exchange.bot_ids[0] != delivery.from_bot_id
            || exchange.bot_ids[1] != delivery.to_bot_id
            || exchange.messages[0].text != delivery.text
            || !chained
        {
            return false;
        }
        if delivery.status == DeliveryStatus::Queued
            && (recipient.run.status != RunStatus::Idle || !recipient.run.id.is_empty() || !recipient.messages.is_empty())
        {
            return false;
        }
        if truthy(&delivery.execution_run_id) {
            if delivery.execution_run_id.as_deref() != Some(recipient.run.id.as_str())
                || delivery.started_at != recipient.run.started_at
                || recipient.messages.len() != 2
                || recipient.messages[0].role != Role::User
                || recipient.messages[0].source_bot_id.as_deref() != Some(delivery.from_bot_id.as_str())
                || recipient.messages[0].text != delivery.text
                || recipient.messages[1].role != Role::Assistant
            {
                return false;
            }
        } else if !recipient.run.id.is_empty() || !recipient.messages.is_empty() || truthy(&delivery.started_at) {
            return false;
        }
        if delivery.status == DeliveryStatus::Running && !recipient.run.status.is_active() {
            return false;
        }
        if let Some(status) = run_status_name(delivery.status)
            && (status != recipient.run.status || delivery.finished_at != recipient.run.finished_at)
        {
            return false;
        }
        let answer = recipient.messages.iter().rev().find(|message| message.role == Role::Assistant);
        if delivery.status == DeliveryStatus::Completed {
            let Some(answer) = answer else { return false };
            if exchange.messages.len() != 2 || exchange.messages[1].id != answer.id || exchange.messages[1].text != answer.text {
                return false;
            }
        } else if exchange.messages.len() != 1 {
            return false;
        }
        recipients.insert(recipient.id.as_str());
        sources.insert(source_message.id.as_str());
        exchange_ids.insert(exchange.id.as_str());
    }
    if exchanges.iter().any(|exchange| !exchange_ids.contains(exchange.id.as_str())) {
        return false;
    }
    let mut reports: HashMap<&str, usize> = HashMap::new();
    for delivery in deliveries {
        if delivery.report.as_ref().is_some_and(|report| report.status != ReportStatus::Skipped) {
            *reports.entry(delivery.chain_id.as_str()).or_default() += 1;
        }
    }
    if reports.values().any(|count| *count > MAX_BOT_REPORTS_PER_CHAIN) {
        return false;
    }
    let mut reported = HashSet::new();
    conversations.iter().all(|conversation| {
        conversation.messages.iter().enumerate().all(|(index, message)| {
            if let Some(ids) = message.report_delivery_ids.as_deref() {
                let next = conversation.messages.get(index + 1);
                if message.role != Role::User
                    || truthy(&message.source_bot_id)
                    || message.attachments.is_some()
                    || message.bot_exchange_ids.is_some()
                    || next.is_none_or(|next| next.role != Role::Assistant)
                    || !ids.iter().all(|id| {
                        reported.insert(id.as_str())
                            && deliveries.iter().any(|delivery| {
                                delivery.id == *id
                                    && delivery.source_conversation_id == conversation.id
                                    && delivery.report.as_ref().is_some_and(|report| report.run_id.is_some())
                            })
                    })
                {
                    return false;
                }
            }
            if let Some(source_bot_id) = message.source_bot_id.as_deref().filter(|id| !id.is_empty())
                && !(index == 0
                    && deliveries.iter().any(|delivery| delivery.recipient_conversation_id == conversation.id && delivery.from_bot_id == source_bot_id))
            {
                return false;
            }
            let ids = message.bot_exchange_ids.as_deref().unwrap_or_default();
            ids.iter().collect::<HashSet<_>>().len() == ids.len()
                && ids.iter().all(|id| {
                    exchanges
                        .iter()
                        .any(|exchange| exchange.id == *id && exchange.source_conversation_id == conversation.id && exchange.source_message_id == message.id)
                })
        })
    })
}

impl crate::State {
    /// IDs of conversations that routines run in (webhook runs included).
    pub fn routine_conversation_ids(&self) -> HashSet<&str> {
        self.disk.routine_runs.iter().map(|run| run.conversation_id.as_str()).collect()
    }

    /// Results of deliveries sent from this conversation are reported back to it ([`reports_to`]).
    pub fn reports_to_conversation(&self, conversation_id: &str) -> bool {
        self.conversation(conversation_id).is_ok_and(|conversation| reports_to(&self.disk.bot_deliveries, conversation, &self.routine_conversation_ids()))
    }

    /// Cancel every currently queued follow-up report of this source conversation, without
    /// starting a run or changing its messages. Running and terminal reports (and unfinished
    /// deliveries) are untouched. Allowed while the app is busy; a no-op does not write state.
    pub fn cancel_bot_reports(&mut self, conversation_id: &str) -> HttpResult<()> {
        self.assert_writable()?;
        self.conversation(conversation_id)?;
        let ids: HashSet<String> = queued_reports(&self.disk.bot_deliveries, conversation_id).iter().map(|delivery| delivery.id.clone()).collect();
        if ids.is_empty() {
            return Ok(());
        }
        let finished_at = iso(self.now());
        for delivery in self.disk_mut().bot_deliveries.iter_mut().filter(|delivery| ids.contains(&delivery.id)) {
            delivery.report = Some(DeliveryReport { status: ReportStatus::Cancelled, finished_at: Some(finished_at.clone()), ..DeliveryReport::queued() });
        }
        Ok(())
    }

    /// The follow-up turn of `conversation_id` could not start: its queued reports end with
    /// `error` and are not retried.
    pub fn fail_bot_reports(&mut self, conversation_id: &str, error: &str, now: DateTime<Utc>) -> HttpResult<()> {
        self.assert_writable()?;
        let ids: HashSet<String> = queued_reports(&self.disk.bot_deliveries, conversation_id).iter().map(|delivery| delivery.id.clone()).collect();
        if ids.is_empty() {
            return Ok(());
        }
        let error = js_slice(&self.redact(error), 1000).to_owned();
        let finished_at = iso(now);
        for delivery in self.disk_mut().bot_deliveries.iter_mut().filter(|delivery| ids.contains(&delivery.id)) {
            delivery.report =
                Some(DeliveryReport { status: ReportStatus::Error, run_id: None, finished_at: Some(finished_at.clone()), error: Some(error.clone()) });
        }
        Ok(())
    }
}
