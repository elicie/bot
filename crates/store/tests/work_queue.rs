//! Focused persistence/FIFO tests for the durable cross-source queue and executing-bot ownership.

mod common;

use common::{Fixture, assert_load_rejected, write_json};
use grok_bot_model::*;
use grok_bot_store::{PRE_WORK_QUEUE_BACKUP, State};

fn bot_input(name: &str) -> BotInput {
    BotInput { name: name.into(), color: None, instructions: None, shape: None, pinned: None }
}

fn settings(cap: Option<u32>) -> SettingsInput {
    SettingsInput {
        base_url: "http://localhost:8000/v1".into(),
        model: "test-model".into(),
        workspace: String::new(),
        api_key: None,
        clear_api_key: None,
        max_concurrent_runs: cap,
    }
}

async fn configured() -> Fixture {
    let f = Fixture::new().await;
    f.store.mutate(|state| state.update_settings(settings(None))).await.unwrap();
    f
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

fn start_source(state: &mut State, conversation_id: &str) -> HttpResult<(String, String)> {
    let now = iso(state.now());
    let run_id = new_id();
    let owner = state.conversation(conversation_id)?.bot_id.clone();
    let conversation = state.conversation_mut(conversation_id)?;
    conversation.messages.push(message(Role::User, "Send this request", &now));
    let assistant = message(Role::Assistant, "Sending", &now);
    let message_id = assistant.id.clone();
    conversation.messages.push(assistant);
    // Exercise legacy-owner fallback too; explicit owners have separate coverage below.
    conversation.run = RunState { id: run_id.clone(), status: RunStatus::Running, started_at: Some(now), ..RunState::idle() };
    assert_eq!(conversation.bot_id, owner);
    Ok((run_id, message_id))
}

async fn delivery(f: &Fixture, from_bot: &str, to_bot: &str) -> BotDelivery {
    f.store
        .mutate(|state| {
            let source = state.add_conversation(from_bot)?;
            let (run, message) = start_source(state, &source.id)?;
            state.queue_bot_message(
                &source.id,
                &run,
                &message,
                BotMessageInput { to_bot_id: to_bot.into(), text: "Please handle this".into(), request_id: None },
            )
        })
        .await
        .unwrap()
}

async fn start_delivery(f: &Fixture, delivery: &BotDelivery) {
    f.store
        .mutate(|state| {
            let now = iso(state.now());
            let mut input = message(Role::User, &delivery.text, &now);
            input.source_bot_id = Some(delivery.from_bot_id.clone());
            let conversation = state.conversation_mut(&delivery.recipient_conversation_id)?;
            conversation.messages.push(input);
            conversation.messages.push(message(Role::Assistant, "Handled", &now));
            conversation.run =
                RunState { id: new_id(), status: RunStatus::Running, bot_id: Some(delivery.to_bot_id.clone()), started_at: Some(now), ..RunState::idle() };
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn fifo_is_acceptance_order_across_sources_even_when_the_clock_moves_backwards() {
    let f = configured().await;
    let helper = f.store.mutate(|state| state.add_bot(bot_input("Helper"))).await.unwrap();
    let conversation = f.store.mutate(|state| state.add_conversation(&f.bot)).await.unwrap();
    let routine = f.store.mutate(|state| state.add_routine(f.input(RoutineSchedule::Interval { minutes: 5 }))).await.unwrap();
    f.set_time("2026-01-01T03:00:00.000Z");
    let first = f.store.mutate(|state| state.enqueue_user_turn(&conversation.id, "First accepted", Vec::new())).await.unwrap();
    f.set_time("2026-01-01T02:00:00.000Z");
    let run = f.store.mutate(|state| state.run_routine(&routine.id, f.now())).await.unwrap();
    f.set_time("2026-01-01T01:00:00.000Z");
    let incoming = delivery(&f, &helper.id, &f.bot).await;
    let last = f.store.mutate(|state| state.enqueue_user_turn(&conversation.id, "Last accepted", Vec::new())).await.unwrap();
    let queue = f.store.read(|state| state.disk().work_queue.clone());
    assert_eq!(queue.len(), 4);
    assert_eq!(queue[0].id, first.id);
    assert_eq!(queue[1].kind, WorkKind::Routine { run_id: run.id });
    assert_eq!(queue[2].kind, WorkKind::Delivery { delivery_id: incoming.id });
    assert_eq!(queue[3].id, last.id);
    assert!(queue.windows(2).any(|pair| pair[0].created_at > pair[1].created_at));
    let before = f.store.snapshot().revision;
    assert!(!f.store.mutate(|state| Ok(state.sync_work_queue())).await.unwrap());
    assert_eq!(f.store.snapshot().revision, before, "an already reconciled queue does not write");
    assert_eq!(f.store.read(|state| state.disk().work_queue.clone()), queue);
}

#[tokio::test]
async fn mixed_sources_in_one_mutation_append_in_method_acceptance_order() {
    let f = configured().await;
    let conversation = f.store.mutate(|state| state.add_conversation(&f.bot)).await.unwrap();
    let first = f.store.mutate(|state| state.add_routine(f.input(RoutineSchedule::Interval { minutes: 5 }))).await.unwrap();
    let second = f.store.mutate(|state| state.add_routine(f.input(RoutineSchedule::Interval { minutes: 6 }))).await.unwrap();
    let (one, user, two) = f
        .store
        .mutate(|state| {
            let one = state.run_routine(&first.id, f.now())?;
            let user = state.enqueue_user_turn(&conversation.id, "Between routines", Vec::new())?;
            let two = state.run_routine(&second.id, f.now())?;
            Ok((one, user, two))
        })
        .await
        .unwrap();
    let queue = f.store.read(|state| state.disk().work_queue.clone());
    assert_eq!(
        queue.iter().map(|work| &work.kind).collect::<Vec<_>>(),
        vec![&WorkKind::Routine { run_id: one.id }, &user.kind, &WorkKind::Routine { run_id: two.id }]
    );
    assert_eq!(queue[1].id, user.id);
}

#[tokio::test]
async fn user_work_is_durable_redacted_and_restart_interrupts_only_the_started_turn() {
    let f = configured().await;
    f.store.mutate(|state| state.update_settings(SettingsInput { api_key: Some("queue-secret".into()), ..settings(None) })).await.unwrap();
    let conversation = f.store.mutate(|state| state.add_conversation(&f.bot)).await.unwrap();
    let attachment = Attachment { id: new_id(), name: "queue-secret.txt".into(), mime_type: "text/plain".into(), kind: AttachmentKind::Text, size: 4 };
    let first = f.store.mutate(|state| state.enqueue_user_turn(&conversation.id, "queue-secret first", vec![attachment.clone()])).await.unwrap();
    let second = f.store.mutate(|state| state.enqueue_user_turn(&conversation.id, "queue-secret second", Vec::new())).await.unwrap();
    assert_eq!(f.conversation(&conversation.id).run.status, RunStatus::Idle);
    assert!(f.conversation(&conversation.id).messages.is_empty());
    assert_eq!(f.store.read(|state| state.active_count()), 0);
    let public = f.store.snapshot();
    let WorkKind::User { text, attachments } = &public.work_queue[0].kind else { panic!("user work") };
    assert_eq!(text, "[redacted] first");
    assert_eq!(attachments[0].name, "[redacted].txt");
    assert_eq!(attachments[0].id, attachment.id);
    assert_eq!(public.work_queue[0].id, first.id);
    assert!(f.disk_text().contains("queue-secret first"));
    let restarted = f.reopen().await.unwrap();
    assert_eq!(restarted.read(|state| state.disk().work_queue.clone()), vec![first.clone(), second.clone()]);
    restarted
        .mutate(|state| {
            assert_eq!(state.remove_queued_work(&first.id), Some(first.clone()));
            let conversation = state.conversation_mut(&conversation.id)?;
            conversation.run =
                RunState { id: new_id(), status: RunStatus::Running, bot_id: Some(f.bot.clone()), started_at: Some(f.iso_now()), ..RunState::idle() };
            Ok(())
        })
        .await
        .unwrap();
    restarted.flush().await.unwrap();
    drop(restarted);
    let restarted = f.reopen().await.unwrap();
    assert_eq!(restarted.read(|state| state.conversation(&conversation.id).unwrap().run.status), RunStatus::Interrupted);
    assert_eq!(restarted.read(|state| state.disk().work_queue.clone()), vec![second]);
    assert_eq!(restarted.read(|state| state.active_count()), 0);
}

#[tokio::test]
async fn cancellation_is_scoped_and_returns_metadata_without_touching_live_runs() {
    let f = configured().await;
    let one = f.store.mutate(|state| state.add_conversation(&f.bot)).await.unwrap();
    let two = f.store.mutate(|state| state.add_conversation(&f.bot)).await.unwrap();
    f.start(&one.id).await;
    let first = f.store.mutate(|state| state.enqueue_user_turn(&one.id, "Cancel one", Vec::new())).await.unwrap();
    let second = f.store.mutate(|state| state.enqueue_user_turn(&one.id, "Cancel all", Vec::new())).await.unwrap();
    let other = f.store.mutate(|state| state.enqueue_user_turn(&two.id, "Keep other conversation", Vec::new())).await.unwrap();
    assert_eq!(f.store.mutate(|state| state.cancel_user_turn(&two.id, &first.id)).await.unwrap_err().status, 404);
    assert_eq!(f.store.mutate(|state| state.cancel_user_turn(&one.id, &first.id)).await.unwrap(), first);
    assert_eq!(f.store.mutate(|state| state.cancel_user_turn(&one.id, &first.id)).await.unwrap_err().status, 404);
    assert_eq!(f.store.mutate(|state| state.cancel_user_turns(&one.id)).await.unwrap(), vec![second]);
    assert_eq!(f.conversation(&one.id).run.status, RunStatus::Running);
    assert_eq!(f.store.read(|state| state.disk().work_queue.clone()), vec![other]);
    assert!(!f.store.mutate(|state| state.cancel_conversation_work(&one.id)).await.unwrap());
    assert!(f.store.mutate(|state| state.cancel_conversation_work(&two.id)).await.unwrap());
    assert!(f.store.read(|state| state.disk().work_queue.is_empty()));
    assert!(f.reopen().await.unwrap().read(|state| state.disk().work_queue.is_empty()));
}

#[tokio::test]
async fn group_user_owner_uses_actual_first_mentioned_speaker_and_locks_only_that_bot() {
    let f = configured().await;
    let beta = f.store.mutate(|state| state.add_bot(bot_input("Beta"))).await.unwrap();
    let group = f
        .store
        .mutate(|state| state.add_group(GroupInput { name: "Team".into(), color: None, shape: None, member_bot_ids: vec![f.bot.clone(), beta.id.clone()] }))
        .await
        .unwrap();
    let conversation = f.store.mutate(|state| state.add_group_conversation(&group.id)).await.unwrap();
    let queued = f.store.mutate(|state| state.enqueue_user_turn(&conversation.id, "@Beta please answer", Vec::new())).await.unwrap();
    assert_eq!(queued.bot_id, beta.id);
    assert_eq!(queued.speaker_bot_ids, Some(vec![beta.id.clone()]));
    // Changing mention parsing after admission must not move accepted work to another bot's lane.
    f.store.mutate(|state| state.update_bot(&beta.id, BotPatch { name: Some("Renamed".into()), ..BotPatch::default() })).await.unwrap();
    assert_eq!(f.store.read(|state| state.disk().work_queue[0].clone()), queued);
    let restarted = f.reopen().await.unwrap();
    assert_eq!(restarted.read(|state| state.disk().work_queue[0].clone()), queued);
    drop(restarted);
    f.store
        .mutate(|state| {
            state.remove_queued_work(&queued.id).unwrap();
            let now = iso(state.now());
            let conversation = state.conversation_mut(&conversation.id)?;
            let mut assistant = message(Role::Assistant, "Answer", &now);
            assistant.speaker_bot_id = Some(beta.id.clone());
            conversation.messages.push(assistant);
            conversation.run = RunState { id: new_id(), status: RunStatus::Running, started_at: Some(now), ..RunState::idle() };
            Ok(())
        })
        .await
        .unwrap();
    f.store.read(|state| {
        assert!(state.active_for_bot(&beta.id).is_some());
        assert!(state.active_for_bot(&f.bot).is_none());
        assert_eq!(state.active_bot_ids(), std::collections::HashSet::from([beta.id.as_str()]));
        assert_eq!(state.assert_bot_idle(&beta.id).unwrap_err().status, 409);
        state.assert_bot_idle(&f.bot).unwrap();
        assert_eq!(state.assert_idle().unwrap_err().status, 409);
    });
    f.store.mutate(|state| state.update_bot(&f.bot, BotPatch { instructions: Some("Allowed while Beta runs".into()), ..BotPatch::default() })).await.unwrap();
    assert_eq!(
        f.store.mutate(|state| state.update_bot(&beta.id, BotPatch { instructions: Some("Blocked".into()), ..BotPatch::default() })).await.unwrap_err().status,
        409
    );
    // Explicit owner wins over a stale/historical speaker marker.
    f.store
        .mutate(|state| {
            state.conversation_mut(&conversation.id)?.run.bot_id = Some(f.bot.clone());
            Ok(())
        })
        .await
        .unwrap();
    f.store.read(|state| {
        assert!(state.active_for_bot(&f.bot).is_some());
        assert!(state.active_for_bot(&beta.id).is_none());
    });
}

#[tokio::test]
async fn group_continuation_is_not_queued_until_the_current_speaker_finishes() {
    let f = configured().await;
    let beta = f.store.mutate(|state| state.add_bot(bot_input("Beta"))).await.unwrap();
    let group = f
        .store
        .mutate(|state| state.add_group(GroupInput { name: "Team".into(), color: None, shape: None, member_bot_ids: vec![f.bot.clone(), beta.id.clone()] }))
        .await
        .unwrap();
    let conversation = f.store.mutate(|state| state.add_group_conversation(&group.id)).await.unwrap();
    let round_id = new_id();
    f.store
        .mutate(|state| {
            let now = iso(state.now());
            let conversation = state.conversation_mut(&conversation.id)?;
            let user = message(Role::User, "Current round", &now);
            conversation.group_round =
                Some(GroupRound { id: round_id.clone(), user_message_id: user.id.clone(), pending_bot_ids: vec![beta.id.clone()], created_at: now.clone() });
            conversation.messages.push(user);
            conversation.messages.push(message(Role::Assistant, "Current speaker", &now));
            conversation.run = RunState { id: new_id(), status: RunStatus::Running, bot_id: Some(f.bot.clone()), started_at: Some(now), ..RunState::idle() };
            Ok(())
        })
        .await
        .unwrap();
    let restarted = f.reopen().await.unwrap();
    restarted.read(|state| {
        let recovered = state.conversation(&conversation.id).unwrap();
        assert_eq!(recovered.run.status, RunStatus::Interrupted);
        assert_eq!(recovered.group_round.as_ref().unwrap().pending_bot_ids.as_slice(), std::slice::from_ref(&beta.id));
        assert!(
            state.disk().work_queue.iter().any(|work| work.bot_id == beta.id && matches!(work.kind, WorkKind::Group { .. })),
            "only unstarted group members recover; the interrupted current speaker never replays"
        );
    });
    drop(restarted);
    let next = f.store.mutate(|state| state.enqueue_user_turn(&conversation.id, "@Beta next round", Vec::new())).await.unwrap();
    assert_eq!(f.store.read(|state| state.disk().work_queue.clone()), vec![next.clone()]);
    f.finish(&conversation.id).await;
    let queue = f.store.read(|state| state.disk().work_queue.clone());
    assert_eq!(queue.len(), 2);
    assert_eq!(queue[0], next);
    assert_eq!(queue[1].kind, WorkKind::Group { round_id });
    assert_eq!(queue[1].bot_id, beta.id);
    // A raw pointer removal alone is not a claim; an unstarted source remains ready.
    let pointer = queue[1].id.clone();
    f.store
        .mutate(|state| {
            state.remove_queued_work(&pointer);
            Ok(())
        })
        .await
        .unwrap();
    assert!(matches!(f.store.read(|state| state.disk().work_queue[1].kind.clone()), WorkKind::Group { .. }));
    f.store.mutate(|state| state.cancel_conversation_work(&conversation.id)).await.unwrap();
    assert!(f.store.read(|state| state.disk().work_queue.is_empty()));
}

#[tokio::test]
async fn started_failed_cancelled_specialized_work_is_not_requeued_after_restart() {
    let f = configured().await;
    let routine = f.store.mutate(|state| state.add_routine(f.input(RoutineSchedule::Interval { minutes: 5 }))).await.unwrap();
    let active = f.store.mutate(|state| state.run_routine(&routine.id, f.now())).await.unwrap();
    f.start(&active.conversation_id).await;
    assert!(f.store.read(|state| state.disk().work_queue.is_empty()));
    let restarted = f.reopen().await.unwrap();
    assert_eq!(restarted.read(|state| state.disk().routine_runs[0].result.as_ref().unwrap().status), RoutineResultStatus::Interrupted);
    assert!(restarted.read(|state| state.disk().work_queue.is_empty()));
    let pending = restarted.mutate(|state| state.run_routine(&routine.id, f.now())).await.unwrap();
    restarted.mutate(|state| state.fail_routine_run(&pending.id, "Could not start", f.now())).await.unwrap();
    assert!(restarted.read(|state| state.disk().work_queue.is_empty()));
    let pending = restarted.mutate(|state| state.run_routine(&routine.id, f.now())).await.unwrap();
    assert!(restarted.mutate(|state| state.cancel_conversation_work(&pending.conversation_id)).await.unwrap());
    assert!(restarted.read(|state| state.disk().work_queue.is_empty()));
    restarted.flush().await.unwrap();
    drop(restarted);
    assert!(f.reopen().await.unwrap().read(|state| state.disk().work_queue.is_empty()));
}

#[tokio::test]
async fn waiting_children_releases_the_bot_and_report_pointer_is_local_and_stable() {
    let f = configured().await;
    let beta = f.store.mutate(|state| state.add_bot(bot_input("Beta"))).await.unwrap();
    let gamma = f.store.mutate(|state| state.add_bot(bot_input("Gamma"))).await.unwrap();
    let first = delivery(&f, &f.bot, &beta.id).await;
    start_delivery(&f, &first).await;
    let child = f
        .store
        .mutate(|state| {
            let conversation = state.conversation(&first.recipient_conversation_id)?;
            let run = conversation.run.id.clone();
            let message = conversation.messages.last().unwrap().id.clone();
            state.queue_bot_message(
                &first.recipient_conversation_id,
                &run,
                &message,
                BotMessageInput { to_bot_id: gamma.id.clone(), text: "Need your help".into(), request_id: None },
            )
        })
        .await
        .unwrap();
    f.finish(&first.recipient_conversation_id).await;
    f.finish(&first.source_conversation_id).await;
    f.store.read(|state| {
        assert_eq!(state.disk().bot_deliveries[0].status, DeliveryStatus::WaitingChildren);
        assert_eq!(state.active_count(), 0);
        state.assert_bot_idle(&beta.id).unwrap();
        assert!(!state.disk().work_queue.iter().any(|work| matches!(&work.kind, WorkKind::Delivery { delivery_id } if delivery_id == &first.id)));
    });
    let plain = f.store.mutate(|state| state.add_conversation(&beta.id)).await.unwrap();
    assert!(f.store.read(|state| state.can_start(&beta.id, &plain.id)));
    start_delivery(&f, &child).await;
    f.finish(&child.recipient_conversation_id).await;
    let report = f.store.read(|state| state.disk().work_queue.iter().find(|work| matches!(work.kind, WorkKind::Report { .. })).unwrap().clone());
    assert_eq!(report.bot_id, beta.id);
    assert_eq!(report.conversation_id, first.recipient_conversation_id);
    assert_eq!(report.kind, WorkKind::Report { delivery_id: child.id.clone() });
    assert!(!f.store.mutate(|state| Ok(state.sync_work_queue())).await.unwrap());
    assert!(f.store.mutate(|state| state.cancel_conversation_work(&first.recipient_conversation_id)).await.unwrap());
    assert_eq!(f.conversation(&child.recipient_conversation_id).run.status, RunStatus::Completed, "local cancellation does not stop a different conversation");
    assert!(f.store.read(|state| !state.disk().work_queue.iter().any(|work| work.id == report.id)));
}

#[tokio::test]
async fn report_start_failure_uses_accepted_anchor_not_a_later_reversed_clock() {
    let f = configured().await;
    let beta = f.store.mutate(|state| state.add_bot(bot_input("Report peer"))).await.unwrap();
    f.set_time("2026-01-01T03:00:00.000Z");
    let first = delivery(&f, &f.bot, &beta.id).await;
    f.finish(&first.source_conversation_id).await;
    start_delivery(&f, &first).await;
    f.finish(&first.recipient_conversation_id).await;
    let anchor = f.store.read(|state| {
        state.disk().work_queue.iter().find(|work| matches!(&work.kind, WorkKind::Report { delivery_id } if delivery_id == &first.id)).unwrap().clone()
    });
    f.set_time("2026-01-01T02:00:00.000Z");
    let second = f
        .store
        .mutate(|state| {
            let (run, message) = start_source(state, &first.source_conversation_id)?;
            state.queue_bot_message(
                &first.source_conversation_id,
                &run,
                &message,
                BotMessageInput { to_bot_id: beta.id.clone(), text: "A second independent chain".into(), request_id: None },
            )
        })
        .await
        .unwrap();
    f.finish(&first.source_conversation_id).await;
    start_delivery(&f, &second).await;
    f.finish(&second.recipient_conversation_id).await;
    assert_eq!(
        f.store.read(|state| grok_bot_store::bot_messages::queued_reports(&state.disk().bot_deliveries, &first.source_conversation_id)[0].id.clone()),
        second.id
    );
    assert!(f.store.read(|state| state.disk().work_queue.contains(&anchor)), "accepted report anchor is stable despite the backwards clock");
    f.store
        .mutate(|state| {
            state.fail_bot_reports_for(&first.source_conversation_id, Some(&first.id), "The selected report could not start", state.now())?;
            state.remove_queued_work(&anchor.id);
            Ok(())
        })
        .await
        .unwrap();
    f.store.read(|state| {
        let one = state.disk().bot_deliveries.iter().find(|delivery| delivery.id == first.id).unwrap();
        let two = state.disk().bot_deliveries.iter().find(|delivery| delivery.id == second.id).unwrap();
        assert_eq!(one.report.as_ref().unwrap().status, ReportStatus::Error);
        assert_eq!(two.report.as_ref().unwrap().status, ReportStatus::Queued, "a different chain's report must not inherit the failure");
        assert!(state.disk().work_queue.iter().any(|work| matches!(&work.kind, WorkKind::Report { delivery_id } if delivery_id == &second.id)));
    });
    f.reopen().await.expect("selected-anchor failure receipts and remaining queue survive restart");
}

#[tokio::test]
async fn migration_sorts_only_the_missing_tail_by_historical_time() {
    let f = configured().await;
    let one = f.store.mutate(|state| state.add_routine(f.input(RoutineSchedule::Interval { minutes: 5 }))).await.unwrap();
    let two = f.store.mutate(|state| state.add_routine(f.input(RoutineSchedule::Interval { minutes: 6 }))).await.unwrap();
    f.set_time("2026-01-01T03:00:00.000Z");
    let later = f.store.mutate(|state| state.run_routine(&one.id, f.now())).await.unwrap();
    f.set_time("2026-01-01T01:00:00.000Z");
    let earlier = f.store.mutate(|state| state.run_routine(&two.id, f.now())).await.unwrap();
    let queue = f.store.read(|state| state.disk().work_queue.clone());
    assert_eq!(queue[0].kind, WorkKind::Routine { run_id: later.id.clone() });
    let mut disk = f.disk_json();
    disk.as_object_mut().unwrap().remove("workQueue");
    write_json(&f.state_file(), &disk);
    let restarted = f.reopen().await.unwrap();
    let migrated = restarted.read(|state| state.disk().work_queue.clone());
    assert_eq!(
        migrated.iter().map(|work| work.kind.clone()).collect::<Vec<_>>(),
        vec![WorkKind::Routine { run_id: earlier.id }, WorkKind::Routine { run_id: later.id }]
    );
    assert_eq!(restarted.snapshot().work_queue, migrated);
}

#[tokio::test]
async fn concurrency_default_bounds_bot_idle_and_rollback_backup_are_preserved() {
    use std::os::unix::fs::PermissionsExt;

    let f = configured().await;
    assert_eq!(f.store.read(|state| state.settings().max_concurrent_runs()), 3);
    assert_eq!(f.store.snapshot().settings.max_concurrent_runs, Some(3));
    assert!(f.disk_json()["settings"].get("maxConcurrentRuns").is_none());
    assert_eq!(f.store.snapshot().work_queue, Vec::new());
    for cap in [0, 17] {
        let before = f.disk_text();
        assert_eq!(f.store.mutate(|state| state.update_settings(settings(Some(cap)))).await.unwrap_err().status, 400);
        assert_eq!(f.disk_text(), before);
    }
    let legacy = f.disk_text();
    f.store.mutate(|state| state.update_settings(settings(Some(1)))).await.unwrap();
    let backup = f.data_dir.join(PRE_WORK_QUEUE_BACKUP);
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), legacy);
    assert_eq!(std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777, 0o600);
    f.store.mutate(|state| state.update_settings(settings(None))).await.unwrap();
    assert_eq!(f.store.read(|state| state.settings().max_concurrent_runs()), 1);
    let beta = f.store.mutate(|state| state.add_bot(bot_input("Beta"))).await.unwrap();
    let active = f.store.mutate(|state| state.add_conversation(&f.bot)).await.unwrap();
    let waiting = f.store.mutate(|state| state.add_conversation(&beta.id)).await.unwrap();
    f.start(&active.id).await;
    assert!(!f.store.read(|state| state.can_start(&beta.id, &waiting.id)), "the global cap applies even across bots");
    f.store.mutate(|state| state.update_bot(&beta.id, BotPatch { instructions: Some("Allowed".into()), ..BotPatch::default() })).await.unwrap();
    assert_eq!(f.store.mutate(|state| state.update_settings(settings(Some(2)))).await.unwrap_err().status, 409);
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), legacy, "later writes never overwrite the rollback point");
}

#[tokio::test]
async fn corrupt_queue_owner_payload_and_cap_are_rejected_without_replacing_disk() {
    let f = configured().await;
    let beta = f.store.mutate(|state| state.add_bot(bot_input("Beta"))).await.unwrap();
    let conversation = f.store.mutate(|state| state.add_conversation(&f.bot)).await.unwrap();
    f.store.mutate(|state| state.enqueue_user_turn(&conversation.id, "Keep this", Vec::new())).await.unwrap();
    let valid = f.disk_json();
    for altered in [
        {
            let mut disk = valid.clone();
            let duplicate = disk["workQueue"][0].clone();
            disk["workQueue"].as_array_mut().unwrap().push(duplicate);
            disk
        },
        {
            let mut disk = valid.clone();
            disk["workQueue"][0]["botId"] = serde_json::json!(beta.id);
            disk
        },
        {
            let mut disk = valid.clone();
            disk["workQueue"][0]["text"] = serde_json::json!("");
            disk
        },
        {
            let mut disk = valid.clone();
            disk["settings"]["maxConcurrentRuns"] = serde_json::json!(17);
            disk
        },
        {
            let mut disk = valid.clone();
            disk["workQueue"][0]["extra"] = serde_json::json!("not allowed");
            disk
        },
    ] {
        let bytes = write_json(&f.state_file(), &altered);
        assert_load_rejected(&f, &bytes).await;
    }
}
