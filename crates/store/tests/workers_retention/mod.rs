//! Retention fixtures are valid durable states, installed in one commit rather than replaying
//! thousands of engine turns. Actual worker/report transitions are covered by the parent module.

use super::*;
use grok_bot_store::workers::{MAX_TERMINAL_WORKER_BATCHES, MAX_TERMINAL_WORKER_BYTES, MAX_WORKER_BATCHES};

fn history(f: &Fixture, count: usize, report: Option<ReportStatus>, answer_bytes: usize) -> Vec<WorkerBatch> {
    (0..count)
        .map(|index| {
            let now = iso(f.now() + chrono::Duration::seconds(index as i64));
            let source = message(Role::Assistant, &format!("Visible source {index}"), &now);
            WorkerBatch {
                id: new_id(),
                bot_id: f.bot.clone(),
                parent_conversation_id: new_id(),
                source_message_id: source.id,
                source_run_id: new_id(),
                authorized_workspace: Some(String::new()),
                created_at: now.clone(),
                released_at: Some(now.clone()),
                cancelled: report.is_none() || report == Some(ReportStatus::Skipped),
                workers: vec![WorkerTask {
                    id: new_id(),
                    conversation_id: new_id(),
                    task: format!("Read retained fact {index}"),
                    created_at: now.clone(),
                    status: DeliveryStatus::Completed,
                    execution_run_id: Some(new_id()),
                    started_at: Some(now.clone()),
                    finished_at: Some(now.clone()),
                    error: None,
                    answer: Some("a".repeat(answer_bytes.min(MAX_WORKER_ANSWER_BYTES))),
                }],
                report: report.map(|status| DeliveryReport {
                    status,
                    run_id: (status != ReportStatus::Skipped && status != ReportStatus::Queued).then(new_id),
                    finished_at: status.is_terminal().then(|| now.clone()),
                    error: None,
                }),
                report_input_id: None,
                report_message_id: None,
            }
        })
        .map(|mut batch| {
            // Build the linked conversations below in `install`; IDs for reports must be durable.
            if batch.report.as_ref().is_some_and(|report| report.run_id.is_some()) {
                batch.report_input_id = Some(new_id());
                batch.report_message_id = Some(new_id());
            }
            batch
        })
        .collect()
}

async fn install(f: &Fixture, batches: &[WorkerBatch], private_bytes: usize) {
    f.store
        .mutate(|state| {
            for batch in batches {
                let now = &batch.created_at;
                let worker = &batch.workers[0];
                let mut parent = Conversation {
                    id: batch.parent_conversation_id.clone(),
                    bot_id: f.bot.clone(),
                    title: "Retained parent".into(),
                    messages: vec![
                        message(Role::User, "Visible user request", now),
                        Message { id: batch.source_message_id.clone(), ..message(Role::Assistant, "Visible spawning answer", now) },
                    ],
                    session_id: Some(new_id()),
                    session_fingerprint: Some("parent-fingerprint".into()),
                    run: RunState {
                        id: batch.source_run_id.clone(),
                        status: RunStatus::Completed,
                        started_at: Some(now.clone()),
                        finished_at: Some(now.clone()),
                        ..RunState::idle()
                    },
                    updated_at: now.clone(),
                    group_id: None,
                    thread: None,
                    group_round: None,
                    member_sessions: None,
                    canvas: None,
                };
                if let Some(report) = &batch.report
                    && let Some(run_id) = &report.run_id
                {
                    parent.messages.extend([
                        Message { id: batch.report_input_id.clone().unwrap(), ..message(Role::User, "PRIVATE_RETENTION_INPUT_CANARY", now) },
                        Message { id: batch.report_message_id.clone().unwrap(), ..message(Role::Assistant, "Visible synthesized answer", now) },
                    ]);
                    parent.run.id = run_id.clone();
                    parent.run.status = match report.status {
                        ReportStatus::Running => RunStatus::Running,
                        ReportStatus::Completed => RunStatus::Completed,
                        ReportStatus::Cancelled => RunStatus::Cancelled,
                        ReportStatus::Error => RunStatus::Error,
                        ReportStatus::Interrupted => RunStatus::Interrupted,
                        _ => unreachable!(),
                    };
                    parent.run.finished_at = report.finished_at.clone();
                }
                let mut child = parent.clone();
                child.id = worker.conversation_id.clone();
                child.title = "Private worker".into();
                child.messages = vec![message(Role::User, &worker.task, now), message(Role::Assistant, &"a".repeat(private_bytes), now)];
                child.run = RunState {
                    id: worker.execution_run_id.clone().unwrap(),
                    status: RunStatus::Completed,
                    started_at: worker.started_at.clone(),
                    finished_at: worker.finished_at.clone(),
                    ..RunState::idle()
                };
                let disk = state.disk_mut();
                disk.conversations.extend([parent, child]);
                disk.worker_batches.push(batch.clone());
            }
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn pruning_is_oldest_terminal_first_atomic_and_preserves_visible_source_and_synthesis() {
    let f = Fixture::new().await;
    let batches = history(&f, MAX_TERMINAL_WORKER_BATCHES + 2, Some(ReportStatus::Completed), 0);
    install(&f, &batches, 0).await;
    let visible_before = f.store.snapshot().conversations;
    let mut events = f.store.subscribe();
    let removed = f.store.mutate(|state| state.prune_worker_history()).await.unwrap();
    assert_eq!(removed, batches[..2].iter().map(|batch| batch.workers[0].conversation_id.clone()).collect::<Vec<_>>());
    let public = events.recv().await.unwrap();
    assert_eq!(public.worker_batches.len(), MAX_TERMINAL_WORKER_BATCHES);
    assert_eq!(public.conversations, visible_before, "retention removes no visible parent message, text, or session");
    assert!(!serde_json::to_string(&*public).unwrap().contains("PRIVATE_RETENTION_INPUT_CANARY"));
    let disk = f.disk_json();
    for old in &batches[..2] {
        assert!(!disk["workerBatches"].as_array().unwrap().iter().any(|batch| batch["id"] == old.id));
        assert!(!disk["conversations"].as_array().unwrap().iter().any(|conversation| conversation["id"] == old.workers[0].conversation_id));
        let parent = disk["conversations"].as_array().unwrap().iter().find(|conversation| conversation["id"] == old.parent_conversation_id).unwrap();
        assert_eq!(parent["messages"].as_array().unwrap().len(), 3);
        assert!(parent["messages"].as_array().unwrap().iter().any(|message| message["id"] == old.report_message_id.as_deref().unwrap()));
    }
    let revision = public.revision;
    assert!(f.store.mutate(|state| state.prune_worker_history()).await.unwrap().is_empty());
    assert_eq!(f.store.snapshot().revision, revision, "no-op retention must not churn revisions");
    assert_eq!(f.reopen().await.unwrap().snapshot().conversations, visible_before);
}

#[tokio::test]
async fn every_terminal_report_outcome_and_suppressed_report_can_age_out() {
    for outcome in [
        None,
        Some(ReportStatus::Completed),
        Some(ReportStatus::Cancelled),
        Some(ReportStatus::Error),
        Some(ReportStatus::Interrupted),
        Some(ReportStatus::Skipped),
    ] {
        let f = Fixture::new().await;
        let mut batches = history(&f, MAX_TERMINAL_WORKER_BATCHES + 1, outcome, 0);
        // Failure before dispatch is terminal too: it has no report input, assistant, or run.
        if outcome == Some(ReportStatus::Error) {
            batches[0].report.as_mut().unwrap().run_id = None;
            batches[0].report_input_id = None;
            batches[0].report_message_id = None;
        }
        install(&f, &batches, 0).await;
        let removed = f.store.mutate(|state| state.prune_worker_history()).await.unwrap();
        assert_eq!(removed, vec![batches[0].workers[0].conversation_id.clone()], "outcome {outcome:?}");
        f.reopen().await.unwrap();
    }
}

#[tokio::test]
async fn byte_budget_includes_private_transcript_not_only_the_bounded_answer() {
    let f = Fixture::new().await;
    let private_bytes = 1024 * 1024;
    let batches = history(&f, 10, Some(ReportStatus::Completed), private_bytes);
    install(&f, &batches, private_bytes).await;
    let removed = f.store.mutate(|state| state.prune_worker_history()).await.unwrap();
    assert!(!removed.is_empty(), "byte retention applies below the count ceiling");
    let private_size = f.store.read(|state| {
        state
            .disk()
            .worker_batches
            .iter()
            .map(|batch| {
                serde_json::to_vec(batch).unwrap().len()
                    + serde_json::to_vec(state.conversation(&batch.workers[0].conversation_id).unwrap()).unwrap().len()
                    + serde_json::to_vec(
                        state
                            .conversation(&batch.parent_conversation_id)
                            .unwrap()
                            .messages
                            .iter()
                            .find(|message| Some(&message.id) == batch.report_input_id.as_ref())
                            .unwrap(),
                    )
                    .unwrap()
                    .len()
            })
            .sum::<usize>()
    });
    assert!(private_size <= MAX_TERMINAL_WORKER_BYTES);
    assert_eq!(f.store.snapshot().conversations.len(), batches.len(), "all visible parents survive");
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn pending_and_active_reports_are_never_evicted_but_an_unrelated_user_turn_is_not_a_pin() {
    let f = Fixture::new().await;
    let batches = history(&f, MAX_TERMINAL_WORKER_BATCHES + 1, Some(ReportStatus::Completed), 0);
    install(&f, &batches, 0).await;
    let queued = history(&f, 1, Some(ReportStatus::Queued), 0);
    let mut running = history(&f, 1, Some(ReportStatus::Running), 0);
    running[0].cancelled = true; // A Stop request does not settle an executing report by itself.
    install(&f, &queued, 0).await;
    f.store
        .mutate(|state| {
            let parent = state.conversation_mut(&batches[0].parent_conversation_id)?;
            parent
                .messages
                .extend([message(Role::User, "A later ordinary user turn", &f.iso_now()), message(Role::Assistant, "New active answer", &f.iso_now())]);
            parent.run = RunState { id: new_id(), status: RunStatus::Running, started_at: Some(f.iso_now()), ..RunState::idle() };
            Ok(())
        })
        .await
        .unwrap();
    let removed = f.store.mutate(|state| state.prune_worker_history()).await.unwrap();
    assert_eq!(removed, vec![batches[0].workers[0].conversation_id.clone()]);
    assert!(f.store.read(|state| state.conversation(&batches[0].parent_conversation_id).unwrap().run.status.is_active()));
    f.finish(&batches[0].parent_conversation_id).await;
    install(&f, &running, 0).await;
    assert!(f.store.mutate(|state| state.prune_worker_history()).await.unwrap().is_empty());
    f.store.read(|state| {
        assert_eq!(state.worker_batch(&queued[0].id).unwrap().report.as_ref().unwrap().status, ReportStatus::Queued);
        assert_eq!(state.worker_batch(&running[0].id).unwrap().report.as_ref().unwrap().status, ReportStatus::Running);
    });
}

#[tokio::test]
async fn legacy_thousand_batch_state_can_prune_before_next_admission_without_touching_pending_work() {
    let f = Fixture::new().await;
    let mut batches = history(&f, MAX_WORKER_BATCHES, None, 0);
    for batch in &mut batches {
        batch.authorized_workspace = None;
    }
    install(&f, &batches, 0).await;
    let f = Fixture { store: f.reopen().await.unwrap(), ..f };
    assert_eq!(f.store.snapshot().worker_batches.len(), MAX_WORKER_BATCHES);
    let p = parent(&f).await;
    let before = f.disk_text();
    assert_eq!(f.store.mutate(|state| state.spawn_worker(&p.conversation, &p.message, &p.run, "Before maintenance")).await.unwrap_err().status, 409);
    assert_eq!(f.disk_text(), before, "rejected admission has no cleanup side effects or rollback dependency");
    assert_eq!(f.store.mutate(|state| state.prune_worker_history()).await.unwrap().len(), MAX_WORKER_BATCHES - MAX_TERMINAL_WORKER_BATCHES);
    let (batch, worker) = spawn(&f, &p, "New approved work after maintenance").await;
    assert_eq!(batch.authorized_workspace, Some(String::new()));
    assert_eq!(worker.status, DeliveryStatus::Queued);
    let revision = f.store.snapshot().revision;
    assert!(f.store.mutate(|state| state.prune_worker_history()).await.unwrap().is_empty());
    assert_eq!(f.store.snapshot().revision, revision);
    assert_eq!(f.store.snapshot().worker_batches.len(), MAX_TERMINAL_WORKER_BATCHES + 1);
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn capacity_pressure_can_release_the_small_terminal_cache_in_a_separate_durable_mutation() {
    let f = Fixture::new().await;
    let terminal = history(&f, 3, Some(ReportStatus::Completed), 0);
    install(&f, &terminal, 0).await;
    let p = parent(&f).await;
    let (pending, child) = spawn(&f, &p, "A pending task must survive capacity cleanup").await;
    let visible = f.store.snapshot().conversations;
    let before = f.disk_text();
    assert!(f.store.mutate(|state| state.prune_worker_history()).await.unwrap().is_empty());
    assert_eq!(f.disk_text(), before, "the cache is below normal retention ceilings");
    assert_eq!(f.store.mutate(|state| state.spawn_worker(&p.conversation, &p.message, &p.run, "")).await.unwrap_err().status, 400);
    assert_eq!(f.disk_text(), before, "failed admission alone performs no cleanup");
    let removed = f.store.mutate(|state| state.prune_worker_history_for_capacity()).await.unwrap();
    assert_eq!(removed, terminal.iter().map(|batch| batch.workers[0].conversation_id.clone()).collect::<Vec<_>>());
    assert_eq!(f.store.snapshot().conversations, visible, "even pressure cleanup preserves all visible messages");
    assert_eq!(batch(&f, &pending.id).workers[0].id, child.id);
    assert_eq!(batch(&f, &pending.id).workers[0].status, DeliveryStatus::Queued);
    let revision = f.store.snapshot().revision;
    assert!(f.store.mutate(|state| state.prune_worker_history_for_capacity()).await.unwrap().is_empty());
    assert_eq!(f.store.snapshot().revision, revision);
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn workspace_authorization_is_pinned_optional_and_checked_lexically_without_requiring_live_directory() {
    let f = Fixture::new().await;
    f.store
        .mutate(|state| {
            state.update_settings(SettingsInput {
                base_url: "http://127.0.0.1:12345/v1".into(),
                model: "fixture".into(),
                workspace: f.workspace.to_string_lossy().into(),
                api_key: None,
                clear_api_key: None,
            })
        })
        .await
        .unwrap();
    let p = parent(&f).await;
    assert!(f.store.read(|state| state.worker_source_eligible(&p.conversation, &p.message, &p.run)));
    let (authorized, _) = spawn(&f, &p, "Read the authorized workspace").await;
    assert_eq!(authorized.authorized_workspace.as_deref(), Some(f.workspace.to_str().unwrap()));
    f.finish(&p.conversation).await;
    f.store
        .mutate(|state| {
            state.update_settings(SettingsInput {
                base_url: "http://127.0.0.1:12345/v1".into(),
                model: "fixture".into(),
                workspace: String::new(),
                api_key: None,
                clear_api_key: None,
            })
        })
        .await
        .unwrap();
    assert_eq!(batch(&f, &authorized.id).authorized_workspace.as_deref(), Some(f.workspace.to_str().unwrap()));
    let original = f.disk_json();
    for path in [
        "relative/path".to_owned(),
        "/".into(),
        "/safe/../other".into(),
        "/bad\0name".into(),
        f.data_dir.to_string_lossy().into(),
        f.data_dir.join("grok").to_string_lossy().into(),
        f.root.path().to_string_lossy().into(),
    ] {
        let mut forged = original.clone();
        forged["workerBatches"][0]["authorizedWorkspace"] = path.into();
        let text = write_json(&f.state_file(), &forged);
        assert_load_rejected(&f, &text).await;
    }
    let mut missing = original.clone();
    missing["workerBatches"][0]["authorizedWorkspace"] = f.root.path().join("no-longer-present").to_string_lossy().into_owned().into();
    write_json(&f.state_file(), &missing);
    f.reopen().await.unwrap();
    let mut legacy = original;
    legacy["workerBatches"][0].as_object_mut().unwrap().remove("authorizedWorkspace");
    write_json(&f.state_file(), &legacy);
    assert_eq!(f.reopen().await.unwrap().read(|state| state.worker_batch(&authorized.id).unwrap().authorized_workspace.clone()), None);
}

#[tokio::test]
async fn unreleased_work_and_cancelled_batches_keep_their_active_source_receipt() {
    let f = Fixture::new().await;
    let terminal = history(&f, MAX_TERMINAL_WORKER_BATCHES + 1, Some(ReportStatus::Completed), 0);
    install(&f, &terminal, 0).await;
    let p = parent(&f).await;
    let (pending, child) = spawn(&f, &p, "Still owned by the active source turn").await;
    assert_eq!(f.store.mutate(|state| state.prune_worker_history()).await.unwrap(), vec![terminal[0].workers[0].conversation_id.clone()]);
    assert_eq!(batch(&f, &pending.id).workers[0].status, DeliveryStatus::Queued);
    f.store.mutate(|state| state.cancel_worker_batches(&p.conversation)).await.unwrap();
    assert_eq!(batch(&f, &pending.id).workers[0].status, DeliveryStatus::Cancelled);
    let revision = f.store.snapshot().revision;
    assert!(f.store.mutate(|state| state.prune_worker_history()).await.unwrap().is_empty(), "a cancelled receipt still belongs to its active source run");
    assert_eq!(f.store.snapshot().revision, revision);
    assert_eq!(spawn(&f, &p, "Still owned by the active source turn").await.1.id, child.id, "the source's exact retry still sees its durable receipt");
    f.finish(&p.conversation).await;
    assert_eq!(f.store.mutate(|state| state.prune_worker_history()).await.unwrap(), vec![child.conversation_id]);
    assert!(f.store.read(|state| state.worker_batch(&pending.id).is_err()));
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn worker_commit_invariant_failures_preserve_last_good_disk_and_fail_stop_without_claiming_rollback() {
    let f = Fixture::new().await;
    let p = parent(&f).await;
    let (batch, _) = spawn(&f, &p, "The original task").await;
    let before = f.disk_text();
    let error = f
        .store
        .mutate(|state| {
            state.disk_mut().worker_batches[0].workers[0].conversation_id = p.conversation.clone();
            Ok(())
        })
        .await
        .unwrap_err();
    assert_eq!(error.status, 500);
    assert_eq!(f.disk_text(), before, "invalid worker state was never written");
    f.store.read(|state| {
        assert_eq!(state.assert_writable().unwrap_err().status, 503);
        assert!(!state.runtime().available);
        assert_eq!(state.worker_batch(&batch.id).unwrap().workers[0].conversation_id, p.conversation, "RAM is fail-stopped, not rolled back");
    });
    f.reopen().await.unwrap();
}
