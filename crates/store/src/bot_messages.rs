//! Bot-to-bot delivery data rules. Port of `server/bot-messages.ts`. CONTRACT (store agent).
//!
//! A recipient waits for all work it sent from its conversation, including later report turns.
//! Child results are consumed by hidden report user/assistant pairs before the recipient's final
//! answer is copied upstream. Reports reconcile before deliveries, deepest first, so recovery and
//! ordinary completion have the same exactly-once projection. Mandatory internal reports do not
//! consume the original conversation's [`MAX_BOT_REPORTS_PER_CHAIN`] budget. Group, thread, and
//! routine sources remain excluded. Report-turn sends continue the reported child's lineage
//! ([`chain_parent`]), never resetting the four-hop limit.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use grok_bot_model::{
    BotDelivery, BotExchange, BotExchangeMessage, BotMessageInput, ChainTermination, Conversation, DeliveryReport, DeliveryStatus, HttpError, HttpResult,
    ReportStatus, Role, RunStatus, ToolStatus, iso,
};

use crate::validate::{Issues, is_timestamp, is_uuid, js_len, js_slice, js_trim};

pub const MAX_BOT_HOPS: u32 = 4;
/// Explicitly retain the natural four-hop/two-root-report fanout bound: 8 + 4 + 2 deliveries.
pub const MAX_BOT_DELIVERIES_PER_CHAIN: usize = 14;
/// Each delivery starts at most one recipient turn and one report turn, plus the origin turn.
/// No persistent counter is needed: admission bounds deliveries and each report is claimed once.
pub const MAX_BOT_TURNS_PER_CHAIN: usize = 2 * MAX_BOT_DELIVERIES_PER_CHAIN + 1;
pub const BOT_CHAIN_CAPACITY_ERROR: &str = "Bot message chains are limited to 14 deliveries.";
/// Reports to original/nonrecipient conversations per chain; internal recipient reports are exempt.
pub const MAX_BOT_REPORTS_PER_CHAIN: usize = 2;
pub const BOT_CHAIN_TIMEOUT_MINUTES: i64 = 30;
pub const BOT_CHAIN_TIMEOUT_ERROR: &str = "Bot message chain exceeded the 30-minute wall-clock limit.";
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

/// The preceding hidden report marker takes precedence over the inbound creator: a follow-up
/// send continues the reported child's highest hop even inside a recipient conversation.
pub fn chain_parent<'a>(deliveries: &'a [BotDelivery], conversation: &Conversation, source_message_id: &str) -> Option<&'a BotDelivery> {
    let report_parent = conversation
        .messages
        .iter()
        .position(|message| message.id == source_message_id)
        .and_then(|index| index.checked_sub(1))
        .and_then(|index| conversation.messages[index].report_delivery_ids.as_deref())
        .into_iter()
        .flatten()
        .filter_map(|id| deliveries.iter().find(|delivery| delivery.id == *id))
        // PR9 kept the FIRST highest-hop ID; old mixed-chain root markers depend on that tie-break.
        .fold(None, |best: Option<&BotDelivery>, delivery| match best {
            Some(best) if best.hop >= delivery.hop => Some(best),
            _ => Some(delivery),
        });
    report_parent.or_else(|| deliveries.iter().find(|delivery| delivery.recipient_conversation_id == conversation.id))
}

/// Recipient conversations can consume internal reports; only group, thread, and routine sources
/// are excluded. A closed legacy recipient is handled separately when a report is first queued.
pub fn reports_to(_deliveries: &[BotDelivery], conversation: &Conversation, routine_conversation_ids: &HashSet<&str>) -> bool {
    conversation.group_id.is_none() && conversation.thread.is_none() && !routine_conversation_ids.contains(conversation.id.as_str())
}

fn inbound<'a>(deliveries: &'a [BotDelivery], conversation_id: &str) -> Option<&'a BotDelivery> {
    deliveries.iter().find(|delivery| delivery.recipient_conversation_id == conversation_id)
}

fn report_pending(delivery: &BotDelivery) -> bool {
    delivery.report.as_ref().is_some_and(|report| !report.status.is_terminal())
}

/// Ownership is the source conversation, not causal lineage: report sends can parent to a child.
fn children_pending(deliveries: &[BotDelivery], conversation_id: &str) -> bool {
    deliveries.iter().any(|delivery| delivery.source_conversation_id == conversation_id && (delivery.status.is_pending() || report_pending(delivery)))
}

/// The latest pair is proven to be a report turn by BOTH its hidden marker and the children's
/// matching report run IDs. An arbitrary change to a recipient run must never rebind its delivery.
fn latest_report_run<'a>(deliveries: &'a [BotDelivery], conversation: &Conversation) -> Option<&'a str> {
    let index = conversation.messages.len().checked_sub(2)?;
    let marker = &conversation.messages[index];
    if marker.role != Role::User || conversation.messages[index + 1].role != Role::Assistant {
        return None;
    }
    let ids = marker.report_delivery_ids.as_deref().filter(|ids| !ids.is_empty())?;
    let mut run = None;
    for id in ids {
        let child = deliveries.iter().find(|delivery| delivery.id == *id && delivery.source_conversation_id == conversation.id)?;
        let id = child.report.as_ref()?.run_id.as_deref()?;
        if run.is_some_and(|run| run != id) {
            return None;
        }
        run = Some(id);
    }
    run
}

fn chain_expired(deliveries: &[BotDelivery], chain_id: &str, now: DateTime<Utc>) -> bool {
    deliveries
        .iter()
        .filter(|delivery| delivery.chain_id == chain_id)
        .filter_map(|delivery| crate::validate::parse_ms(&delivery.created_at))
        .min()
        .is_some_and(|created| now.timestamp_millis().saturating_sub(created) >= BOT_CHAIN_TIMEOUT_MINUTES * 60 * 1000)
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
        // Only finished deliveries are reported; the root latch is the durable pending abort receipt.
        && delivery.report.as_ref().is_none_or(|report| report_valid(report) && delivery.status.is_terminal())
        && delivery.chain_termination.as_ref().is_none_or(|termination| {
            delivery.id == delivery.chain_id
                && matches!(termination.status, RunStatus::Cancelled | RunStatus::Error)
                && is_timestamp(&termination.finished_at)
                && termination.error.as_deref().is_none_or(|error| js_len(error) <= 1000)
        });
    if !fields || delivery.from_bot_id == delivery.to_bot_id || delivery.text.len() > MAX_BOT_MESSAGE_BYTES {
        return false;
    }
    let present = |value: &Option<String>| value.as_deref().is_some_and(|text| !text.is_empty());
    match delivery.status {
        DeliveryStatus::Queued => {
            !present(&delivery.execution_run_id) && !present(&delivery.started_at) && !present(&delivery.finished_at) && !present(&delivery.error)
        }
        DeliveryStatus::Running | DeliveryStatus::WaitingChildren => {
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
        DeliveryStatus::Queued | DeliveryStatus::Running | DeliveryStatus::WaitingChildren => None,
    }
}

/// Finish non-active owned runs only. Active engines retain their run/delivery until Runtime has
/// stopped them. This also makes a durable latch authoritative if Stop and timeout race.
fn terminate_run(conversation: &mut Conversation, termination: &ChainTermination, now: DateTime<Utc>) -> bool {
    if conversation.run.status.is_active() {
        return false;
    }
    let run = &mut conversation.run;
    if run.status == termination.status && run.error == termination.error && run.finished_at.is_some() && run.permission.is_none() {
        return false;
    }
    run.status = termination.status;
    run.error = termination.error.clone();
    run.finished_at = Some(iso(now));
    run.permission = None;
    for tool in conversation.messages.iter_mut().flat_map(|message| &mut message.tools) {
        if matches!(tool.status, ToolStatus::Pending | ToolStatus::InProgress) {
            tool.status = ToolStatus::Failed;
        }
    }
    conversation.updated_at = iso(now);
    true
}

fn skip_report(delivery: &mut BotDelivery, error: Option<String>, now: DateTime<Utc>) {
    delivery.report = Some(DeliveryReport { status: ReportStatus::Skipped, run_id: None, finished_at: Some(iso(now)), error });
}

/// Enforce canonical root receipts on pending work, never rewriting historical terminal records.
fn enforce_chain_terminations(deliveries: &mut [BotDelivery], conversations: &mut [Conversation], now: DateTime<Utc>) -> bool {
    let terminations: HashMap<String, ChainTermination> =
        deliveries.iter().filter_map(|delivery| delivery.chain_termination.clone().map(|termination| (delivery.chain_id.clone(), termination))).collect();
    let mut changed = false;
    for delivery in deliveries {
        let Some(termination) = terminations.get(&delivery.chain_id) else { continue };
        if delivery.status.is_pending()
            && let Some(recipient) = conversations.iter_mut().find(|conversation| conversation.id == delivery.recipient_conversation_id)
        {
            changed |= terminate_run(recipient, termination, now);
        }
        match delivery.report.as_ref().map(|report| report.status) {
            Some(ReportStatus::Queued) => {
                skip_report(delivery, termination.error.clone(), now);
                changed = true;
            }
            Some(ReportStatus::Running) => {
                if let Some(source) = conversations.iter_mut().find(|conversation| {
                    conversation.id == delivery.source_conversation_id
                        && delivery.report.as_ref().and_then(|report| report.run_id.as_deref()) == Some(conversation.run.id.as_str())
                }) {
                    changed |= terminate_run(source, termination, now);
                }
            }
            _ => {}
        }
    }
    changed
}

/// A failed recipient cannot consume later child reports. Cancel its queued descendant subtree
/// locally (not the whole chain, because its own failure still needs to be reported upstream).
fn suppress_failed_children(deliveries: &mut [BotDelivery], conversations: &mut [Conversation], now: DateTime<Utc>) {
    if deliveries.is_empty() {
        return;
    }
    let terminated: HashSet<String> =
        deliveries.iter().filter(|delivery| delivery.chain_termination.is_some()).map(|delivery| delivery.chain_id.clone()).collect();
    let failed_conversations: HashSet<&str> = conversations
        .iter()
        .filter(|conversation| matches!(conversation.run.status, RunStatus::Cancelled | RunStatus::Error | RunStatus::Interrupted))
        .map(|conversation| conversation.id.as_str())
        .collect();
    // Streamed-token commits normally have no failed recipient: seed in O(C + D), not O(C * D).
    let mut failed: HashSet<String> = deliveries
        .iter()
        .filter(|delivery| !terminated.contains(&delivery.chain_id) && failed_conversations.contains(delivery.recipient_conversation_id.as_str()))
        .map(|delivery| delivery.recipient_conversation_id.clone())
        .collect();
    if failed.is_empty() {
        return;
    }
    let conversation_index: HashMap<String, usize> = conversations.iter().enumerate().map(|(index, conversation)| (conversation.id.clone(), index)).collect();
    let mut outgoing: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, delivery) in deliveries.iter().enumerate().filter(|(_, delivery)| !terminated.contains(&delivery.chain_id)) {
        outgoing.entry(delivery.source_conversation_id.clone()).or_default().push(index);
    }
    // All outgoing work is owned by the recipient conversation, including sends in report turns.
    // Visit each descendant and its outgoing edges once, retaining terminal history and root latches.
    let mut pending: Vec<String> = failed.iter().cloned().collect();
    let termination =
        ChainTermination { status: RunStatus::Cancelled, finished_at: iso(now), error: Some("The sending recipient run failed or stopped.".into()) };
    while let Some(source) = pending.pop() {
        for &index in outgoing.get(&source).into_iter().flatten() {
            let delivery = &mut deliveries[index];
            if delivery.status.is_pending() {
                if let Some(&recipient) = conversation_index.get(&delivery.recipient_conversation_id) {
                    terminate_run(&mut conversations[recipient], &termination, now);
                }
                if failed.insert(delivery.recipient_conversation_id.clone()) {
                    pending.push(delivery.recipient_conversation_id.clone());
                }
            }
            if delivery.report.as_ref().is_some_and(|report| report.status == ReportStatus::Queued) {
                skip_report(delivery, termination.error.clone(), now);
            }
        }
    }
}

/// Reports FIRST, then deepest pending delivery to root. Only deliveries that become terminal in
/// this pass receive a report, preserving old completed disks that never had report fields.
pub fn reconcile_bot_deliveries(
    deliveries: &mut [BotDelivery],
    exchanges: &mut [BotExchange],
    conversations: &mut [Conversation],
    routine_conversation_ids: &HashSet<&str>,
    now: DateTime<Utc>,
) {
    enforce_chain_terminations(deliveries, conversations, now);
    suppress_failed_children(deliveries, conversations, now);
    for delivery in deliveries.iter_mut() {
        let Some(report) = delivery.report.as_mut().filter(|report| report.status == ReportStatus::Running) else { continue };
        let Some(source) = conversations.iter().find(|item| item.id == delivery.source_conversation_id) else { continue };
        let Some(status) = report_status(source.run.status).filter(|_| report.run_id.as_deref() == Some(source.run.id.as_str())) else { continue };
        report.status = status;
        report.finished_at = Some(source.run.finished_at.clone().unwrap_or_else(|| iso(now)));
        report.error =
            source.run.error.as_deref().filter(|error| !error.is_empty() && status != ReportStatus::Completed).map(|error| js_slice(error, 1000).to_owned());
    }
    let mut order: Vec<usize> = deliveries.iter().enumerate().filter(|(_, delivery)| delivery.status.is_pending()).map(|(index, _)| index).collect();
    order.sort_by_key(|index| std::cmp::Reverse(deliveries[*index].hop));
    for index in order {
        let delivery = &deliveries[index];
        let Some(conversation) = conversations.iter().find(|item| item.id == delivery.recipient_conversation_id) else { continue };
        let execution = &conversation.run;
        if execution.status == RunStatus::Idle {
            continue;
        }
        let linked_report = latest_report_run(deliveries, conversation) == Some(execution.id.as_str());
        if delivery.execution_run_id.as_deref().is_some_and(|id| !id.is_empty() && id != execution.id) && !linked_report {
            continue;
        }
        let waiting = children_pending(deliveries, &conversation.id);
        let delivery = &mut deliveries[index];
        if !execution.id.is_empty() {
            delivery.execution_run_id = Some(execution.id.clone());
        }
        if linked_report {
            // Report turns rebind execution, not the first recipient start. Normalize pending
            // prior-PR27 records that used the latest report start instead of the initial input.
            delivery.started_at = conversation.messages.first().map(|message| message.created_at.clone());
        } else if let Some(started_at) = execution.started_at.as_ref().filter(|value| !value.is_empty()) {
            delivery.started_at.get_or_insert_with(|| started_at.clone());
        }
        if execution.status.is_active() {
            delivery.status = if linked_report { DeliveryStatus::WaitingChildren } else { DeliveryStatus::Running };
            continue;
        }
        if execution.status == RunStatus::Completed && waiting {
            delivery.status = DeliveryStatus::WaitingChildren;
            continue;
        }
        let Some(status) = delivery_status(execution.status) else { continue };
        delivery.status = status;
        let finished_at = execution.finished_at.clone().unwrap_or_else(|| iso(now));
        delivery.finished_at = Some(finished_at.clone());
        delivery.error = execution.error.as_deref().filter(|error| !error.is_empty()).map(|error| js_slice(error, 1000).to_owned());
        if status == DeliveryStatus::Completed
            && let Some(answer) = conversation.messages.iter().rev().find(|message| message.role == Role::Assistant)
            && let Some(exchange) = exchanges.iter_mut().find(|item| item.id == delivery.exchange_id)
            && exchange.messages.len() == 1
        {
            exchange.messages.push(BotExchangeMessage {
                id: answer.id.clone(),
                bot_id: delivery.to_bot_id.clone(),
                text: answer.text.clone(),
                created_at: answer.created_at.clone(),
            });
            exchange.updated_at = finished_at;
        }
        let delivery = &deliveries[index];
        let Some(source) = conversations.iter().find(|item| item.id == delivery.source_conversation_id) else { continue };
        if delivery.report.is_some() || !reports_to(deliveries, source, routine_conversation_ids) {
            continue;
        }
        let owner = inbound(deliveries, &source.id);
        let closed_recipient = owner.is_some_and(|owner| owner.status.is_terminal())
            || (owner.is_some() && matches!(source.run.status, RunStatus::Cancelled | RunStatus::Error | RunStatus::Interrupted));
        let terminated = deliveries.iter().find(|root| root.id == delivery.chain_id).and_then(|root| root.chain_termination.as_ref());
        // PR9 could complete a parent while its forwarded child was still queued. Do not add
        // even a skipped recipient report to that closed legacy conversation: the old reader
        // rejects every recipient report. Failed recipients likewise have nothing left to consume.
        if closed_recipient && terminated.is_none() {
            continue;
        }
        let used = deliveries
            .iter()
            .filter(|item| {
                item.chain_id == delivery.chain_id
                    && inbound(deliveries, &item.source_conversation_id).is_none()
                    && item.report.as_ref().is_some_and(|report| report.status != ReportStatus::Skipped)
            })
            .count();
        if terminated.is_some() || (owner.is_none() && used >= MAX_BOT_REPORTS_PER_CHAIN) {
            let error = terminated.and_then(|termination| termination.error.clone());
            skip_report(&mut deliveries[index], error, now);
        } else {
            deliveries[index].report = Some(DeliveryReport::queued());
        }
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

/// A completed report is an immutable historical receipt even if the recipient aggregate later
/// terminates while waiting for another child. Require a canonical root latch or a proven failed
/// start/cancellation of the next linked child report without starting another run.
fn completed_report_aggregate_override(deliveries: &[BotDelivery], delivery: &BotDelivery, report: &DeliveryReport, source: &Conversation) -> bool {
    if report.status != ReportStatus::Completed || report.run_id.as_deref() != Some(source.run.id.as_str()) {
        return false;
    }
    let Some(owner) = inbound(deliveries, &source.id).filter(|owner| owner.chain_id == delivery.chain_id && owner.status.is_terminal()) else { return false };
    if let Some(termination) = deliveries.iter().find(|root| root.id == delivery.chain_id).and_then(|root| root.chain_termination.as_ref()) {
        return source.run.status == termination.status && source.run.error == termination.error && run_status_name(owner.status) == Some(termination.status);
    }
    let status = match (source.run.status, owner.status) {
        (RunStatus::Error, DeliveryStatus::Error) => ReportStatus::Error,
        (RunStatus::Cancelled, DeliveryStatus::Cancelled) => ReportStatus::Cancelled,
        _ => return false,
    };
    deliveries.iter().any(|child| {
        child.chain_id == delivery.chain_id
            && child.source_conversation_id == source.id
            && source.messages.last().is_some_and(|answer| answer.role == Role::Assistant && answer.id == child.source_message_id)
            && child.report.as_ref().is_some_and(|failed| {
                failed.status == status
                    && failed.run_id.is_none()
                    && failed.error.is_some() == (status == ReportStatus::Error)
                    && failed.error == source.run.error
                    && failed.finished_at == source.run.finished_at
            })
    })
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
        status => {
            source.run.id != run_id
                || report_run_status(status) == Some(source.run.status)
                || completed_report_aggregate_override(deliveries, delivery, report, source)
        }
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
        if recipient.group_id.is_some() || recipient.thread.is_some() {
            return false;
        }
        if truthy(&delivery.execution_run_id) {
            // New report turns retain the first input's timestamp. Prior PR27 disks used the
            // latest run start; accept that historical shape without weakening report-run proof.
            let start_matches = delivery.started_at == recipient.run.started_at
                || (recipient.messages.len() > 2 && delivery.started_at.as_deref() == recipient.messages.first().map(|message| message.created_at.as_str()));
            if delivery.execution_run_id.as_deref() != Some(recipient.run.id.as_str())
                || !start_matches
                || recipient.messages.len() < 2
                || recipient.messages.len() % 2 != 0
                || recipient.messages[0].role != Role::User
                || recipient.messages[0].source_bot_id.as_deref() != Some(delivery.from_bot_id.as_str())
                || recipient.messages[0].text != delivery.text
                || recipient.messages[0].report_delivery_ids.is_some()
                || recipient.messages[1].role != Role::Assistant
                || recipient.messages[1].source_bot_id.is_some()
                || recipient.messages[1].report_delivery_ids.is_some()
                || !recipient.messages[2..].as_chunks::<2>().0.iter().all(|pair| {
                    pair[0].role == Role::User
                        && pair[0].report_delivery_ids.is_some()
                        && pair[1].role == Role::Assistant
                        && pair[1].report_delivery_ids.is_none()
                })
                || (recipient.messages.len() > 2
                    && recipient.messages[recipient.messages.len() - 2].report_delivery_ids.as_deref().is_some_and(|ids| !ids.is_empty())
                    && latest_report_run(deliveries, recipient) != Some(recipient.run.id.as_str()))
            {
                return false;
            }
        } else if !recipient.run.id.is_empty() || !recipient.messages.is_empty() || truthy(&delivery.started_at) {
            return false;
        }
        let waiting = children_pending(deliveries, &recipient.id);
        if delivery.status == DeliveryStatus::Running && (!recipient.run.status.is_active() || recipient.messages.len() > 2) {
            return false;
        }
        if delivery.status == DeliveryStatus::WaitingChildren
            && (!waiting
                || !(recipient.run.status == RunStatus::Completed
                    || (recipient.run.status.is_active() && latest_report_run(deliveries, recipient) == Some(recipient.run.id.as_str()))))
        {
            return false;
        }
        // Old PR9 completed parents can legitimately still have queued children. Never replay them.
        if delivery.status == DeliveryStatus::Completed && recipient.messages.len() > 2 && waiting {
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
        if inbound(deliveries, &delivery.source_conversation_id).is_none()
            && delivery.report.as_ref().is_some_and(|report| report.status != ReportStatus::Skipped)
        {
            *reports.entry(delivery.chain_id.as_str()).or_default() += 1;
        }
    }
    if reports.values().any(|count| *count > MAX_BOT_REPORTS_PER_CHAIN) {
        return false;
    }
    let mut reported = HashSet::new();
    conversations.iter().all(|conversation| {
        let mut report_runs = HashSet::new();
        conversation.messages.iter().enumerate().all(|(index, message)| {
            if let Some(ids) = message.report_delivery_ids.as_deref() {
                let next = conversation.messages.get(index + 1);
                let linked: Vec<&BotDelivery> = ids.iter().filter_map(|id| deliveries.iter().find(|delivery| delivery.id == *id)).collect();
                let first = linked.first();
                if linked.len() != ids.len()
                    || first.is_some_and(|first| {
                        !linked.iter().all(|delivery| {
                            (inbound(deliveries, &conversation.id).is_none() || delivery.chain_id == first.chain_id)
                                && delivery.report.as_ref().and_then(|report| report.run_id.as_deref())
                                    == first.report.as_ref().and_then(|report| report.run_id.as_deref())
                        }) || !report_runs.insert(first.report.as_ref().and_then(|report| report.run_id.as_deref()))
                    })
                    || message.role != Role::User
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
    /// starting a run or changing its messages. A waiting recipient aggregate is cancelled too:
    /// its provisional answer must never become final because a mandatory report was cancelled.
    /// Root conversations and running/terminal reports are unchanged. Allowed while the app is
    /// busy; a no-op does not write state. Failed-source reconciliation suppresses descendants.
    pub fn cancel_bot_reports(&mut self, conversation_id: &str) -> HttpResult<()> {
        self.assert_writable()?;
        self.conversation(conversation_id)?;
        let ids: HashSet<String> = queued_reports(&self.disk.bot_deliveries, conversation_id).iter().map(|delivery| delivery.id.clone()).collect();
        if ids.is_empty() {
            return Ok(());
        }
        let waiting = inbound(&self.disk.bot_deliveries, conversation_id).is_some_and(|delivery| delivery.status == DeliveryStatus::WaitingChildren);
        let finished_at = iso(self.now());
        for delivery in self.disk_mut().bot_deliveries.iter_mut().filter(|delivery| ids.contains(&delivery.id)) {
            delivery.report = Some(DeliveryReport { status: ReportStatus::Cancelled, finished_at: Some(finished_at.clone()), ..DeliveryReport::queued() });
        }
        if waiting {
            let conversation = self.conversation_mut(conversation_id)?;
            conversation.run.status = RunStatus::Cancelled;
            conversation.run.error = None;
            conversation.run.finished_at = Some(finished_at.clone());
            conversation.run.permission = None;
            conversation.updated_at = finished_at;
        }
        Ok(())
    }

    /// Resolve Stop ownership without coupling the current user run to unrelated historical work.
    /// An idle Stop (`source_message_id == None`) also includes outstanding outbound work.
    pub fn bot_chain_ids_for_cancel(&self, conversation_id: &str, source_message_id: Option<&str>, report_delivery_ids: &[String]) -> HashSet<String> {
        self.disk
            .bot_deliveries
            .iter()
            .filter(|delivery| {
                (delivery.recipient_conversation_id == conversation_id && delivery.status.is_pending())
                    || (delivery.source_conversation_id == conversation_id
                        && match source_message_id {
                            Some(message_id) => delivery.source_message_id == message_id,
                            None => delivery.status.is_pending() || report_pending(delivery),
                        })
                    || report_delivery_ids.contains(&delivery.id)
            })
            .map(|delivery| delivery.chain_id.clone())
            .collect()
    }

    /// Whole-chain wall clock starts at the earliest admitted delivery, including while waiting on
    /// permissions or queued reports. Finished historical chains do not expire retroactively.
    pub fn expired_bot_chains(&self, now: DateTime<Utc>) -> HashSet<String> {
        self.disk
            .bot_deliveries
            .iter()
            .filter(|delivery| delivery.status.is_pending() || report_pending(delivery))
            .filter(|delivery| chain_expired(&self.disk.bot_deliveries, &delivery.chain_id, now))
            .map(|delivery| delivery.chain_id.clone())
            .collect()
    }

    pub(crate) fn assert_bot_chain_open(&self, chain_id: &str, now: DateTime<Utc>) -> HttpResult<()> {
        if let Some(termination) = self.disk.bot_deliveries.iter().find(|delivery| delivery.id == chain_id).and_then(|root| root.chain_termination.as_ref()) {
            return Err(HttpError::conflict(termination.error.as_deref().unwrap_or("This bot message chain was stopped.")));
        }
        if chain_expired(&self.disk.bot_deliveries, chain_id, now) {
            return Err(HttpError::conflict(BOT_CHAIN_TIMEOUT_ERROR));
        }
        Ok(())
    }

    /// Latch first, then terminate only non-active owned recipient runs and suppress queued reports.
    /// Runtime stops active engines and calls again after finish; reconciliation/restart also
    /// enforces the canonical first receipt. Repeated calls do not overwrite historical results.
    pub fn terminate_bot_chains(&mut self, chain_ids: &HashSet<String>, status: RunStatus, error: Option<&str>, now: DateTime<Utc>) -> HttpResult<()> {
        self.assert_writable()?;
        if !matches!(status, RunStatus::Cancelled | RunStatus::Error) {
            return Err(HttpError::bad_request("Bot chains may only be cancelled or failed."));
        }
        let error = error.map(|error| js_slice(&self.redact(error), 1000).to_owned()).or_else(|| Some("This bot message chain was stopped.".into()));
        let mut changed = false;
        for root in self.disk.bot_deliveries.iter_mut().filter(|delivery| delivery.id == delivery.chain_id && chain_ids.contains(&delivery.chain_id)) {
            if root.chain_termination.is_none() {
                root.chain_termination = Some(ChainTermination { status, finished_at: iso(now), error: error.clone() });
                changed = true;
            }
        }
        changed |= enforce_chain_terminations(&mut self.disk.bot_deliveries, &mut self.disk.conversations, now);
        self.dirty |= changed;
        Ok(())
    }

    /// Fail only the oldest SAME-chain queued reports, matching Runtime's report claim. If this
    /// recipient was waiting, fail its handling run too: the initial answer is not a final summary.
    pub fn fail_bot_reports(&mut self, conversation_id: &str, error: &str, now: DateTime<Utc>) -> HttpResult<()> {
        self.assert_writable()?;
        let queued = queued_reports(&self.disk.bot_deliveries, conversation_id);
        let Some(first) = queued.first() else { return Ok(()) };
        let ids: HashSet<String> = queued.iter().filter(|delivery| delivery.chain_id == first.chain_id).map(|delivery| delivery.id.clone()).collect();
        let waiting = inbound(&self.disk.bot_deliveries, conversation_id).is_some_and(|delivery| delivery.status == DeliveryStatus::WaitingChildren);
        let error = js_slice(&self.redact(error), 1000).to_owned();
        let finished_at = iso(now);
        for delivery in self.disk_mut().bot_deliveries.iter_mut().filter(|delivery| ids.contains(&delivery.id)) {
            delivery.report =
                Some(DeliveryReport { status: ReportStatus::Error, run_id: None, finished_at: Some(finished_at.clone()), error: Some(error.clone()) });
        }
        if waiting {
            let conversation = self.conversation_mut(conversation_id)?;
            conversation.run.status = RunStatus::Error;
            conversation.run.error = Some(error);
            conversation.run.finished_at = Some(finished_at.clone());
            conversation.run.permission = None;
            conversation.updated_at = finished_at;
        }
        Ok(())
    }
}
