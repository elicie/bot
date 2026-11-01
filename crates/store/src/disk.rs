//! `state.json` loading: the `diskSchema` checks serde cannot express (formats, bounds, schema trim
//! transforms) and the reference validation in `Store.open`.

use std::collections::HashSet;

use grok_bot_model::{DiskState, Routine, RoutineResult, RoutineRun, RunStatus};

use crate::bot_messages::{MAX_BOT_DELIVERIES, delivery_valid, exchange_valid, valid_bot_message_references};
use crate::schedule::schedule_issues;
use crate::skills::{MAX_SKILLS_PER_BOT, persisted_skill_valid, validate_skill_set};
use crate::validate::{Issues, is_timestamp, is_uuid, js_len, js_trim, parse_ms};
use crate::{MAX_ROUTINE_RUNS, MAX_ROUTINES, base_url};

fn routine_valid(routine: &mut Routine) -> bool {
    let mut issues = Issues::new();
    issues.uuid("botId", &routine.bot_id);
    routine.name = js_trim(&routine.name).to_owned();
    issues.length("name", &routine.name, 1, 100);
    routine.prompt = js_trim(&routine.prompt).to_owned();
    issues.length("prompt", &routine.prompt, 1, 100000);
    schedule_issues(&mut issues, &mut routine.schedule);
    issues.is_empty()
        && is_uuid(&routine.id)
        && routine.next_run_at.as_deref().is_none_or(is_timestamp)
        && is_timestamp(&routine.created_at)
        && is_timestamp(&routine.updated_at)
}

fn result_valid(result: &RoutineResult) -> bool {
    result.started_at.as_deref().is_none_or(is_timestamp) && is_timestamp(&result.finished_at)
}

fn run_valid(run: &RoutineRun) -> bool {
    [&run.id, &run.routine_id, &run.bot_id, &run.conversation_id].iter().all(|id| is_uuid(id))
        && (1..=100).contains(&js_len(&run.routine_name))
        && (1..=100000).contains(&js_len(&run.prompt))
        && is_timestamp(&run.scheduled_for)
        && is_timestamp(&run.created_at)
        && run.result.as_ref().is_none_or(result_valid)
}

/// `diskSchema.parse` beyond the JSON shape. Applies the schema's transforms in place.
pub(crate) fn schema_valid(disk: &mut DiskState) -> bool {
    crate::groups::normalize(disk);
    if disk.version != 1
        || disk.bots.is_empty()
        || disk.routines.len() > MAX_ROUTINES
        || disk.routine_runs.len() > MAX_ROUTINE_RUNS
        || disk.bot_exchanges.len() > MAX_BOT_DELIVERIES
        || disk.bot_deliveries.len() > MAX_BOT_DELIVERIES
    {
        return false;
    }
    for bot in &mut disk.bots {
        if !is_uuid(&bot.id) {
            return false;
        }
        if let Some(skills) = bot.skills.as_mut()
            && (skills.len() > MAX_SKILLS_PER_BOT || !skills.iter_mut().all(persisted_skill_valid))
        {
            return false;
        }
    }
    let conversations = disk.conversations.iter().all(|conversation| {
        is_uuid(&conversation.id)
            && is_uuid(&conversation.bot_id)
            && conversation.messages.iter().all(|message| {
                is_uuid(&message.id)
                    && message.bot_exchange_ids.as_ref().is_none_or(|ids| ids.len() <= 1 && ids.iter().all(|id| is_uuid(id)))
                    && message.source_bot_id.as_deref().is_none_or(is_uuid)
            })
    });
    conversations
        && crate::groups::schema_valid(disk)
        && disk.routines.iter_mut().all(routine_valid)
        && disk.routine_runs.iter().all(run_valid)
        && disk.bot_exchanges.iter().all(exchange_valid)
        && disk.bot_deliveries.iter_mut().all(delivery_valid)
}

fn truthy(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(|text| !text.is_empty())
}

/// The semantic checks `Store.open` runs after parsing. `Err` carries the reason.
pub(crate) fn references_valid(disk: &DiskState) -> Result<(), String> {
    if !disk.settings.base_url.is_empty() {
        base_url(&disk.settings.base_url).map_err(|error| error.message)?;
    }
    for bot in &disk.bots {
        validate_skill_set(bot.skills.as_deref().unwrap_or_default()).map_err(|error| error.message)?;
    }
    let bot_ids: HashSet<&str> = disk.bots.iter().map(|bot| bot.id.as_str()).collect();
    let conversation_ids: HashSet<&str> = disk.conversations.iter().map(|conversation| conversation.id.as_str()).collect();
    let entity_ids: Vec<&str> = bot_ids
        .iter()
        .copied()
        .chain(conversation_ids.iter().copied())
        .chain(disk.routines.iter().map(|routine| routine.id.as_str()))
        .chain(disk.routine_runs.iter().map(|run| run.id.as_str()))
        .chain(disk.bot_exchanges.iter().map(|exchange| exchange.id.as_str()))
        .chain(disk.bot_deliveries.iter().map(|delivery| delivery.id.as_str()))
        .collect();
    let unique_ids: HashSet<&str> = entity_ids.iter().copied().collect();
    let mut linked_conversations: HashSet<&str> = HashSet::new();
    let mut pending_routines: HashSet<&str> = HashSet::new();
    let mut invalid_runs = false;
    for run in &disk.routine_runs {
        let conversation = disk.conversations.iter().find(|item| item.id == run.conversation_id);
        let routine = disk.routines.iter().find(|item| item.id == run.routine_id);
        let linked_execution =
            conversation.is_some_and(|conversation| !truthy(&run.execution_run_id) || run.execution_run_id.as_deref() == Some(conversation.run.id.as_str()));
        let pending = run.result.is_none()
            && linked_execution
            && conversation.is_some_and(|conversation| conversation.run.status == RunStatus::Idle || conversation.run.status.is_active());
        let invalid = !bot_ids.contains(run.bot_id.as_str())
            || conversation.is_none_or(|conversation| conversation.bot_id != run.bot_id)
            || linked_conversations.contains(run.conversation_id.as_str())
            || (run.result.is_none() && !linked_execution)
            || (pending && (routine.is_none() || pending_routines.contains(run.routine_id.as_str())));
        linked_conversations.insert(run.conversation_id.as_str());
        if pending {
            pending_routines.insert(run.routine_id.as_str());
        }
        if invalid {
            invalid_runs = true;
            break;
        }
    }
    let invalid = bot_ids.len() != disk.bots.len()
        || conversation_ids.len() != disk.conversations.len()
        || unique_ids.len() != entity_ids.len()
        || disk
            .routine_runs
            .iter()
            .any(|run| unique_ids.contains(run.routine_id.as_str()) && !disk.routines.iter().any(|routine| routine.id == run.routine_id))
        || disk.conversations.iter().any(|conversation| !bot_ids.contains(conversation.bot_id.as_str()) && !crate::groups::is_group_conversation(conversation))
        || !crate::groups::references_valid(disk)
        || disk.routines.iter().any(|routine| {
            !bot_ids.contains(routine.bot_id.as_str())
                || (routine.enabled && !truthy(&routine.next_run_at))
                || match (&routine.schedule, routine.next_run_at.as_deref().filter(|value| !value.is_empty())) {
                    (grok_bot_model::RoutineSchedule::Once { at }, Some(next)) => {
                        let (next, at) = (parse_ms(next), parse_ms(at));
                        next.is_none() || at.is_none() || next != at
                    }
                    _ => false,
                }
        })
        || invalid_runs
        || !valid_bot_message_references(&disk.bot_deliveries, &disk.bot_exchanges, &disk.conversations, &bot_ids, &linked_conversations);
    if invalid {
        return Err("Application state contains invalid references.".into());
    }
    Ok(())
}
