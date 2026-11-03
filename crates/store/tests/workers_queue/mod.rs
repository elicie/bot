//! Workers share main's accepted FIFO and actual-Bot slots; receipts stay authoritative.

use super::*;
use grok_bot_store::PRE_WORK_QUEUE_BACKUP;

fn bot_input(name: &str) -> BotInput {
    BotInput { name: name.into(), color: None, instructions: None, shape: None, pinned: None }
}

async fn configured(cap: u32) -> Fixture {
    let f = Fixture::new().await;
    f.store
        .mutate(|state| {
            state.update_settings(SettingsInput {
                base_url: "http://localhost:8000/v1".into(),
                model: "fixture".into(),
                workspace: String::new(),
                api_key: None,
                clear_api_key: None,
                max_concurrent_runs: Some(cap),
            })
        })
        .await
        .unwrap();
    f
}

fn queue(f: &Fixture) -> Vec<QueuedWork> {
    f.store.read(|state| state.disk().work_queue.clone())
}

fn worker_pointer(f: &Fixture, id: &str) -> QueuedWork {
    queue(f).into_iter().find(|work| matches!(&work.kind, WorkKind::Worker { worker_id } if worker_id == id)).unwrap()
}

fn report_pointer(f: &Fixture, id: &str) -> QueuedWork {
    queue(f).into_iter().find(|work| matches!(&work.kind, WorkKind::WorkerReport { batch_id } if batch_id == id)).unwrap()
}

#[tokio::test]
async fn different_bots_workers_overlap_with_two_slots_while_same_bot_siblings_serialize() {
    let f = configured(2).await;
    let beta = f.store.mutate(|state| state.add_bot(bot_input("Beta"))).await.unwrap();
    let gamma = f.store.mutate(|state| state.add_bot(bot_input("Gamma"))).await.unwrap();
    let p = parent(&f).await;
    let (a_batch, a) = spawn(&f, &p, "First Alpha task").await;
    let (_, sibling) = spawn(&f, &p, "Second Alpha task").await;
    assert!(queue(&f).is_empty(), "unreleased receipts do not reserve queue positions");
    assert!(f.store.snapshot().conversations.iter().all(|conversation| conversation.id != a.conversation_id && conversation.id != sibling.conversation_id));
    f.finish(&p.conversation).await;
    let q = parent_for_bot(&f, &beta.id).await;
    let (b_batch, b) = spawn(&f, &q, "Beta task").await;
    f.finish(&q.conversation).await;
    let third = f.store.mutate(|state| state.add_conversation(&gamma.id)).await.unwrap();
    let sibling_pointer = worker_pointer(&f, &sibling.id);
    let mut events = f.store.subscribe();
    let a_run = begin_worker(&f, &a).await;
    let public = events.recv().await.unwrap();
    assert!(public.conversations.iter().all(|conversation| conversation.id != a.conversation_id && conversation.id != sibling.conversation_id));
    assert_eq!(public.work_queue.iter().find(|work| work.id == sibling_pointer.id), Some(&sibling_pointer));
    let projection = serde_json::to_value(&sibling_pointer).unwrap();
    assert_eq!(projection.as_object().unwrap().len(), 6);
    assert!(projection.get("task").is_none() && projection.get("text").is_none() && projection.get("sessionId").is_none());
    f.store.read(|state| {
        assert_eq!(state.active_count(), 1);
        assert!(!state.can_start(&f.bot, &sibling.conversation_id));
        assert!(state.can_start(&beta.id, &b.conversation_id));
        assert_eq!(state.active_for_bot(&f.bot).unwrap().run.id, a_run);
        assert_eq!(state.assert_bot_idle(&f.bot).unwrap_err().status, 409);
        state.assert_bot_idle(&beta.id).unwrap();
    });
    let b_run = begin_worker(&f, &b).await;
    f.store.read(|state| {
        assert_eq!(state.active_count(), 2);
        assert_eq!(state.active_for_bot(&beta.id).unwrap().run.id, b_run);
        assert_eq!(state.conversation(&a.conversation_id).unwrap().run.bot_id.as_deref(), Some(f.bot.as_str()));
        assert_eq!(state.conversation(&b.conversation_id).unwrap().run.bot_id.as_deref(), Some(beta.id.as_str()));
        assert!(!state.can_start(&gamma.id, &third.id), "ordinary turns use the same global cap as hidden workers");
        assert_eq!(state.assert_idle().unwrap_err().status, 409);
        state.assert_bot_idle(&gamma.id).unwrap();
    });
    assert_eq!(worker_pointer(&f, &sibling.id), sibling_pointer, "another Bot's claim cannot reorder or regenerate accepted work");
    finish_worker(&f, &a, "Alpha result", RunStatus::Completed).await;
    assert!(f.store.read(|state| state.can_start(&f.bot, &sibling.conversation_id)));
    begin_worker(&f, &sibling).await;
    assert_eq!(f.store.read(|state| state.active_count()), 2);
    finish_worker(&f, &sibling, "Second Alpha result", RunStatus::Completed).await;
    finish_worker(&f, &b, "Beta result", RunStatus::Completed).await;
    assert_eq!(batch(&f, &a_batch.id).report.unwrap().status, ReportStatus::Queued);
    assert_eq!(batch(&f, &b_batch.id).report.unwrap().status, ReportStatus::Queued);
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn ordinary_parent_input_accepted_before_release_survives_worker_projection_and_restart() {
    let f = configured(2).await;
    let p = parent(&f).await;
    let (_, worker) = spawn(&f, &p, "Unreleased worker receipt").await;
    let user = f.store.mutate(|state| state.enqueue_user_turn(&p.conversation, "Accepted during the source turn", Vec::new())).await.unwrap();
    assert_eq!(queue(&f), vec![user.clone()]);
    f.finish(&p.conversation).await;
    let worker = worker_pointer(&f, &worker.id);
    assert_eq!(queue(&f), vec![user.clone(), worker.clone()]);
    assert_eq!(f.reopen().await.unwrap().read(|state| state.disk().work_queue.clone()), vec![user, worker]);
}

#[tokio::test]
async fn ordinary_parent_input_is_accepted_while_workers_pending_and_keeps_fifo_ahead_of_report() {
    let f = configured(2).await;
    let p = parent(&f).await;
    let (b, worker) = spawn(&f, &p, "Private worker task").await;
    let before = f.disk_text();
    assert_eq!(f.store.mutate(|state| state.enqueue_user_turn(&worker.conversation_id, "Manual hidden input", Vec::new())).await.unwrap_err().status, 409);
    assert_eq!(f.disk_text(), before, "manual hidden-child input is rejected without mutation");
    f.finish(&p.conversation).await;
    let pointer = worker_pointer(&f, &worker.id);
    assert_eq!(pointer.bot_id, f.bot);
    assert_eq!(pointer.conversation_id, worker.conversation_id);
    assert_eq!(pointer.created_at, worker.created_at);
    begin_worker(&f, &worker).await;
    f.set_time("2026-01-01T03:00:00.000Z");
    let user = f.store.mutate(|state| state.enqueue_user_turn(&p.conversation, "Another normal request", Vec::new())).await.unwrap();
    assert!(!f.store.read(|state| state.is_reserved_conversation(&p.conversation)));
    f.set_time("2026-01-01T01:00:00.000Z");
    finish_worker(&f, &worker, "A bounded final answer", RunStatus::Completed).await;
    let report = report_pointer(&f, &b.id);
    assert_eq!(report.bot_id, f.bot);
    assert_eq!(report.conversation_id, p.conversation);
    assert_eq!(queue(&f), vec![user.clone(), report.clone()]);
    assert!(report.created_at < user.created_at, "wall-clock order must not reorder accepted claims");
    assert!(!f.store.mutate(|state| Ok(state.sync_work_queue())).await.unwrap());
    let now = f.iso_now();
    f.store
        .mutate(|state| {
            assert!(state.can_start(&f.bot, &p.conversation));
            assert_eq!(state.disk().work_queue.first(), Some(&user));
            state.remove_queued_work(&user.id).unwrap();
            let conversation = state.conversation_mut(&p.conversation)?;
            conversation.messages.extend([message(Role::User, "Another normal request", &now), message(Role::Assistant, "Normal answer", &now)]);
            conversation.run = RunState { id: new_id(), status: RunStatus::Running, bot_id: Some(f.bot.clone()), started_at: Some(now), ..RunState::idle() };
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(report_pointer(&f, &b.id), report, "an executing ordinary parent turn does not drop or retarget the grouped report");
    f.finish(&p.conversation).await;
    begin_report(&f, &b.id).await;
    assert!(queue(&f).is_empty());
    f.finish(&p.conversation).await;
    assert_eq!(batch(&f, &b.id).report.unwrap().status, ReportStatus::Completed);
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn one_global_slot_releases_parent_then_workers_and_grouped_reports_without_deadlock() {
    let f = configured(1).await;
    let beta = f.store.mutate(|state| state.add_bot(bot_input("Beta"))).await.unwrap();
    let p = parent(&f).await;
    let (a_batch, a) = spawn(&f, &p, "Alpha task").await;
    assert!(!f.store.read(|state| state.can_start(&f.bot, &a.conversation_id)));
    f.finish(&p.conversation).await;
    let q = parent_for_bot(&f, &beta.id).await;
    let (b_batch, b) = spawn(&f, &q, "Beta task").await;
    f.finish(&q.conversation).await;
    assert_eq!(f.store.read(|state| state.active_count()), 0, "pending batches do not own slots");
    assert_eq!(
        queue(&f).iter().map(|work| &work.kind).collect::<Vec<_>>(),
        vec![&WorkKind::Worker { worker_id: a.id.clone() }, &WorkKind::Worker { worker_id: b.id.clone() }]
    );
    begin_worker(&f, &a).await;
    assert!(!f.store.read(|state| state.can_start(&beta.id, &b.conversation_id)));
    finish_worker(&f, &a, "Alpha result", RunStatus::Completed).await;
    assert_eq!(queue(&f).first().unwrap().kind, WorkKind::Worker { worker_id: b.id.clone() });
    begin_worker(&f, &b).await;
    assert!(!f.store.read(|state| state.can_start(&f.bot, &p.conversation)));
    finish_worker(&f, &b, "Beta result", RunStatus::Completed).await;
    assert_eq!(queue(&f).first().unwrap().kind, WorkKind::WorkerReport { batch_id: a_batch.id.clone() });
    begin_report(&f, &a_batch.id).await;
    assert_eq!(f.store.read(|state| state.active_count()), 1);
    assert!(!f.store.read(|state| state.can_start(&beta.id, &q.conversation)));
    f.finish(&p.conversation).await;
    begin_report(&f, &b_batch.id).await;
    assert_eq!(f.store.read(|state| state.active_count()), 1);
    f.finish(&q.conversation).await;
    assert!(queue(&f).is_empty());
    assert_eq!(f.store.read(|state| state.active_count()), 0);
    assert_eq!(batch(&f, &a_batch.id).report.unwrap().status, ReportStatus::Completed);
    assert_eq!(batch(&f, &b_batch.id).report.unwrap().status, ReportStatus::Completed);
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn parent_stop_cancels_only_its_worker_children_not_other_bots_ordinary_queues_or_chains() {
    let f = configured(3).await;
    let beta = f.store.mutate(|state| state.add_bot(bot_input("Beta"))).await.unwrap();
    let gamma = f.store.mutate(|state| state.add_bot(bot_input("Gamma"))).await.unwrap();
    let p = parent(&f).await;
    let (a_batch, a) = spawn(&f, &p, "Running Alpha task").await;
    let (_, a_queued) = spawn(&f, &p, "Queued Alpha sibling").await;
    f.finish(&p.conversation).await;
    let q = parent_for_bot(&f, &beta.id).await;
    let (b_batch, b) = spawn(&f, &q, "Running Beta task").await;
    let (_, b_queued) = spawn(&f, &q, "Queued Beta sibling").await;
    f.finish(&q.conversation).await;
    begin_worker(&f, &a).await;
    begin_worker(&f, &b).await;
    let ordinary_a = f.store.mutate(|state| state.add_conversation(&f.bot)).await.unwrap();
    let ordinary_b = f.store.mutate(|state| state.add_conversation(&beta.id)).await.unwrap();
    let a_user = f.store.mutate(|state| state.enqueue_user_turn(&ordinary_a.id, "Unrelated Alpha request", Vec::new())).await.unwrap();
    let b_user = f.store.mutate(|state| state.enqueue_user_turn(&ordinary_b.id, "Unrelated Beta request", Vec::new())).await.unwrap();
    let source = parent_for_bot(&f, &gamma.id).await;
    let incoming = f
        .store
        .mutate(|state| {
            state.queue_bot_message(
                &source.conversation,
                &source.run,
                &source.message,
                BotMessageInput { to_bot_id: f.bot.clone(), text: "Unrelated delivery chain".into(), request_id: None },
            )
        })
        .await
        .unwrap();
    let keep = queue(&f).into_iter().filter(|work| work.conversation_id != a_queued.conversation_id).collect::<Vec<_>>();
    let active = f
        .store
        .mutate(|state| {
            let active = state.cancel_worker_batches(&p.conversation)?;
            state.cancel_conversation_work(&p.conversation)?;
            Ok(active)
        })
        .await
        .unwrap();
    assert_eq!(active, vec![a.conversation_id.clone()]);
    assert_eq!(queue(&f), keep);
    assert!(queue(&f).contains(&a_user) && queue(&f).contains(&b_user));
    assert_eq!(worker_pointer(&f, &b_queued.id).conversation_id, b_queued.conversation_id);
    assert_eq!(batch(&f, &a_batch.id).workers[1].status, DeliveryStatus::Cancelled);
    assert!(batch(&f, &a_batch.id).cancelled);
    assert!(!batch(&f, &b_batch.id).cancelled);
    f.store.read(|state| {
        assert_eq!(state.conversation(&a.conversation_id).unwrap().run.status, RunStatus::Running, "the runtime still owns engine shutdown");
        assert_eq!(state.conversation(&b.conversation_id).unwrap().run.status, RunStatus::Running);
        assert_eq!(state.conversation(&source.conversation).unwrap().run.id, source.run);
        assert!(state.conversation(&source.conversation).unwrap().run.status.is_active());
        let delivery = state.disk().bot_deliveries.iter().find(|delivery| delivery.id == incoming.id).unwrap();
        assert_eq!(delivery.status, DeliveryStatus::Queued);
        assert!(delivery.chain_termination.is_none());
    });
    finish_worker(&f, &a, "Cancelled private result", RunStatus::Cancelled).await;
    assert!(batch(&f, &a_batch.id).report.is_none());
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn restart_keeps_queued_worker_ids_and_order_without_replaying_claimed_workers_or_reports() {
    let f = configured(2).await;
    let beta = f.store.mutate(|state| state.add_bot(bot_input("Beta"))).await.unwrap();
    let p = parent(&f).await;
    let (a_batch, a) = spawn(&f, &p, "Started Alpha task").await;
    let (_, waiting) = spawn(&f, &p, "Unstarted Alpha task").await;
    f.finish(&p.conversation).await;
    let q = parent_for_bot(&f, &beta.id).await;
    let (b_batch, b) = spawn(&f, &q, "Started Beta task").await;
    f.finish(&q.conversation).await;
    let queued = worker_pointer(&f, &waiting.id);
    begin_worker(&f, &a).await;
    begin_worker(&f, &b).await;
    assert_eq!(queue(&f), vec![queued.clone()]);
    let restarted = f.reopen().await.unwrap();
    assert_eq!(restarted.read(|state| state.active_count()), 0);
    assert_eq!(restarted.read(|state| state.worker_batch(&a_batch.id).unwrap().workers[0].status), DeliveryStatus::Interrupted);
    assert_eq!(restarted.read(|state| state.worker_batch(&b_batch.id).unwrap().workers[0].status), DeliveryStatus::Interrupted);
    let f = Fixture { store: restarted, ..f };
    assert_eq!(queue(&f)[0], queued);
    let report = report_pointer(&f, &b_batch.id);
    assert_eq!(queue(&f), vec![queued.clone(), report]);
    begin_report(&f, &b_batch.id).await;
    assert_eq!(queue(&f), vec![queued.clone()]);
    let restarted = f.reopen().await.unwrap();
    assert_eq!(restarted.read(|state| state.disk().work_queue.clone()), vec![queued]);
    assert_eq!(restarted.read(|state| state.worker_batch(&b_batch.id).unwrap().report.as_ref().unwrap().status), ReportStatus::Interrupted);
    assert_eq!(restarted.read(|state| state.worker_batch(&a_batch.id).unwrap().workers[1].status), DeliveryStatus::Queued);
}

#[tokio::test]
async fn unclaimed_start_failures_consume_pointers_without_starting_parent_or_replaying_on_restart() {
    let f = configured(2).await;
    let p = parent(&f).await;
    let (b, worker) = spawn(&f, &p, "Could not start").await;
    f.finish(&p.conversation).await;
    let work = worker_pointer(&f, &worker.id);
    let parent_before = f.conversation(&p.conversation);
    f.store
        .mutate(|state| {
            state.fail_worker(&worker.id, "Private worker preflight failed")?;
            state.remove_queued_work(&work.id);
            Ok(())
        })
        .await
        .unwrap();
    let failed = batch(&f, &b.id).workers[0].clone();
    assert_eq!(failed.status, DeliveryStatus::Error);
    assert!(failed.execution_run_id.is_none() && failed.started_at.is_none());
    assert!(f.conversation(&worker.conversation_id).run.id.is_empty());
    assert!(f.conversation(&worker.conversation_id).messages.is_empty());
    let report = report_pointer(&f, &b.id);
    f.store
        .mutate(|state| {
            state.fail_worker_report(&b.id, "Grouped report preflight failed")?;
            state.remove_queued_work(&report.id);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(f.conversation(&p.conversation), parent_before);
    let failed_report = batch(&f, &b.id).report.unwrap();
    assert_eq!(failed_report.status, ReportStatus::Error);
    assert!(failed_report.run_id.is_none());
    assert!(queue(&f).is_empty());
    assert!(f.reopen().await.unwrap().read(|state| state.disk().work_queue.is_empty()));
}

#[tokio::test]
async fn worker_pointer_reconciliation_repairs_stale_projections_without_moving_existing_claims() {
    let f = configured(2).await;
    let p = parent(&f).await;
    let (_, a) = spawn(&f, &p, "First ready receipt").await;
    let (_, b) = spawn(&f, &p, "Second ready receipt").await;
    f.finish(&p.conversation).await;
    let first = worker_pointer(&f, &a.id);
    let second = worker_pointer(&f, &b.id);
    let user = f.store.mutate(|state| state.enqueue_user_turn(&p.conversation, "Keep an accepted ordinary claim", Vec::new())).await.unwrap();
    let mut disk = f.disk_json();
    let work = disk["workQueue"].as_array_mut().unwrap();
    work.retain(|entry| entry["id"] != first.id);
    let mut stale = serde_json::to_value(&second).unwrap();
    stale["id"] = new_id().into();
    stale["workerId"] = new_id().into();
    work.insert(0, stale);
    write_json(&f.state_file(), &disk);
    let restarted = f.reopen().await.unwrap();
    let queue = restarted.read(|state| state.disk().work_queue.clone());
    assert_eq!(queue.len(), 3);
    assert_eq!(queue[0], second);
    assert_eq!(queue[1], user);
    assert_eq!(queue[2].kind, first.kind);
    assert_ne!(queue[2].id, first.id, "only a genuinely missing pointer acquires a new queue ID");
}

#[tokio::test]
async fn hidden_child_user_forgery_and_invalid_worker_pointer_payloads_are_rejected_on_load() {
    let f = configured(2).await;
    let p = parent(&f).await;
    let (_, worker) = spawn(&f, &p, "A protected hidden child").await;
    f.finish(&p.conversation).await;
    let original = f.disk_json();
    for change in [0, 1, 2] {
        let mut disk = original.clone();
        match change {
            0 => {
                disk["workQueue"][0]["workerId"] = "not-a-uuid".into();
            }
            1 => {
                disk["workQueue"][0]["text"] = "Unexpected inline worker prompt".into();
            }
            2 => {
                disk["workQueue"].as_array_mut().unwrap().push(serde_json::json!({
                    "id": new_id(), "botId": f.bot, "conversationId": worker.conversation_id,
                    "createdAt": f.iso_now(), "kind": "user", "text": "Forged manual hidden input", "attachments": []
                }));
            }
            _ => unreachable!(),
        }
        let bytes = write_json(&f.state_file(), &disk);
        assert_load_rejected(&f, &bytes).await;
    }
    write_json(&f.state_file(), &original);
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn legacy_worker_run_owner_can_be_absent_but_never_retargets_a_worker_to_another_bot() {
    let f = configured(2).await;
    let p = parent(&f).await;
    let (b, worker) = spawn(&f, &p, "Legacy owner fixture").await;
    f.finish(&p.conversation).await;
    begin_worker(&f, &worker).await;
    let mut disk = f.disk_json();
    let child = disk["conversations"].as_array_mut().unwrap().iter_mut().find(|conversation| conversation["id"] == worker.conversation_id).unwrap();
    child["run"].as_object_mut().unwrap().remove("botId");
    write_json(&f.state_file(), &disk);
    let restarted = f.reopen().await.unwrap();
    assert_eq!(restarted.read(|state| state.worker_batch(&b.id).unwrap().workers[0].status), DeliveryStatus::Interrupted);
    assert!(!restarted.read(|state| state.disk().work_queue.iter().any(|work| matches!(work.kind, WorkKind::Worker { .. }))));
}

#[tokio::test]
async fn queue_and_worker_shape_upgrades_keep_independent_private_rollback_copies() {
    use std::os::unix::fs::PermissionsExt;
    let f = configured(2).await;
    let queue_backup = f.data_dir.join(PRE_WORK_QUEUE_BACKUP);
    let original_queue_backup = std::fs::read(&queue_backup).unwrap();
    let p = parent(&f).await;
    let before_worker = f.disk_text();
    let (_, worker) = spawn(&f, &p, "First worker after queue upgrade").await;
    let worker_backup = f.data_dir.join(PRE_WORKERS_BACKUP);
    assert_eq!(std::fs::read_to_string(&worker_backup).unwrap(), before_worker);
    f.finish(&p.conversation).await;
    worker_pointer(&f, &worker.id);
    assert_eq!(std::fs::read(&queue_backup).unwrap(), original_queue_backup);
    assert_eq!(std::fs::read_to_string(&worker_backup).unwrap(), before_worker);
    for backup in [queue_backup, worker_backup] {
        assert_eq!(std::fs::metadata(backup).unwrap().permissions().mode() & 0o777, 0o600);
    }
    f.reopen().await.unwrap();
}
