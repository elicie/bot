//! Durable admission and fair dispatch. The command queue serializes mutations; Bot slots
//! belong only to live engine turns. Waiting chain parents and future group members hold none.

use std::collections::HashSet;

use grok_bot_model::{HttpError, HttpResult, QueuedWork, WorkKind};
use grok_bot_store::attachments::{PreparedAttachment, discard_attachments, read_attachment, write_attachments};

use super::{Runtime, Turn};

impl Runtime {
    pub(super) async fn accept_prompt(&self, conversation_id: &str, prompt: &str, attachments: Vec<PreparedAttachment>) -> HttpResult<()> {
        self.store.read(|state| self.check_admission(state))?;
        self.expire_bot_chains(self.store.now()).await?;
        // Keep the existing direct-runtime entry point useful for reserved routine/delivery
        // callers: their durable claim already occupies its place in the same FIFO.
        let reserved = self.store.read(|state| state.is_reserved_conversation(conversation_id));
        if reserved {
            let waiting = self.store.read(|state| {
                state
                    .disk()
                    .work_queue
                    .iter()
                    .any(|work| work.conversation_id == conversation_id && matches!(work.kind, WorkKind::Routine { .. } | WorkKind::Delivery { .. }))
            });
            if !waiting {
                return Err(HttpError::conflict("This bot delivery is no longer waiting for that turn."));
            }
        } else {
            let metas = attachments.iter().map(|attachment| attachment.meta.clone()).collect::<Vec<_>>();
            let directory = self.store.data_dir().to_path_buf();
            let (write_directory, write_conversation, write_metas) = (directory.clone(), conversation_id.to_owned(), metas.clone());
            tokio::task::spawn_blocking(move || {
                let written = write_attachments(&write_directory, &write_conversation, &attachments);
                if written.is_err() {
                    discard_attachments(&write_directory, &write_conversation, &write_metas);
                }
                written
            })
            .await
            .map_err(|_| HttpError::new(500, "Attachments could not be stored."))??;
            let accepted = self
                .store
                .mutate(|state| {
                    self.check_admission(state)?;
                    state.enqueue_user_turn(conversation_id, prompt, metas.clone())
                })
                .await;
            if let Err(error) = accepted {
                discard_attachments(&directory, conversation_id, &metas);
                return Err(error);
            }
        }
        self.settled.notify_one();
        self.dispatch_available().await
    }

    fn check_admission(&self, state: &grok_bot_store::State) -> HttpResult<()> {
        use std::sync::atomic::Ordering;
        state.assert_writable()?;
        if self.closing.load(Ordering::SeqCst) {
            return Err(HttpError::unavailable("The server is shutting down."));
        }
        if self.is_paused() {
            return Err(HttpError::unavailable("The server is paused for update maintenance."));
        }
        if !state.runtime().available {
            return Err(HttpError::unavailable(state.runtime().error.as_deref().unwrap_or("Grok Build is unavailable.")));
        }
        if self.origin.lock().unwrap_or_else(|p| p.into_inner()).is_empty() {
            return Err(HttpError::unavailable("Runtime is not connected to the local server."));
        }
        if state.settings().base_url.is_empty() || state.settings().model.is_empty() {
            return Err(HttpError::bad_request("Configure a provider and model in settings first."));
        }
        Ok(())
    }

    pub(super) async fn dispatch_conversation(&self, conversation_id: &str, group: bool) -> HttpResult<()> {
        self.store.read(|state| {
            self.check_admission(state)?;
            let waiting = state.disk().work_queue.iter().any(|work| {
                work.conversation_id == conversation_id
                    && if group { matches!(work.kind, WorkKind::Group { .. }) } else { matches!(work.kind, WorkKind::Report { .. }) }
            });
            if waiting { Ok(()) } else { Err(HttpError::conflict("No queued turn is waiting in this conversation.")) }
        })?;
        self.dispatch_available().await
    }

    /// Fill available capacity, taking one ready FIFO head for each unoccupied Bot.
    /// All callers share this path, so a direct user request cannot bypass older routine work.
    pub async fn dispatch_available(&self) -> HttpResult<()> {
        let _admission = self.admission.lock().await;
        self.expire_bot_chains(self.store.now()).await?;
        self.store
            .mutate(|state| {
                state.sync_work_queue();
                Ok(())
            })
            .await?;
        loop {
            if self.closing.load(std::sync::atomic::Ordering::SeqCst) || self.is_paused() || self.mcp_app_busy.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(());
            }
            let work = self.store.read(|state| {
                if self.runs().len() >= state.settings().max_concurrent_runs() {
                    return None;
                }
                let mut heads = HashSet::new();
                state
                    .disk()
                    .work_queue
                    .iter()
                    .find(|work| {
                        let Ok(conversation) = state.conversation(&work.conversation_id) else { return false };
                        // A later user round depends on finishing this conversation's prior round;
                        // it must not reserve the speaker ahead of those unfinished member turns.
                        if matches!(work.kind, WorkKind::User { .. }) && conversation.group_round.is_some() {
                            return false;
                        }
                        if !heads.insert(work.bot_id.as_str()) {
                            return false;
                        }
                        // A finishing runtime still owns its slot until its final write and cleanup.
                        let live = self.runs().iter().any(|run| run.bot.id == work.bot_id || run.conversation_id == work.conversation_id);
                        !live && state.can_start(&work.bot_id, &work.conversation_id)
                    })
                    .cloned()
            });
            let Some(work) = work else {
                // A final snapshot can wake a caller just before finish removes its registry
                // entry. Do not leave a direct Runtime caller's new work stranded in that gap.
                let finishing = self.store.read(|state| {
                    self.runs()
                        .iter()
                        .find(|run| {
                            run.flags().finishing && state.conversation(&run.conversation_id).is_ok_and(|conversation| !conversation.run.status.is_active())
                        })
                        .cloned()
                });
                if let Some(run) = finishing {
                    let mut done = run.done.subscribe();
                    let _ = done.wait_for(|done| *done).await;
                    continue;
                }
                return Ok(());
            };
            let result = self.dispatch_work(&work).await;
            if let Err(error) = result {
                // A maintenance latch or isolated call can win between selection and claim.
                // Leave accepted work durable rather than turning a temporary slot into an error.
                if self.closing.load(std::sync::atomic::Ordering::SeqCst) || self.is_paused() || self.mcp_app_busy.load(std::sync::atomic::Ordering::SeqCst) {
                    return Ok(());
                }
                let waiting = self.store.read(|state| state.disk().work_queue.iter().any(|item| item.id == work.id));
                if waiting {
                    self.fail_queued_work(&work, &error.message).await?;
                }
            }
        }
    }

    async fn dispatch_work(&self, work: &QueuedWork) -> HttpResult<()> {
        let (turn, attachments) = match &work.kind {
            WorkKind::User { text, attachments } => {
                let directory = self.store.data_dir().to_path_buf();
                let attachments = attachments
                    .iter()
                    .map(|meta| {
                        let bytes = read_attachment(&directory, &work.conversation_id, &meta.id)?;
                        if bytes.len() as u64 != meta.size {
                            return Err(HttpError::bad_request("A queued attachment changed on disk."));
                        }
                        Ok(PreparedAttachment { meta: meta.clone(), bytes })
                    })
                    .collect::<HttpResult<Vec<_>>>()?;
                (Turn::Prompt(text.clone()), attachments)
            }
            WorkKind::Routine { run_id } => {
                let prompt = self
                    .store
                    .read(|state| state.disk().routine_runs.iter().find(|run| run.id == *run_id).map(|run| run.prompt.clone()))
                    .ok_or_else(|| HttpError::conflict("The queued routine no longer exists."))?;
                (Turn::Prompt(prompt), Vec::new())
            }
            WorkKind::Delivery { delivery_id } => {
                let prompt = self
                    .store
                    .read(|state| state.disk().bot_deliveries.iter().find(|delivery| delivery.id == *delivery_id).map(|delivery| delivery.text.clone()))
                    .ok_or_else(|| HttpError::conflict("The queued delivery no longer exists."))?;
                (Turn::Prompt(prompt), Vec::new())
            }
            WorkKind::Group { .. } => (Turn::Group, Vec::new()),
            WorkKind::Report { .. } => (Turn::Report, Vec::new()),
        };
        self.start_turn(&work.conversation_id, turn, attachments, &work.id).await
    }

    async fn fail_queued_work(&self, work: &QueuedWork, error: &str) -> HttpResult<()> {
        self.store
            .mutate(|state| {
                state.assert_writable()?;
                let now = state.now();
                match &work.kind {
                    WorkKind::Routine { run_id } => state.fail_routine_run(run_id, error, now)?,
                    WorkKind::Delivery { delivery_id } => state.fail_bot_delivery(delivery_id, error, now)?,
                    WorkKind::Group { .. } => state.fail_group_round(&work.conversation_id, error)?,
                    WorkKind::Report { .. } => state.fail_bot_reports(&work.conversation_id, error, now)?,
                    WorkKind::User { text, attachments } => {
                        use grok_bot_model::{Message, Role, RunState, RunStatus, iso, new_id};
                        let timestamp = iso(now);
                        let error = state.redact(error);
                        let conversation = state.conversation_mut(&work.conversation_id)?;
                        conversation.messages.push(Message {
                            id: new_id(),
                            role: Role::User,
                            text: text.clone(),
                            created_at: work.created_at.clone(),
                            tools: Vec::new(),
                            bot_exchange_ids: None,
                            source_bot_id: None,
                            speaker_bot_id: None,
                            attachments: (!attachments.is_empty()).then(|| attachments.clone()),
                            report_delivery_ids: None,
                        });
                        conversation.run = RunState {
                            id: work.id.clone(),
                            bot_id: Some(work.bot_id.clone()),
                            status: RunStatus::Error,
                            error: Some(error),
                            finished_at: Some(timestamp.clone()),
                            ..RunState::idle()
                        };
                        conversation.updated_at = timestamp;
                    }
                }
                state.remove_queued_work(&work.id);
                Ok(())
            })
            .await
    }
}
