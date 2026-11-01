//! Public snapshot redaction. Port of the `JSON.stringify` replacer in `Store.snapshot()`: every
//! string has the API key replaced by `[redacted]` except
//! - `kind`, `status`, `role`, `color`, and `hash` fields,
//! - identifier fields (`id`, `botId`, `routineId`, ...) whose value is a UUID or 64-hex digest,
//! - the elements of bot exchange `botIds` and message `botExchangeIds` arrays.
//!
//! Enum-typed fields outside that list (`shape`, `trigger`) are kept as-is: the Node replacer would
//! rewrite them only if the key were a substring of the enum name, which a typed snapshot cannot
//! represent and which cannot reveal the key.

use std::sync::LazyLock;

use grok_bot_model::{AppState, BotDelivery, BotExchange, Conversation, Routine, RoutineRun, RoutineSchedule};
use regex::Regex;

static IDENTIFIER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}|[a-f0-9]{64})$").unwrap());

struct Redactor<'a> {
    key: &'a str,
}

impl Redactor<'_> {
    fn text(&self, value: &mut String) {
        if value.contains(self.key) {
            *value = value.replace(self.key, "[redacted]");
        }
    }

    fn optional(&self, value: &mut Option<String>) {
        if let Some(value) = value {
            self.text(value);
        }
    }

    fn id(&self, value: &mut String) {
        if !IDENTIFIER.is_match(value) {
            self.text(value);
        }
    }

    fn optional_id(&self, value: &mut Option<String>) {
        if let Some(value) = value {
            self.id(value);
        }
    }

    fn conversation(&self, conversation: &mut Conversation) {
        self.id(&mut conversation.id);
        self.id(&mut conversation.bot_id);
        self.text(&mut conversation.title);
        for message in &mut conversation.messages {
            self.id(&mut message.id);
            self.text(&mut message.text);
            self.text(&mut message.created_at);
            for tool in &mut message.tools {
                self.id(&mut tool.id);
                self.text(&mut tool.title);
                self.optional(&mut tool.input);
                self.optional(&mut tool.output);
                for path in tool.paths.iter_mut().flatten() {
                    self.text(path);
                }
            }
            self.optional_id(&mut message.source_bot_id);
            for attachment in message.attachments.iter_mut().flatten() {
                self.id(&mut attachment.id);
                self.text(&mut attachment.name);
            }
            self.optional_id(&mut message.speaker_bot_id);
        }
        self.optional_id(&mut conversation.session_id);
        self.optional_id(&mut conversation.session_fingerprint);
        let run = &mut conversation.run;
        self.id(&mut run.id);
        self.optional(&mut run.error);
        if let Some(permission) = run.permission.as_mut() {
            self.id(&mut permission.id);
            self.text(&mut permission.title);
            self.text(&mut permission.description);
            for option in &mut permission.options {
                self.id(&mut option.id);
                self.text(&mut option.label);
            }
            for path in &mut permission.paths {
                self.text(path);
            }
        }
        self.optional(&mut run.started_at);
        self.optional(&mut run.finished_at);
        self.text(&mut conversation.updated_at);
    }

    fn routine(&self, routine: &mut Routine) {
        self.id(&mut routine.id);
        self.id(&mut routine.bot_id);
        self.text(&mut routine.name);
        self.text(&mut routine.prompt);
        match &mut routine.schedule {
            RoutineSchedule::Once { at } => self.text(at),
            RoutineSchedule::Interval { .. } => {}
            RoutineSchedule::Daily { time, timezone } => {
                self.text(time);
                self.text(timezone);
            }
        }
        self.optional(&mut routine.next_run_at);
        self.text(&mut routine.created_at);
        self.text(&mut routine.updated_at);
    }

    fn routine_run(&self, run: &mut RoutineRun) {
        self.id(&mut run.id);
        self.id(&mut run.routine_id);
        self.text(&mut run.routine_name);
        self.id(&mut run.bot_id);
        self.id(&mut run.conversation_id);
        self.text(&mut run.prompt);
        self.text(&mut run.scheduled_for);
        self.text(&mut run.created_at);
        self.optional_id(&mut run.execution_run_id);
        if let Some(result) = run.result.as_mut() {
            self.optional(&mut result.started_at);
            self.text(&mut result.finished_at);
            self.optional(&mut result.error);
        }
    }

    fn exchange(&self, exchange: &mut BotExchange) {
        self.id(&mut exchange.id);
        for message in &mut exchange.messages {
            self.id(&mut message.id);
            self.id(&mut message.bot_id);
            self.text(&mut message.text);
            self.text(&mut message.created_at);
        }
        self.id(&mut exchange.source_conversation_id);
        self.id(&mut exchange.source_message_id);
        self.text(&mut exchange.updated_at);
    }

    fn delivery(&self, delivery: &mut BotDelivery) {
        for id in [
            &mut delivery.id,
            &mut delivery.exchange_id,
            &mut delivery.source_conversation_id,
            &mut delivery.source_message_id,
            &mut delivery.recipient_conversation_id,
            &mut delivery.from_bot_id,
            &mut delivery.to_bot_id,
            &mut delivery.chain_id,
        ] {
            self.id(id);
        }
        self.text(&mut delivery.text);
        self.text(&mut delivery.created_at);
        self.optional(&mut delivery.request_id);
        self.optional_id(&mut delivery.execution_run_id);
        self.optional(&mut delivery.started_at);
        self.optional(&mut delivery.finished_at);
        self.optional(&mut delivery.error);
    }

    fn state(&self, state: &mut AppState) {
        for bot in &mut state.bots {
            self.id(&mut bot.id);
            self.text(&mut bot.name);
            self.text(&mut bot.instructions);
            for skill in bot.skills.iter_mut().flatten() {
                self.id(&mut skill.id);
                self.text(&mut skill.name);
                self.text(&mut skill.description);
                self.text(&mut skill.instructions);
                self.text(&mut skill.created_at);
                self.text(&mut skill.updated_at);
            }
            self.text(&mut bot.created_at);
            for memory in bot.memories.iter_mut().flatten() {
                self.id(&mut memory.id);
                self.text(&mut memory.text);
                self.text(&mut memory.created_at);
            }
            for file in bot.knowledge.iter_mut().flatten() {
                self.id(&mut file.id);
                self.text(&mut file.name);
                self.text(&mut file.created_at);
            }
        }
        state.conversations.iter_mut().for_each(|conversation| self.conversation(conversation));
        state.routines.iter_mut().for_each(|routine| self.routine(routine));
        state.routine_runs.iter_mut().for_each(|run| self.routine_run(run));
        state.bot_exchanges.iter_mut().for_each(|exchange| self.exchange(exchange));
        state.bot_deliveries.iter_mut().for_each(|delivery| self.delivery(delivery));
        for group in &mut state.groups {
            self.id(&mut group.id);
            self.text(&mut group.name);
            group.member_bot_ids.iter_mut().for_each(|id| self.id(id));
            self.text(&mut group.created_at);
            self.text(&mut group.updated_at);
        }
        self.optional(&mut state.routine_scheduling_error);
        self.text(&mut state.settings.base_url);
        self.text(&mut state.settings.model);
        self.text(&mut state.settings.workspace);
        self.optional(&mut state.runtime.version);
        self.optional(&mut state.runtime.error);
    }
}

/// Replace every occurrence of `key` (when non-empty) in the snapshot's redactable strings.
pub(crate) fn redact_state(state: &mut AppState, key: &str) {
    if !key.is_empty() {
        Redactor { key }.state(state);
    }
}
