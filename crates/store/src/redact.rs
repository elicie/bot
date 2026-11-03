//! Public snapshot redaction. Port of the `JSON.stringify` replacer in `Store.snapshot()`: every
//! string has the API key and every vault secret value replaced by `[redacted]` except
//! - `kind`, `status`, `role`, `color`, and `hash` fields,
//! - identifier fields (`id`, `botId`, `routineId`, ...) whose value is a UUID or 64-hex digest,
//! - the elements of bot exchange `botIds` and message `botExchangeIds` arrays,
//! - app-generated canvas version timestamps (structural metadata, not editable text).
//!
//! Enum-typed fields outside that list (`shape`, `trigger`) are kept as-is: the Node replacer would
//! rewrite them only if the key were a substring of the enum name, which a typed snapshot cannot
//! represent and which cannot reveal the key.

use std::sync::LazyLock;

use grok_bot_model::{AppState, BotDelivery, BotExchange, Conversation, McpApp, QueuedWork, Routine, RoutineRun, RoutineSchedule, WorkKind, WorkerBatch};
use regex::Regex;

static IDENTIFIER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}|[a-f0-9]{64})$").unwrap());

struct Redactor<'a> {
    /// The API key and every vault secret value, longest first, none empty.
    keys: &'a [String],
}

impl Redactor<'_> {
    fn text(&self, value: &mut String) {
        for key in self.keys {
            if value.contains(key.as_str()) {
                *value = value.replace(key.as_str(), "[redacted]");
            }
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

    /// App arguments are JSON encoded inside a string. Redact decoded keys and string values too,
    /// so credentials with quotes/backslashes or Unicode escapes cannot survive that encoding.
    fn encoded_json(&self, text: &mut String) {
        if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(text)
            && value.is_object()
        {
            let original = value.clone();
            if !self.json_value(&mut value) {
                // Redacted keys collided. Do not silently merge argument fields or leave secrets.
                *text = "{}".into();
            } else if value != original {
                *text = value.to_string();
            }
        } else {
            // Only object arguments are durable. Invalid in-memory data is never forwarded.
            *text = "{}".into();
        }
        self.text(text);
    }

    /// False when redacting object keys would conflate distinct fields.
    fn json_value(&self, value: &mut serde_json::Value) -> bool {
        match value {
            serde_json::Value::String(text) => self.text(text),
            serde_json::Value::Array(items) => return items.iter_mut().all(|item| self.json_value(item)),
            serde_json::Value::Object(fields) => {
                let mut redacted = serde_json::Map::new();
                for (mut key, mut value) in std::mem::take(fields) {
                    self.text(&mut key);
                    if !self.json_value(&mut value) || redacted.insert(key, value).is_some() {
                        return false;
                    }
                }
                *fields = redacted;
            }
            _ => {}
        }
        true
    }

    fn mcp_app(&self, app: &mut McpApp) {
        self.id(&mut app.bot_id);
        self.id(&mut app.server_id);
        self.text(&mut app.server_updated_at);
        self.text(&mut app.server);
        self.text(&mut app.tool);
        self.text(&mut app.uri);
        self.text(&mut app.html);
        self.encoded_json(&mut app.input);
        self.text(&mut app.output);
    }

    fn conversation(&self, conversation: &mut Conversation) {
        self.id(&mut conversation.id);
        self.id(&mut conversation.bot_id);
        self.text(&mut conversation.title);
        if let Some(canvas) = &mut conversation.canvas {
            canvas.redacted |= crate::canvas::contains_secret(self.keys, &[&canvas.title, &canvas.language, &canvas.content]);
            self.text(&mut canvas.title);
            self.text(&mut canvas.language);
            self.text(&mut canvas.content);
            for version in &mut canvas.history {
                version.redacted |= crate::canvas::version_has_secret(version, self.keys);
                self.text(&mut version.title);
                self.text(&mut version.language);
                self.text(&mut version.content);
            }
        }
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
                if let Some(app) = &mut tool.mcp_app {
                    self.mcp_app(app);
                }
            }
            self.optional_id(&mut message.source_bot_id);
            for attachment in message.attachments.iter_mut().flatten() {
                self.id(&mut attachment.id);
                self.text(&mut attachment.name);
            }
            self.optional_id(&mut message.speaker_bot_id);
            message.report_delivery_ids.iter_mut().flatten().for_each(|id| self.id(id));
        }
        self.optional_id(&mut conversation.session_id);
        self.optional_id(&mut conversation.session_fingerprint);
        let run = &mut conversation.run;
        self.id(&mut run.id);
        self.optional_id(&mut run.bot_id);
        self.optional(&mut run.error);
        for todo in run.todos.iter_mut().flatten() {
            self.text(&mut todo.content);
        }
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
        if let Some(report) = delivery.report.as_mut() {
            self.optional_id(&mut report.run_id);
            self.optional(&mut report.finished_at);
            self.optional(&mut report.error);
        }
        if let Some(termination) = delivery.chain_termination.as_mut() {
            self.text(&mut termination.finished_at);
            self.optional(&mut termination.error);
        }
    }

    fn worker_batch(&self, batch: &mut WorkerBatch) {
        for id in [&mut batch.id, &mut batch.bot_id, &mut batch.parent_conversation_id, &mut batch.source_message_id, &mut batch.source_run_id] {
            self.id(id);
        }
        self.text(&mut batch.created_at);
        self.optional(&mut batch.released_at);
        self.optional(&mut batch.authorized_workspace);
        self.optional_id(&mut batch.report_input_id);
        self.optional_id(&mut batch.report_message_id);
        for worker in &mut batch.workers {
            self.id(&mut worker.id);
            self.id(&mut worker.conversation_id);
            self.text(&mut worker.task);
            self.text(&mut worker.created_at);
            self.optional_id(&mut worker.execution_run_id);
            self.optional(&mut worker.started_at);
            self.optional(&mut worker.finished_at);
            self.optional(&mut worker.error);
            self.optional(&mut worker.answer);
            if let Some(answer) = &mut worker.answer {
                *answer = crate::workers::bounded_answer(answer);
            }
        }
        if let Some(report) = &mut batch.report {
            self.optional_id(&mut report.run_id);
            self.optional(&mut report.finished_at);
            self.optional(&mut report.error);
        }
    }

    fn queued_work(&self, work: &mut QueuedWork) {
        self.id(&mut work.id);
        self.id(&mut work.bot_id);
        work.speaker_bot_ids.iter_mut().flatten().for_each(|id| self.id(id));
        self.id(&mut work.conversation_id);
        self.text(&mut work.created_at);
        match &mut work.kind {
            WorkKind::User { text, attachments } => {
                self.text(text);
                for attachment in attachments {
                    self.id(&mut attachment.id);
                    self.text(&mut attachment.name);
                    self.text(&mut attachment.mime_type);
                }
            }
            WorkKind::Routine { run_id } => self.id(run_id),
            WorkKind::Delivery { delivery_id } | WorkKind::Report { delivery_id } => self.id(delivery_id),
            WorkKind::Group { round_id } => self.id(round_id),
            WorkKind::Worker { worker_id } => self.id(worker_id),
            WorkKind::WorkerReport { batch_id } => self.id(batch_id),
        }
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
        state.worker_batches.iter_mut().for_each(|batch| self.worker_batch(batch));
        state.work_queue.iter_mut().for_each(|work| self.queued_work(work));
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
        for secret in &mut state.secrets {
            self.text(&mut secret.name);
        }
        for server in &mut state.mcp_servers {
            self.text(&mut server.name);
        }
    }
}

/// Replace every occurrence of each key (`vault::redaction_keys`: the API key and every vault
/// secret value) in the snapshot's redactable strings.
pub(crate) fn redact_state(state: &mut AppState, keys: &[String]) {
    let keys: Vec<String> = keys.iter().filter(|key| !key.is_empty()).cloned().collect();
    if !keys.is_empty() {
        Redactor { keys: &keys }.state(state);
    }
}
