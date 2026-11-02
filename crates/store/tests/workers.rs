//! Same-bot worker admission, lifecycle, public-state hiding, recovery, and persistence integrity.

mod common;
#[path = "workers_retention/mod.rs"]
mod retention;

use common::{Fixture, assert_load_rejected, write_json};
use grok_bot_model::*;
use grok_bot_store::PRE_WORKERS_BACKUP;
use grok_bot_store::workers::{MAX_WORKER_ANSWER_BYTES, MAX_WORKERS_PER_BATCH};
use serde_json::{Value, json};

struct Parent {
    conversation: String,
    message: String,
    run: String,
}

fn message(role: Role, text: &str, now: &str) -> Message {
    Message {
        id: new_id(),
        role,
        text: text.into(),
        created_at: now.into(),
        tools: Vec::new(),
        bot_exchange_ids: None,
        source_bot_id: None,
        speaker_bot_id: None,
        attachments: None,
        report_delivery_ids: None,
    }
}

async fn parent(f: &Fixture) -> Parent {
    let id = f.store.mutate(|state| state.add_conversation(&f.bot)).await.unwrap().id;
    let now = f.iso_now();
    let (run, assistant) = (new_id(), message(Role::Assistant, "I will delegate the independent checks.", &now));
    let message_id = assistant.id.clone();
    f.store
        .mutate(|state| {
            let conversation = state.conversation_mut(&id)?;
            conversation.messages = vec![message(Role::User, "Check two independent facts.", &now), assistant];
            conversation.run = RunState { id: run.clone(), status: RunStatus::Running, started_at: Some(now), ..RunState::idle() };
            Ok(())
        })
        .await
        .unwrap();
    Parent { conversation: id, message: message_id, run }
}

async fn spawn(f: &Fixture, parent: &Parent, task: &str) -> (WorkerBatch, WorkerTask) {
    f.store.mutate(|state| state.spawn_worker(&parent.conversation, &parent.message, &parent.run, task)).await.unwrap()
}

fn batch(f: &Fixture, id: &str) -> WorkerBatch {
    f.store.read(|state| state.worker_batch(id).unwrap().clone())
}

async fn begin_worker(f: &Fixture, worker: &WorkerTask) -> String {
    let run = new_id();
    let now = f.iso_now();
    f.store
        .mutate(|state| {
            let conversation = state.conversation_mut(&worker.conversation_id)?;
            conversation.messages = vec![message(Role::User, &worker.task, &now), message(Role::Assistant, "", &now)];
            conversation.run = RunState { id: run.clone(), status: RunStatus::Running, started_at: Some(now), ..RunState::idle() };
            state.bind_worker_execution(&worker.id, &run)
        })
        .await
        .unwrap();
    run
}

async fn finish_worker(f: &Fixture, worker: &WorkerTask, text: &str, status: RunStatus) {
    let now = f.iso_now();
    f.store
        .mutate(|state| {
            let conversation = state.conversation_mut(&worker.conversation_id)?;
            conversation.messages.last_mut().unwrap().text = text.into();
            conversation.run.status = status;
            conversation.run.finished_at = Some(now);
            state.mark_conversation_unread(&worker.conversation_id)
        })
        .await
        .unwrap();
}

async fn begin_report(f: &Fixture, id: &str) -> (String, String, String) {
    let parent = batch(f, id).parent_conversation_id;
    let now = f.iso_now();
    let run = new_id();
    let input = message(Role::User, "PRIVATE_GROUPED_REPORT_CANARY", &now);
    let assistant = message(Role::Assistant, "The combined result is ready.", &now);
    let ids = (run.clone(), input.id.clone(), assistant.id.clone());
    f.store
        .mutate(|state| {
            let conversation = state.conversation_mut(&parent)?;
            conversation.messages.extend([input, assistant]);
            conversation.run = RunState { id: run.clone(), status: RunStatus::Running, started_at: Some(now), ..RunState::idle() };
            state.bind_worker_report(id, &ids.0, &ids.1, &ids.2)
        })
        .await
        .unwrap();
    ids
}

#[tokio::test]
async fn old_state_stays_field_free_until_first_worker_and_backup_is_private_never_replaced() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new().await;
    assert!(f.disk_json().get("workerBatches").is_none());
    assert!(serde_json::to_value(f.store.snapshot()).unwrap().get("workerBatches").is_none());
    let p = parent(&f).await;
    let before = f.disk_text();
    let (b, _) = spawn(&f, &p, "Inspect a synthetic fact").await;
    let path = f.data_dir.join(PRE_WORKERS_BACKUP);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    spawn(&f, &p, "Inspect another synthetic fact").await;
    assert_eq!(std::fs::read_to_string(path).unwrap(), before);
    assert_eq!(b.workers.len(), 1);
    assert_eq!(f.disk_json()["workerBatches"][0]["workers"].as_array().unwrap().len(), 2);
    let reopened = f.reopen().await.unwrap();
    assert!(reopened.read(|state| state.worker_batch(&b.id).unwrap().cancelled), "an interrupted spawning parent cancels unreleased work");
}

#[tokio::test]
async fn admission_is_idempotent_bounded_and_rejects_nested_or_stale_sources_without_mutation() {
    let f = Fixture::new().await;
    let p = parent(&f).await;
    for task in ["", "  ", &"x".repeat(4001), &"😀".repeat(2001)] {
        let revision = f.store.snapshot().revision;
        assert_eq!(f.store.mutate(|state| state.spawn_worker(&p.conversation, &p.message, &p.run, task)).await.unwrap_err().status, 400);
        assert_eq!(f.store.snapshot().revision, revision);
    }
    let (b, first) = spawn(&f, &p, "  Check alpha  ").await;
    assert_eq!(first.task, "Check alpha");
    let revision = f.store.snapshot().revision;
    assert_eq!(spawn(&f, &p, "Check alpha").await.1.id, first.id);
    assert_eq!(f.store.snapshot().revision, revision);
    f.store.read(|state| {
        assert_eq!(state.find_existing_worker(&p.conversation, &p.message, &p.run, " Check alpha ").unwrap().1.id, first.id);
        assert!(state.is_reserved_conversation(&first.conversation_id));
        assert!(state.is_reserved_conversation(&p.conversation));
    });
    spawn(&f, &p, "Check beta").await;
    spawn(&f, &p, "Check gamma").await;
    assert_eq!(batch(&f, &b.id).workers.len(), MAX_WORKERS_PER_BATCH);
    assert_eq!(f.store.mutate(|state| state.spawn_worker(&p.conversation, &p.message, &p.run, "A fourth task")).await.unwrap_err().status, 409);
    assert_eq!(f.store.mutate(|state| state.spawn_worker(&p.conversation, &p.message, &new_id(), "Stale")).await.unwrap_err().status, 409);
    assert_eq!(f.store.mutate(|state| state.spawn_worker(&first.conversation_id, &p.message, &p.run, "Nested")).await.unwrap_err().status, 409);
    assert_eq!(f.store.read(|state| state.disk().conversations.len()), 4);
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.snapshot().worker_batches[0].workers.len(), 3);
}

#[tokio::test]
async fn siblings_release_after_success_report_once_as_a_group_and_hidden_inputs_never_reach_public_state() {
    let f = Fixture::new().await;
    let p = parent(&f).await;
    let (b, a) = spawn(&f, &p, "Check alpha").await;
    let (_, z) = spawn(&f, &p, "Check beta").await;
    assert!(batch(&f, &b.id).released_at.is_none());
    f.finish(&p.conversation).await;
    assert!(batch(&f, &b.id).released_at.is_some());
    let mut events = f.store.subscribe();
    begin_worker(&f, &a).await;
    let state = events.recv().await.unwrap();
    assert_eq!(state.conversations.len(), 1, "SSE uses the same hidden-conversation projection as GET");
    assert_eq!(state.worker_batches[0].workers[0].status, DeliveryStatus::Running);
    finish_worker(&f, &a, "Alpha verified.", RunStatus::Completed).await;
    assert!(batch(&f, &b.id).report.is_none(), "a finished sibling cannot trigger an early follow-up");
    assert_eq!(f.store.snapshot().bots[0].unread, None, "private completions are not user-visible new answers");
    begin_worker(&f, &z).await;
    finish_worker(&f, &z, "Beta verified.", RunStatus::Completed).await;
    assert_eq!(batch(&f, &b.id).report.unwrap().status, ReportStatus::Queued);
    let ids = begin_report(&f, &b.id).await;
    let public = f.store.snapshot();
    assert_eq!(public.conversations.len(), 1);
    assert_eq!(public.conversations[0].messages.len(), 3);
    assert!(!serde_json::to_string(&public).unwrap().contains("PRIVATE_GROUPED_REPORT_CANARY"));
    assert!(f.disk_text().contains("PRIVATE_GROUPED_REPORT_CANARY"));
    assert_eq!(public.worker_batches[0].report_input_id.as_deref(), Some(ids.1.as_str()));
    assert!(f.store.mutate(|state| state.create_thread(&p.conversation, &ids.1)).await.is_err());
    assert!(f.store.mutate(|state| state.spawn_worker(&p.conversation, &ids.2, &ids.0, "Report recursion")).await.is_err());
    f.finish(&p.conversation).await;
    assert_eq!(batch(&f, &b.id).report.unwrap().status, ReportStatus::Completed);
    assert!(!f.store.read(|state| state.is_reserved_conversation(&p.conversation)));
    let revision = f.store.snapshot().revision;
    assert!(f.store.mutate(|state| state.cancel_worker_batches(&p.conversation)).await.unwrap().is_empty());
    assert_eq!(f.store.snapshot().revision, revision);
    assert!(!batch(&f, &b.id).cancelled, "Stop in a later turn cannot cancel already completed history");
    let original_report = batch(&f, &b.id).report;
    f.start(&p.conversation).await;
    f.finish(&p.conversation).await;
    assert_eq!(batch(&f, &b.id).report, original_report, "a later parent run cannot overwrite the frozen report result");
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.snapshot().worker_batches[0].report.as_ref().unwrap().status, ReportStatus::Completed);
    assert_eq!(reopened.snapshot().conversations[0].messages.len(), 3);
}

#[tokio::test]
async fn parent_failure_and_stop_cancel_queued_siblings_suppress_reports_and_return_active_children() {
    for status in [RunStatus::Cancelled, RunStatus::Error, RunStatus::Interrupted] {
        let f = Fixture::new().await;
        let p = parent(&f).await;
        let (b, a) = spawn(&f, &p, "Never dispatch").await;
        let now = f.iso_now();
        f.store
            .mutate(|state| {
                let run = &mut state.conversation_mut(&p.conversation)?.run;
                run.status = status;
                run.finished_at = Some(now);
                Ok(())
            })
            .await
            .unwrap();
        let b = batch(&f, &b.id);
        assert!(b.cancelled);
        assert_eq!(b.workers[0].status, DeliveryStatus::Cancelled);
        assert!(b.report.is_none());
        assert_eq!(f.conversation(&a.conversation_id).run.status, RunStatus::Cancelled);
        f.reopen().await.unwrap();
    }
    let f = Fixture::new().await;
    let p = parent(&f).await;
    let (b, a) = spawn(&f, &p, "Active task").await;
    let (_, z) = spawn(&f, &p, "Queued sibling").await;
    f.finish(&p.conversation).await;
    begin_worker(&f, &a).await;
    let active = f.store.mutate(|state| state.cancel_worker_batches(&p.conversation)).await.unwrap();
    assert_eq!(active.as_slice(), std::slice::from_ref(&a.conversation_id));
    assert_eq!(f.conversation(&z.conversation_id).run.status, RunStatus::Cancelled);
    finish_worker(&f, &a, "partial output", RunStatus::Cancelled).await;
    let b = batch(&f, &b.id);
    assert!(b.cancelled && b.report.is_none());
    assert!(b.workers.iter().all(|worker| worker.answer.is_none()));
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn restart_interrupts_started_worker_and_started_report_without_replaying_them() {
    let f = Fixture::new().await;
    let p = parent(&f).await;
    let (b, a) = spawn(&f, &p, "Started once").await;
    f.finish(&p.conversation).await;
    begin_worker(&f, &a).await;
    let reopened = f.reopen().await.unwrap();
    let recovered = reopened.read(|state| state.worker_batch(&b.id).unwrap().clone());
    assert_eq!(recovered.workers[0].status, DeliveryStatus::Interrupted);
    assert_eq!(recovered.report.unwrap().status, ReportStatus::Queued);
    drop(reopened);
    let f = Fixture { store: f.reopen().await.unwrap(), ..f };
    begin_report(&f, &b.id).await;
    let reopened = f.reopen().await.unwrap();
    let recovered = &reopened.snapshot().worker_batches[0];
    assert_eq!(recovered.report.as_ref().unwrap().status, ReportStatus::Interrupted);
    assert_eq!(reopened.snapshot().conversations[0].messages.len(), 3);
    drop(reopened);
    assert_eq!(f.reopen().await.unwrap().snapshot().worker_batches[0].report.as_ref().unwrap().status, ReportStatus::Interrupted);
}

#[tokio::test]
async fn failure_paths_are_terminal_and_final_answers_are_bounded_and_credentials_redacted() {
    let f = Fixture::new().await;
    let key = "synthetic-provider-key-123456";
    f.store
        .mutate(|state| {
            state.update_settings(SettingsInput {
                base_url: "http://127.0.0.1:12345/v1".into(),
                model: "fixture".into(),
                workspace: String::new(),
                api_key: Some(key.into()),
                clear_api_key: None,
            })
        })
        .await
        .unwrap();
    let p = parent(&f).await;
    let (b, a) = spawn(&f, &p, &format!("Check {key}")).await;
    let (_, z) = spawn(&f, &p, "Startup fails").await;
    f.finish(&p.conversation).await;
    begin_worker(&f, &a).await;
    let answer = format!("{key}{}", "😀".repeat(MAX_WORKER_ANSWER_BYTES));
    finish_worker(&f, &a, &answer, RunStatus::Completed).await;
    let now = f.iso_now();
    f.store.mutate(|state| state.fail_worker(&z.id, &format!("Failed {key}{}", "x".repeat(2000)))).await.unwrap();
    let b = batch(&f, &b.id);
    assert_eq!(b.workers[1].status, DeliveryStatus::Error);
    assert_eq!(b.workers[1].finished_at.as_deref(), Some(now.as_str()));
    assert!(b.workers[1].error.as_ref().unwrap().len() <= 1000);
    let saved = b.workers[0].answer.as_ref().unwrap();
    assert!(saved.len() <= MAX_WORKER_ANSWER_BYTES && answer.starts_with(saved));
    let public = serde_json::to_string(&f.store.snapshot()).unwrap();
    assert!(!public.contains(key));
    assert!(public.contains("[redacted]"));
    f.store.mutate(|state| state.fail_worker_report(&b.id, &format!("Report failed {key}"))).await.unwrap();
    assert_eq!(batch(&f, &b.id).report.unwrap().status, ReportStatus::Error);
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn invalid_worker_links_statuses_and_hidden_message_forgery_are_rejected_without_replacing_state() {
    let f = Fixture::new().await;
    let p = parent(&f).await;
    let (b, a) = spawn(&f, &p, "Valid task").await;
    f.finish(&p.conversation).await;
    begin_worker(&f, &a).await;
    finish_worker(&f, &a, "A final answer.", RunStatus::Completed).await;
    begin_report(&f, &b.id).await;
    f.finish(&p.conversation).await;
    let original = f.disk_json();
    let forgeries: Vec<fn(&mut Value)> = vec![
        |s| s["workerBatches"][0]["extra"] = true.into(),
        |s| s["workerBatches"][0]["botId"] = new_id().into(),
        |s| s["workerBatches"][0]["parentConversationId"] = s["workerBatches"][0]["workers"][0]["conversationId"].clone(),
        |s| s["workerBatches"][0]["sourceMessageId"] = s["workerBatches"][0]["reportMessageId"].clone(),
        |s| s["workerBatches"][0]["workers"][0]["answer"] = "not the actual answer".into(),
        |s| s["workerBatches"][0]["workers"][0]["status"] = "queued".into(),
        |s| s["workerBatches"][0]["workers"][0]["executionRunId"] = new_id().into(),
        |s| s["workerBatches"][0]["workers"][0]["error"] = "x".repeat(1001).into(),
        |s| s["workerBatches"][0]["report"]["error"] = "x".repeat(1001).into(),
        |s| s["workerBatches"][0]["reportInputId"] = s["workerBatches"][0]["sourceMessageId"].clone(),
        |s| s["workerBatches"][0]["report"]["status"] = "queued".into(),
        |s| s["workerBatches"][0]["reportMessageId"] = new_id().into(),
        |s| s["workerBatches"][0]["workers"][0]["task"] = "tampered task".into(),
        |s| {
            let w = s["workerBatches"][0]["workers"][0].clone();
            s["workerBatches"][0]["workers"].as_array_mut().unwrap().push(w);
        },
    ];
    for forge in forgeries {
        let mut state = original.clone();
        forge(&mut state);
        let bytes = write_json(&f.state_file(), &state);
        assert_load_rejected(&f, &bytes).await;
    }
    write_json(&f.state_file(), &original);
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn pending_batches_guard_bot_deletion_then_terminal_history_is_removed_with_hidden_conversations() {
    let f = Fixture::new().await;
    f.store.mutate(|state| state.add_bot(BotInput { name: "Keep".into(), color: None, instructions: None, shape: None, pinned: None })).await.unwrap();
    let p = parent(&f).await;
    let (b, a) = spawn(&f, &p, "A pending task").await;
    f.finish(&p.conversation).await;
    assert_eq!(f.store.mutate(|state| state.delete_bot(&f.bot)).await.unwrap_err().status, 409);
    assert!(f.store.mutate(|state| state.create_thread(&a.conversation_id, &new_id())).await.is_err());
    f.store.mutate(|state| state.cancel_worker_batches(&p.conversation)).await.unwrap();
    assert!(batch(&f, &b.id).cancelled);
    let removed = f.store.mutate(|state| state.delete_bot(&f.bot)).await.unwrap();
    assert!(removed.contains(&p.conversation) && removed.contains(&a.conversation_id));
    assert!(f.store.snapshot().worker_batches.is_empty());
    assert_eq!(f.disk_json()["conversations"], json!([]));
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn sealed_queued_workers_survive_restart_and_cancelling_the_queued_report_is_final() {
    let f = Fixture::new().await;
    let p = parent(&f).await;
    let (b, a) = spawn(&f, &p, "Queued before restart").await;
    f.finish(&p.conversation).await;
    let reopened = f.reopen().await.unwrap();
    let resumed = reopened.read(|state| state.worker_batch(&b.id).unwrap().clone());
    assert!(!resumed.cancelled && resumed.released_at.is_some());
    assert_eq!(resumed.workers[0].status, DeliveryStatus::Queued);
    assert!(resumed.report.is_none());
    drop(reopened);
    let f = Fixture { store: f.reopen().await.unwrap(), ..f };
    begin_worker(&f, &a).await;
    finish_worker(&f, &a, "A finished fact.", RunStatus::Completed).await;
    assert_eq!(batch(&f, &b.id).report.unwrap().status, ReportStatus::Queued);
    f.store.mutate(|state| state.cancel_worker_batches(&p.conversation)).await.unwrap();
    let stopped = batch(&f, &b.id);
    assert!(stopped.cancelled);
    let report = stopped.report.unwrap();
    assert_eq!(report.status, ReportStatus::Skipped);
    assert!(report.run_id.is_none() && report.finished_at.is_some());
    assert_eq!(f.reopen().await.unwrap().snapshot().worker_batches[0].report.as_ref().unwrap().status, ReportStatus::Skipped);
}

#[tokio::test]
async fn completion_winning_a_running_report_stop_race_remains_valid_and_is_never_retried() {
    let f = Fixture::new().await;
    let p = parent(&f).await;
    let (b, a) = spawn(&f, &p, "A result to report").await;
    f.finish(&p.conversation).await;
    begin_worker(&f, &a).await;
    finish_worker(&f, &a, "The result.", RunStatus::Completed).await;
    begin_report(&f, &b.id).await;
    f.store.mutate(|state| state.cancel_worker_batches(&p.conversation)).await.unwrap();
    // The engine can settle between the store's batch cancellation and Runtime::request_stop.
    f.finish(&p.conversation).await;
    let completed = batch(&f, &b.id);
    assert!(completed.cancelled);
    assert_eq!(completed.report.unwrap().status, ReportStatus::Completed);
    assert_eq!(f.reopen().await.unwrap().snapshot().worker_batches[0].report.as_ref().unwrap().status, ReportStatus::Completed);
}

#[tokio::test]
async fn capacity_refusal_reserves_full_private_transcripts_and_grouped_results_without_partial_mutation() {
    let f = Fixture::new().await;
    let p = parent(&f).await;
    // Leave less than the conservative worker/reply reservation in the 64 MiB state budget.
    let current_bytes = f.disk_text().len();
    f.store
        .mutate(|state| {
            state.conversation_mut(&p.conversation)?.messages[0].text = "x".repeat(grok_bot_store::MAX_STATE_BYTES - current_bytes - 2_000_000);
            Ok(())
        })
        .await
        .unwrap();
    let revision = f.store.snapshot().revision;
    let bytes = f.disk_text().len();
    let error = f.store.mutate(|state| state.spawn_worker(&p.conversation, &p.message, &p.run, "Do not overfill state")).await.unwrap_err();
    assert_eq!(error.status, 413);
    assert_eq!(f.store.snapshot().revision, revision);
    assert_eq!(f.disk_text().len(), bytes);
    f.store.read(|state| {
        assert!(state.disk().worker_batches.is_empty());
        assert_eq!(state.disk().conversations.len(), 1);
        state.assert_writable().unwrap();
    });
    assert!(!f.data_dir.join(PRE_WORKERS_BACKUP).exists());
}

#[tokio::test]
async fn routine_group_thread_and_other_bot_report_sources_cannot_spawn_workers() {
    let f = Fixture::new().await;
    let p = parent(&f).await;
    let other_bot = new_id();
    for kind in ["group", "thread", "routine", "delivery", "report"] {
        // These variants deliberately alter only in-memory admission inputs. No forged state is
        // committed: callers must validate before creating an execution receipt.
        let error = f
            .store
            .mutate(|state| {
                let before = state.disk().clone();
                match kind {
                    "group" => state.disk_mut().conversations[0].group_id = Some(other_bot.clone()),
                    "thread" => state.disk_mut().conversations[0].thread = Some(ThreadLink { parent_conversation_id: new_id(), parent_message_id: new_id() }),
                    "routine" => state.disk_mut().routine_runs.push(RoutineRun {
                        id: new_id(),
                        routine_id: new_id(),
                        routine_name: "Synthetic".into(),
                        bot_id: f.bot.clone(),
                        conversation_id: p.conversation.clone(),
                        prompt: "Synthetic".into(),
                        trigger: RoutineTrigger::Manual,
                        scheduled_for: f.iso_now(),
                        created_at: f.iso_now(),
                        execution_run_id: Some(p.run.clone()),
                        result: None,
                    }),
                    "delivery" => state.disk_mut().bot_deliveries.push(BotDelivery {
                        id: new_id(),
                        exchange_id: new_id(),
                        source_conversation_id: new_id(),
                        source_message_id: new_id(),
                        recipient_conversation_id: p.conversation.clone(),
                        from_bot_id: other_bot.clone(),
                        to_bot_id: f.bot.clone(),
                        text: "Synthetic".into(),
                        chain_id: new_id(),
                        hop: 1,
                        created_at: f.iso_now(),
                        status: DeliveryStatus::Running,
                        request_id: None,
                        execution_run_id: Some(p.run.clone()),
                        started_at: Some(f.iso_now()),
                        finished_at: None,
                        error: None,
                        report: None,
                        chain_termination: None,
                    }),
                    "report" => state.disk_mut().conversations[0].messages[0].report_delivery_ids = Some(Vec::new()),
                    _ => unreachable!(),
                }
                let result = state.validate_worker_source(&p.conversation, &p.message, &p.run, "Disallowed source");
                *state.disk_mut() = before;
                Ok(result.unwrap_err())
            })
            .await
            .unwrap();
        assert_eq!(error.status, 409, "{kind}");
    }
}
