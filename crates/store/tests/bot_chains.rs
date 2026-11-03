//! Issue12: durable recipient handling turns, reverse-order reports, cancellation, and deadlines.
mod common;

use common::{Fixture, assert_load_rejected, write_json};
use grok_bot_model::*;
use grok_bot_store::Store;
use grok_bot_store::bot_messages::{
    BOT_CHAIN_TIMEOUT_ERROR, MAX_BOT_DELIVERIES_PER_CHAIN, MAX_BOT_REPORTS_PER_CHAIN, MAX_BOT_TURNS_PER_CHAIN, queued_reports, valid_bot_message_references,
};
use serde_json::{Value, json};
use std::collections::HashSet;

#[derive(Clone)]
struct Turn {
    conversation: String,
    run: String,
    answer: String,
}

fn message(role: Role, text: &str, now: &str) -> Message {
    Message {
        id: new_id(),
        role,
        text: text.into(),
        created_at: now.into(),
        tools: vec![],
        bot_exchange_ids: None,
        source_bot_id: None,
        speaker_bot_id: None,
        attachments: None,
        report_delivery_ids: None,
    }
}

async fn bots(f: &Fixture, count: usize) -> Vec<String> {
    let mut bots = vec![f.bot_id()];
    for index in 1..count {
        bots.push(
            f.store
                .mutate(|state| state.add_bot(BotInput { name: format!("Bot {index}"), color: None, instructions: None, shape: None, pinned: None }))
                .await
                .unwrap()
                .id,
        );
    }
    bots
}

async fn start(f: &Fixture, conversation_id: &str, input: Option<&BotDelivery>, text: &str) -> Turn {
    let (run, answer, now) = (new_id(), new_id(), f.iso_now());
    f.store
        .mutate(|state| {
            let conversation = state.conversation_mut(conversation_id)?;
            if let Some(delivery) = input {
                conversation.messages.push(Message { source_bot_id: Some(delivery.from_bot_id.clone()), ..message(Role::User, &delivery.text, &now) });
            }
            conversation.messages.push(Message { id: answer.clone(), ..message(Role::Assistant, text, &now) });
            conversation.run = RunState { id: run.clone(), status: RunStatus::Running, started_at: Some(now), ..RunState::idle() };
            Ok(())
        })
        .await
        .unwrap();
    Turn { conversation: conversation_id.into(), run, answer }
}

async fn root(f: &Fixture, bot: &str) -> Turn {
    let conversation = f.store.mutate(|state| state.add_conversation(bot)).await.unwrap();
    start(f, &conversation.id, None, "Initial answer").await
}

async fn receive(f: &Fixture, delivery: &BotDelivery, text: &str) -> Turn {
    start(f, &delivery.recipient_conversation_id, Some(delivery), text).await
}

async fn send(f: &Fixture, turn: &Turn, bot: &str, text: &str) -> HttpResult<BotDelivery> {
    f.store
        .mutate(|state| {
            state.queue_bot_message(&turn.conversation, &turn.run, &turn.answer, BotMessageInput { to_bot_id: bot.into(), text: text.into(), request_id: None })
        })
        .await
}

async fn finish(f: &Fixture, turn: &Turn, text: &str, status: RunStatus) {
    let now = f.iso_now();
    f.store
        .mutate(|state| {
            let conversation = state.conversation_mut(&turn.conversation)?;
            conversation.messages.iter_mut().find(|message| message.id == turn.answer).unwrap().text = text.into();
            conversation.run.status = status;
            conversation.run.finished_at = Some(now);
            conversation.run.error = (status == RunStatus::Error).then(|| "Fixture provider failed.".into());
            Ok(())
        })
        .await
        .unwrap();
}

async fn report(f: &Fixture, conversation_id: &str) -> Turn {
    let (run, answer, now) = (new_id(), new_id(), f.iso_now());
    f.store
        .mutate(|state| {
            let queued = queued_reports(&state.disk().bot_deliveries, conversation_id);
            let chain = queued.first().expect("one report must be queued").chain_id.clone();
            let ids: Vec<String> = queued.iter().filter(|delivery| delivery.chain_id == chain).map(|delivery| delivery.id.clone()).collect();
            let conversation = state.conversation_mut(conversation_id)?;
            conversation.messages.push(Message { report_delivery_ids: Some(ids.clone()), ..message(Role::User, "{\"botReplies\":[]}", &now) });
            conversation.messages.push(Message { id: answer.clone(), ..message(Role::Assistant, "", &now) });
            conversation.run = RunState { id: run.clone(), status: RunStatus::Running, started_at: Some(now), ..RunState::idle() };
            for delivery in state.disk_mut().bot_deliveries.iter_mut().filter(|delivery| ids.contains(&delivery.id)) {
                delivery.report = Some(DeliveryReport { status: ReportStatus::Running, run_id: Some(run.clone()), ..DeliveryReport::queued() });
            }
            Ok(())
        })
        .await
        .unwrap();
    Turn { conversation: conversation_id.into(), run, answer }
}

fn delivery(f: &Fixture, id: &str) -> BotDelivery {
    f.store.read(|state| state.disk().bot_deliveries.iter().find(|delivery| delivery.id == id).unwrap().clone())
}

fn exchange(f: &Fixture, delivery: &BotDelivery) -> BotExchange {
    f.store.read(|state| state.disk().bot_exchanges.iter().find(|exchange| exchange.id == delivery.exchange_id).unwrap().clone())
}

fn markers(f: &Fixture, conversation_id: &str) -> usize {
    f.conversation(conversation_id).messages.iter().filter(|message| message.report_delivery_ids.is_some()).count()
}

/// Validate the serialized state and independently open a copy, so testing an active disk does not
/// interrupt the fixture's actual run. Store::open validates before interrupting recovered runs.
async fn roundtrip(f: &Fixture) {
    let encoded = f.disk_text();
    let disk: DiskState = serde_json::from_str(&encoded).unwrap();
    let serialized = serde_json::to_string(&disk).unwrap();
    assert_eq!(serde_json::from_str::<DiskState>(&serialized).unwrap(), disk);
    let bots = disk.bots.iter().map(|bot| bot.id.as_str()).collect();
    let routines = disk.routine_runs.iter().map(|run| run.conversation_id.as_str()).collect();
    assert!(valid_bot_message_references(&disk.bot_deliveries, &disk.bot_exchanges, &disk.conversations, &bots, &routines));
    let copy = tempfile::tempdir().unwrap();
    std::fs::write(copy.path().join("state.json"), encoded).unwrap();
    let reopened = Store::open(copy.path(), f.clock()).await.expect("valid disk must load before recovery");
    reopened.flush().await.unwrap();
    Store::open(copy.path(), f.clock()).await.expect("recovered disk must load again");
}

async fn three(f: &Fixture, bots: &[String]) -> (Turn, BotDelivery, Turn, BotDelivery) {
    let a = root(f, &bots[0]).await;
    let ab = send(f, &a, &bots[1], "A asks B").await.unwrap();
    finish(f, &a, "I asked B", RunStatus::Completed).await;
    let b = receive(f, &ab, "B's provisional answer").await;
    let bc = send(f, &b, &bots[2], "B asks C").await.unwrap();
    finish(f, &b, "I need C's result", RunStatus::Completed).await;
    (a, ab, b, bc)
}

#[tokio::test]
async fn a_b_c_waits_then_reports_c_b_a_exactly_once_across_durable_roundtrips() {
    let f = Fixture::new().await;
    let bots = bots(&f, 3).await;
    let (a, ab, b, bc) = three(&f, &bots).await;
    let waiting = delivery(&f, &ab.id);
    assert_eq!(waiting.status, DeliveryStatus::WaitingChildren);
    assert_eq!((waiting.finished_at, waiting.report), (None, None));
    assert_eq!(exchange(&f, &ab).messages.len(), 1);
    assert!(f.store.read(|state| queued_reports(&state.disk().bot_deliveries, &a.conversation).is_empty()));
    roundtrip(&f).await;

    let c = receive(&f, &bc, "C's final answer").await;
    finish(&f, &c, "C's final answer", RunStatus::Completed).await;
    assert_eq!(delivery(&f, &bc.id).report.unwrap().status, ReportStatus::Queued);
    assert_eq!(delivery(&f, &ab.id).status, DeliveryStatus::WaitingChildren);
    f.set_time("2026-01-01T00:01:00.000Z");
    let summary_b = report(&f, &b.conversation).await;
    let handling = delivery(&f, &ab.id);
    assert_eq!(handling.status, DeliveryStatus::WaitingChildren);
    assert_eq!(handling.execution_run_id.as_deref(), Some(summary_b.run.as_str()));
    assert_ne!(handling.execution_run_id.as_deref(), Some(b.run.as_str()));
    assert_eq!(handling.started_at, waiting.started_at, "report turns preserve the first recipient start");
    assert_ne!(handling.started_at, Some(f.iso_now()));
    assert_eq!(exchange(&f, &ab).messages.len(), 1);
    roundtrip(&f).await;

    finish(&f, &summary_b, "B summary including C", RunStatus::Completed).await;
    assert_eq!(delivery(&f, &bc.id).report.unwrap().status, ReportStatus::Completed);
    assert_eq!(delivery(&f, &ab.id).status, DeliveryStatus::Completed);
    assert_eq!(exchange(&f, &ab).messages[1].text, "B summary including C");
    assert_eq!(exchange(&f, &ab).messages[1].id, summary_b.answer);
    assert_eq!(delivery(&f, &ab.id).report.unwrap().status, ReportStatus::Queued);
    roundtrip(&f).await;

    let summary_a = report(&f, &a.conversation).await;
    finish(&f, &summary_a, "A summary including B and C", RunStatus::Completed).await;
    assert_eq!((markers(&f, &a.conversation), markers(&f, &b.conversation), markers(&f, &c.conversation)), (1, 1, 0));
    assert_eq!(delivery(&f, &ab.id).report.unwrap().status, ReportStatus::Completed);
    let before = f.store.read(|state| state.disk().clone());
    for _ in 0..3 {
        f.store
            .mutate(|state| {
                state.disk_mut();
                Ok(())
            })
            .await
            .unwrap();
    }
    f.store.read(|state| {
        assert_eq!(state.disk().bot_deliveries, before.bot_deliveries);
        assert_eq!(state.disk().bot_exchanges, before.bot_exchanges);
        assert_eq!(state.disk().conversations, before.conversations);
    });
    roundtrip(&f).await;
}

#[tokio::test]
async fn nested_followup_send_uses_reported_child_hop_and_all_conversation_owned_work_blocks_parent() {
    let f = Fixture::new().await;
    let bots = bots(&f, 4).await;
    let (a, ab, b, bc) = three(&f, &bots).await;
    let c = receive(&f, &bc, "C answer").await;
    finish(&f, &c, "C answer", RunStatus::Completed).await;
    let b_report = report(&f, &b.conversation).await;
    let bd = send(&f, &b_report, &bots[3], "B needs D after C").await.unwrap();
    assert_eq!((bd.chain_id.as_str(), bd.hop), (ab.chain_id.as_str(), 3));
    finish(&f, &b_report, "Still need D", RunStatus::Completed).await;
    assert_eq!(delivery(&f, &bc.id).report.unwrap().status, ReportStatus::Completed);
    assert_eq!(delivery(&f, &ab.id).status, DeliveryStatus::WaitingChildren);
    assert!(delivery(&f, &ab.id).report.is_none());
    // C is the causal parent of this send, but B's recipient conversation owns its wait.
    roundtrip(&f).await;
    let d = receive(&f, &bd, "D answer").await;
    finish(&f, &d, "D answer", RunStatus::Completed).await;
    let final_b = report(&f, &b.conversation).await;
    finish(&f, &final_b, "B combined C and D", RunStatus::Completed).await;
    assert_eq!(delivery(&f, &ab.id).status, DeliveryStatus::Completed);
    assert_eq!(exchange(&f, &ab).messages[1].text, "B combined C and D");
    assert_eq!(markers(&f, &b.conversation), 2);
    assert_eq!(markers(&f, &a.conversation), 0);
    roundtrip(&f).await;
}

#[tokio::test]
async fn four_hops_unwind_every_mandatory_internal_report_despite_original_cap_two() {
    let f = Fixture::new().await;
    let bots = bots(&f, 5).await;
    let a = root(&f, &bots[0]).await;
    let mut sender = a.clone();
    let mut deliveries = Vec::new();
    for (hop, bot) in bots.iter().enumerate().skip(1) {
        let next = send(&f, &sender, bot, &format!("Hop {hop}")).await.unwrap();
        finish(&f, &sender, "Provisional", RunStatus::Completed).await;
        sender = receive(&f, &next, "Working").await;
        assert_eq!(next.hop, hop as u32);
        deliveries.push(next);
    }
    let error = send(&f, &sender, &bots[0], "Hop five").await.unwrap_err();
    assert_eq!(error.status, 409);
    assert!(error.message.contains("four hops"));
    finish(&f, &sender, "Deepest answer", RunStatus::Completed).await;
    roundtrip(&f).await;
    for index in (0..4).rev() {
        let child = &deliveries[index];
        assert_eq!(delivery(&f, &child.id).report.unwrap().status, ReportStatus::Queued);
        let summary = report(&f, &child.source_conversation_id).await;
        finish(&f, &summary, &format!("Summary {index}"), RunStatus::Completed).await;
        assert_eq!(delivery(&f, &child.id).report.unwrap().status, ReportStatus::Completed);
        if index > 0 {
            assert_eq!(delivery(&f, &deliveries[index - 1].id).status, DeliveryStatus::Completed);
            assert_eq!(exchange(&f, &deliveries[index - 1]).messages[1].text, format!("Summary {index}"));
        }
        roundtrip(&f).await;
    }
    assert_eq!(MAX_BOT_REPORTS_PER_CHAIN, 2);
    assert_eq!(MAX_BOT_DELIVERIES_PER_CHAIN, 14);
    assert_eq!(MAX_BOT_TURNS_PER_CHAIN, 29);
    assert_eq!(
        f.store.read(|state| state
            .disk()
            .bot_deliveries
            .iter()
            .filter(|delivery| delivery.report.as_ref().is_some_and(|report| report.status == ReportStatus::Completed))
            .count()),
        4
    );
    assert_eq!(markers(&f, &a.conversation), 1);
}

fn json_delivery<'a>(state: &'a mut Value, id: &str) -> &'a mut Value {
    state["botDeliveries"].as_array_mut().unwrap().iter_mut().find(|delivery| delivery["id"] == id).unwrap()
}

fn json_conversation<'a>(state: &'a mut Value, id: &str) -> &'a mut Value {
    state["conversations"].as_array_mut().unwrap().iter_mut().find(|conversation| conversation["id"] == id).unwrap()
}

#[tokio::test]
async fn invalid_recipient_suffixes_and_latest_report_run_references_are_rejected() {
    let f = Fixture::new().await;
    let bots = bots(&f, 3).await;
    let (_, ab, b, bc) = three(&f, &bots).await;
    let c = receive(&f, &bc, "C answer").await;
    finish(&f, &c, "C answer", RunStatus::Completed).await;
    f.set_time("2026-01-01T00:01:00.000Z");
    let summary = report(&f, &b.conversation).await;
    finish(&f, &summary, "B summary", RunStatus::Completed).await;
    let original = f.disk_json();
    let mut corruptions = Vec::new();
    let mut plain = original.clone();
    json_conversation(&mut plain, &b.conversation)["messages"].as_array_mut().unwrap().extend([
        serde_json::to_value(message(Role::User, "Unauthorized recipient followup", &f.iso_now())).unwrap(),
        serde_json::to_value(message(Role::Assistant, "Forged", &f.iso_now())).unwrap(),
    ]);
    corruptions.push(plain);
    let mut wrong_marker = original.clone();
    json_conversation(&mut wrong_marker, &b.conversation)["messages"][2]["reportDeliveryIds"] = json!([ab.id]);
    corruptions.push(wrong_marker);
    let mut stale_execution = original.clone();
    json_delivery(&mut stale_execution, &ab.id)["executionRunId"] = b.run.clone().into();
    corruptions.push(stale_execution);
    let mut stale_start = original.clone();
    json_delivery(&mut stale_start, &ab.id)["startedAt"] = "2026-01-01T00:00:30.000Z".into();
    corruptions.push(stale_start);
    let mut wrong_run = original.clone();
    json_delivery(&mut wrong_run, &bc.id)["report"]["runId"] = new_id().into();
    corruptions.push(wrong_run);
    let mut source_bot = original.clone();
    json_conversation(&mut source_bot, &b.conversation)["messages"][2]["sourceBotId"] = bots[0].clone().into();
    corruptions.push(source_bot);
    let mut missing_pair = original.clone();
    json_conversation(&mut missing_pair, &b.conversation)["messages"].as_array_mut().unwrap().pop();
    corruptions.push(missing_pair);
    let mut orphan_wait = original.clone();
    let target = json_delivery(&mut orphan_wait, &ab.id);
    target["status"] = "waiting_children".into();
    target.as_object_mut().unwrap().remove("finishedAt");
    target.as_object_mut().unwrap().remove("report");
    let target = orphan_wait["botExchanges"].as_array_mut().unwrap().iter_mut().find(|exchange| exchange["id"] == ab.exchange_id).unwrap();
    target["messages"].as_array_mut().unwrap().pop();
    corruptions.push(orphan_wait);
    let mut child_latch = original.clone();
    json_delivery(&mut child_latch, &bc.id)["chainTermination"] = json!({"status":"cancelled","finishedAt":f.iso_now()});
    corruptions.push(child_latch);
    let mut invalid_latch = original.clone();
    json_delivery(&mut invalid_latch, &ab.id)["chainTermination"] = json!({"status":"completed","finishedAt":f.iso_now()});
    corruptions.push(invalid_latch);
    for corruption in corruptions {
        let bytes = write_json(&f.state_file(), &corruption);
        assert_load_rejected(&f, &bytes).await;
    }
    write_json(&f.state_file(), &original);
    roundtrip(&f).await;
}

#[tokio::test]
async fn stop_latches_whole_chain_without_terminalizing_active_recipient_and_never_resurrects() {
    let f = Fixture::new().await;
    let bots = bots(&f, 4).await;
    let (a, ab, b, bc) = three(&f, &bots).await;
    let c = receive(&f, &bc, "Still running").await;
    let ids = f.store.read(|state| state.bot_chain_ids_for_cancel(&b.conversation, Some(&b.answer), &[]));
    assert_eq!(ids, HashSet::from([ab.chain_id.clone()]));
    assert_eq!(f.store.read(|state| state.bot_chain_ids_for_cancel(&a.conversation, None, &[])), ids);
    f.store.mutate(|state| state.terminate_bot_chains(&ids, RunStatus::Cancelled, None, f.now())).await.unwrap();
    assert_eq!(delivery(&f, &ab.id).status, DeliveryStatus::Cancelled);
    assert_eq!(delivery(&f, &ab.id).report.unwrap().status, ReportStatus::Skipped);
    assert_eq!(delivery(&f, &bc.id).status, DeliveryStatus::Running);
    assert_eq!(f.conversation(&c.conversation).run.status, RunStatus::Running);
    assert!(delivery(&f, &ab.id).chain_termination.is_some());
    assert_eq!(f.conversation(&a.conversation).run.status, RunStatus::Completed);
    let before = f.store.snapshot().revision;
    assert_eq!(send(&f, &c, &bots[3], "Must not escape Stop").await.unwrap_err().status, 409);
    assert_eq!(f.store.snapshot().revision, before);
    roundtrip(&f).await; // opening a copy interrupts the active engine, then enforces the root latch

    finish(&f, &c, "Engine finally stopped", RunStatus::Cancelled).await;
    f.store.mutate(|state| state.terminate_bot_chains(&ids, RunStatus::Error, Some("Later timeout loses"), f.now())).await.unwrap();
    for id in [&ab.id, &bc.id] {
        let delivery = delivery(&f, id);
        assert_eq!(delivery.status, DeliveryStatus::Cancelled);
        let report = delivery.report.unwrap();
        assert_eq!(report.status, ReportStatus::Skipped);
        assert!(report.error.is_some());
    }
    let revision = f.store.snapshot().revision;
    f.store.mutate(|state| state.terminate_bot_chains(&ids, RunStatus::Cancelled, None, f.now())).await.unwrap();
    assert_eq!(f.store.snapshot().revision, revision, "termination is idempotent");
    assert!(f.store.read(|state| state.bot_chain_ids_for_cancel(&a.conversation, None, &[])).is_empty());
    roundtrip(&f).await;
}

#[tokio::test]
async fn stop_ownership_excludes_unrelated_historical_work_during_current_user_turn() {
    let f = Fixture::new().await;
    let bots = bots(&f, 2).await;
    let first_turn = root(&f, &bots[0]).await;
    let first = send(&f, &first_turn, &bots[1], "First chain").await.unwrap();
    finish(&f, &first_turn, "Sent", RunStatus::Completed).await;
    let recipient = receive(&f, &first, "Answer").await;
    finish(&f, &recipient, "Answer", RunStatus::Completed).await;
    let current = start(&f, &first_turn.conversation, None, "Unrelated new user turn").await;
    assert!(f.store.read(|state| state.bot_chain_ids_for_cancel(&current.conversation, Some(&current.answer), &[])).is_empty());
    assert_eq!(
        f.store.read(|state| state.bot_chain_ids_for_cancel(&current.conversation, Some(&current.answer), std::slice::from_ref(&first.id))),
        HashSet::from([first.chain_id.clone()])
    );
    let ids = f.store.read(|state| state.bot_chain_ids_for_cancel(&current.conversation, None, &[]));
    f.store.mutate(|state| state.terminate_bot_chains(&ids, RunStatus::Cancelled, None, f.now())).await.unwrap();
    assert_eq!(f.conversation(&current.conversation).run.id, current.run);
    assert_eq!(f.conversation(&current.conversation).run.status, RunStatus::Running);
    assert_eq!(delivery(&f, &first.id).status, DeliveryStatus::Completed, "completed exchange history is immutable");
    assert_eq!(delivery(&f, &first.id).report.unwrap().status, ReportStatus::Skipped);
    roundtrip(&f).await;
}

#[tokio::test]
async fn deadline_uses_earliest_admission_and_aborts_waits_active_work_and_reports_durably() {
    let f = Fixture::new().await;
    let bots = bots(&f, 4).await;
    let a = root(&f, &bots[0]).await;
    let ab = send(&f, &a, &bots[1], "A asks B").await.unwrap();
    finish(&f, &a, "Sent", RunStatus::Completed).await;
    let b = receive(&f, &ab, "Working").await;
    f.set_time("2026-01-01T00:20:00.000Z");
    let bc = send(&f, &b, &bots[2], "B asks C after twenty minutes").await.unwrap();
    finish(&f, &b, "Waiting", RunStatus::Completed).await;
    let c = receive(&f, &bc, "Working").await;
    f.set_time("2026-01-01T00:29:59.999Z");
    assert!(f.store.read(|state| state.expired_bot_chains(f.now())).is_empty());
    f.set_time("2026-01-01T00:30:00.000Z");
    let ids = f.store.read(|state| state.expired_bot_chains(f.now()));
    assert_eq!(ids, HashSet::from([ab.chain_id.clone()]));
    assert_eq!(send(&f, &c, &bots[3], "No per-hop reset").await.unwrap_err().message, BOT_CHAIN_TIMEOUT_ERROR);
    f.store.mutate(|state| state.terminate_bot_chains(&ids, RunStatus::Error, Some(BOT_CHAIN_TIMEOUT_ERROR), f.now())).await.unwrap();
    assert_eq!(delivery(&f, &ab.id).status, DeliveryStatus::Error);
    assert_eq!(delivery(&f, &bc.id).status, DeliveryStatus::Running);
    finish(&f, &c, "A late success must not win", RunStatus::Completed).await;
    for id in [&ab.id, &bc.id] {
        let delivery = delivery(&f, id);
        assert_eq!(delivery.status, DeliveryStatus::Error);
        assert_eq!(delivery.error.as_deref(), Some(BOT_CHAIN_TIMEOUT_ERROR));
        assert_eq!(delivery.report.unwrap().status, ReportStatus::Skipped);
    }
    assert_eq!(delivery(&f, &ab.id).chain_termination.unwrap().status, RunStatus::Error);
    assert!(f.store.read(|state| state.expired_bot_chains(f.now())).is_empty());
    let settled = f.store.read(|state| state.disk().clone());
    f.store.mutate(|state| state.fail_bot_delivery(&bc.id, "Stale scheduler start failure", f.now() - chrono::Duration::minutes(1))).await.unwrap();
    assert_eq!(f.store.read(|state| state.disk().clone()), settled, "a stale scheduler failure cannot rewrite timeout receipts");
    roundtrip(&f).await;
}

#[tokio::test]
async fn recovery_interrupts_deepest_run_then_allows_parent_to_consume_failure_and_report_upstream() {
    let mut f = Fixture::new().await;
    let bots = bots(&f, 3).await;
    let (a, ab, b, bc) = three(&f, &bots).await;
    receive(&f, &bc, "Partial C answer").await;
    f.store = f.reopen().await.unwrap();
    assert_eq!(delivery(&f, &bc.id).status, DeliveryStatus::Interrupted);
    assert_eq!(delivery(&f, &bc.id).report.unwrap().status, ReportStatus::Queued);
    assert_eq!(delivery(&f, &ab.id).status, DeliveryStatus::WaitingChildren);
    assert!(delivery(&f, &ab.id).report.is_none());
    let summary_b = report(&f, &b.conversation).await;
    finish(&f, &summary_b, "C was interrupted; B reports that result", RunStatus::Completed).await;
    assert_eq!(exchange(&f, &ab).messages[1].text, "C was interrupted; B reports that result");
    let summary_a = report(&f, &a.conversation).await;
    finish(&f, &summary_a, "A has the recovered outcome", RunStatus::Completed).await;
    roundtrip(&f).await;
}

#[tokio::test]
async fn internal_report_start_failure_errors_waiting_parent_instead_of_completing_stale_answer() {
    let f = Fixture::new().await;
    let bots = bots(&f, 3).await;
    let (a, ab, b, bc) = three(&f, &bots).await;
    let c = receive(&f, &bc, "C final").await;
    finish(&f, &c, "C final", RunStatus::Completed).await;
    f.store.mutate(|state| state.fail_bot_reports(&b.conversation, "Cannot start B summary", f.now())).await.unwrap();
    assert_eq!(delivery(&f, &bc.id).report.unwrap().status, ReportStatus::Error);
    assert_eq!(f.conversation(&b.conversation).run.status, RunStatus::Error);
    assert_eq!(delivery(&f, &ab.id).status, DeliveryStatus::Error);
    assert_eq!(exchange(&f, &ab).messages.len(), 1, "never publish the provisional initial B answer");
    assert_eq!(delivery(&f, &ab.id).report.unwrap().status, ReportStatus::Queued);
    assert_eq!(markers(&f, &a.conversation), 0);
    roundtrip(&f).await;
}

#[tokio::test]
async fn report_start_failure_only_fails_oldest_same_chain_not_unrelated_queued_reports() {
    let f = Fixture::new().await;
    let bots = bots(&f, 2).await;
    let first_turn = root(&f, &bots[0]).await;
    let first = send(&f, &first_turn, &bots[1], "First").await.unwrap();
    finish(&f, &first_turn, "Sent", RunStatus::Completed).await;
    let b = receive(&f, &first, "First answer").await;
    finish(&f, &b, "First answer", RunStatus::Completed).await;
    f.set_time("2026-01-01T00:01:00.000Z");
    let second_turn = start(&f, &first_turn.conversation, None, "Next user answer").await;
    let second = send(&f, &second_turn, &bots[1], "Second").await.unwrap();
    finish(&f, &second_turn, "Sent", RunStatus::Completed).await;
    let b = receive(&f, &second, "Second answer").await;
    finish(&f, &b, "Second answer", RunStatus::Completed).await;
    assert_ne!(first.chain_id, second.chain_id);
    f.store.mutate(|state| state.fail_bot_reports(&first_turn.conversation, "First report failed", f.now())).await.unwrap();
    assert_eq!(delivery(&f, &first.id).report.unwrap().status, ReportStatus::Error);
    assert_eq!(delivery(&f, &second.id).report.unwrap().status, ReportStatus::Queued);
    roundtrip(&f).await;
}

#[tokio::test]
async fn legacy_completed_parent_with_pending_child_and_absent_reports_stays_loadable_without_replay() {
    let mut f = Fixture::new().await;
    let bots = bots(&f, 3).await;
    let (_, ab, b, bc) = three(&f, &bots).await;
    let mut legacy = f.disk_json();
    let recipient = json_conversation(&mut legacy, &b.conversation).clone();
    let target = json_delivery(&mut legacy, &ab.id);
    target["status"] = "completed".into();
    target["finishedAt"] = recipient["run"]["finishedAt"].clone();
    target.as_object_mut().unwrap().remove("report");
    target.as_object_mut().unwrap().remove("chainTermination");
    let answer = &recipient["messages"][1];
    let target = legacy["botExchanges"].as_array_mut().unwrap().iter_mut().find(|exchange| exchange["id"] == ab.exchange_id).unwrap();
    target["messages"].as_array_mut().unwrap().push(json!({
        "id": answer["id"], "botId": bots[1], "text": answer["text"], "createdAt": answer["createdAt"],
    }));
    write_json(&f.state_file(), &legacy);
    f.store = f.reopen().await.expect("PR9 valid completed parent plus queued child remains supported");
    assert_eq!(delivery(&f, &ab.id).status, DeliveryStatus::Completed);
    assert!(delivery(&f, &ab.id).report.is_none());
    assert_eq!(delivery(&f, &bc.id).status, DeliveryStatus::Queued);
    let c = receive(&f, &bc, "Legacy child eventually answered").await;
    finish(&f, &c, "Legacy child eventually answered", RunStatus::Completed).await;
    assert!(delivery(&f, &bc.id).report.is_none(), "do not synthesize even a skipped report for a closed legacy recipient");
    assert!(delivery(&f, &ab.id).report.is_none(), "never synthesize absent reports on terminal legacy deliveries");
    assert_eq!(f.conversation(&b.conversation).messages.len(), 2);
    assert_eq!(exchange(&f, &ab).messages[1].text, "I need C's result");
    roundtrip(&f).await;
}

#[tokio::test]
async fn provider_failure_cancels_queued_descendants_but_reports_the_failed_parent_upstream() {
    let f = Fixture::new().await;
    let bots = bots(&f, 3).await;
    let a = root(&f, &bots[0]).await;
    let ab = send(&f, &a, &bots[1], "A asks B").await.unwrap();
    finish(&f, &a, "Sent", RunStatus::Completed).await;
    let b = receive(&f, &ab, "Partial B answer").await;
    let bc = send(&f, &b, &bots[2], "B asks C").await.unwrap();
    finish(&f, &b, "Partial B answer", RunStatus::Error).await;
    assert_eq!(delivery(&f, &ab.id).status, DeliveryStatus::Error);
    assert_eq!(delivery(&f, &ab.id).report.unwrap().status, ReportStatus::Queued);
    assert_eq!(delivery(&f, &bc.id).status, DeliveryStatus::Cancelled);
    assert!(delivery(&f, &bc.id).report.is_none(), "failed recipients do not receive a newly synthesized report");
    assert!(delivery(&f, &ab.id).chain_termination.is_none(), "a provider error must still be reportable upstream");
    roundtrip(&f).await;
}

#[tokio::test]
async fn failed_recipient_suppresses_waiting_descendants_once_and_preserves_terminal_history() {
    let f = Fixture::new().await;
    let bots = bots(&f, 4).await;
    let (a, ab, b, bc) = three(&f, &bots).await;
    let c = receive(&f, &bc, "C needs D").await;
    let cd = send(&f, &c, &bots[3], "C asks D").await.unwrap();
    finish(&f, &c, "Waiting for D", RunStatus::Completed).await;
    assert_eq!(delivery(&f, &bc.id).status, DeliveryStatus::WaitingChildren);
    let before_a = f.conversation(&a.conversation);
    finish(&f, &b, "B failed while waiting", RunStatus::Error).await;
    assert_eq!(delivery(&f, &ab.id).status, DeliveryStatus::Error);
    assert_eq!(delivery(&f, &ab.id).report.unwrap().status, ReportStatus::Queued);
    assert!(delivery(&f, &ab.id).chain_termination.is_none());
    for child in [&bc, &cd] {
        let current = delivery(&f, &child.id);
        assert_eq!(current.status, DeliveryStatus::Cancelled);
        assert!(current.report.is_none(), "no new report is synthesized for a failed recipient");
        assert_eq!(exchange(&f, child).messages.len(), 1);
    }
    assert_eq!(f.conversation(&a.conversation), before_a);
    let settled = f.store.read(|state| state.disk().clone());
    f.set_time("2026-01-01T00:01:00.000Z");
    f.store
        .mutate(|state| {
            state.disk_mut();
            Ok(())
        })
        .await
        .unwrap();
    f.store.read(|state| {
        assert_eq!(state.disk().bot_deliveries, settled.bot_deliveries);
        assert_eq!(state.disk().bot_exchanges, settled.bot_exchanges);
        assert_eq!(state.disk().conversations, settled.conversations);
    });
    roundtrip(&f).await;
}

#[tokio::test]
async fn stop_and_timeout_after_nested_report_send_preserve_completed_report_receipts_and_reload() {
    for status in [RunStatus::Cancelled, RunStatus::Error] {
        let f = Fixture::new().await;
        let bots = bots(&f, 4).await;
        let (_, ab, b, bc) = three(&f, &bots).await;
        let c = receive(&f, &bc, "C answer").await;
        finish(&f, &c, "C answer", RunStatus::Completed).await;
        let summary = report(&f, &b.conversation).await;
        let bd = send(&f, &summary, &bots[3], "Still need D").await.unwrap();
        finish(&f, &summary, "Waiting for D", RunStatus::Completed).await;
        let receipt = delivery(&f, &bc.id).report.unwrap();
        assert_eq!(receipt.run_id.as_deref(), Some(summary.run.as_str()));
        assert_eq!(receipt.status, ReportStatus::Completed);
        let ids = HashSet::from([ab.chain_id.clone()]);
        f.store.mutate(|state| state.terminate_bot_chains(&ids, status, Some("Canonical aggregate stop"), f.now())).await.unwrap();
        assert_eq!(delivery(&f, &bc.id).report.unwrap(), receipt, "consumed report history is immutable");
        assert_eq!(f.conversation(&b.conversation).run.id, summary.run);
        assert_eq!(f.conversation(&b.conversation).run.status, status);
        assert_eq!(delivery(&f, &ab.id).status, if status == RunStatus::Cancelled { DeliveryStatus::Cancelled } else { DeliveryStatus::Error });
        assert_eq!(delivery(&f, &bd.id).report.unwrap().status, ReportStatus::Skipped);
        roundtrip(&f).await;
    }
}

#[tokio::test]
async fn next_internal_report_start_failure_preserves_previous_completed_report_and_reloads() {
    let f = Fixture::new().await;
    let bots = bots(&f, 4).await;
    let (_, ab, b, bc) = three(&f, &bots).await;
    let c = receive(&f, &bc, "C answer").await;
    finish(&f, &c, "C answer", RunStatus::Completed).await;
    let summary = report(&f, &b.conversation).await;
    let bd = send(&f, &summary, &bots[3], "Still need D").await.unwrap();
    finish(&f, &summary, "Waiting for D", RunStatus::Completed).await;
    let consumed = delivery(&f, &bc.id).report.unwrap();
    let d = receive(&f, &bd, "D answer").await;
    finish(&f, &d, "D answer", RunStatus::Completed).await;
    f.set_time("2026-01-01T00:02:00.000Z");
    f.store.mutate(|state| state.fail_bot_reports(&b.conversation, "Next internal summary cannot start", f.now())).await.unwrap();
    assert_eq!(delivery(&f, &bc.id).report.unwrap(), consumed);
    assert_eq!(delivery(&f, &bd.id).report.unwrap().status, ReportStatus::Error);
    assert_eq!(delivery(&f, &ab.id).status, DeliveryStatus::Error);
    assert_eq!(f.conversation(&b.conversation).run.status, RunStatus::Error);
    roundtrip(&f).await;
}

#[tokio::test]
async fn cancelling_queued_nested_report_cancels_parent_without_publishing_provisional_answer() {
    let mut f = Fixture::new().await;
    let bots = bots(&f, 3).await;
    let (a, ab, b, bc) = three(&f, &bots).await;
    let c = receive(&f, &bc, "C answer").await;
    finish(&f, &c, "C answer", RunStatus::Completed).await;
    let independent = root(&f, &bots[0]).await;
    let other = send(&f, &independent, &bots[2], "Independent root question").await.unwrap();
    finish(&f, &independent, "Sent independently", RunStatus::Completed).await;
    let other_recipient = receive(&f, &other, "Independent answer").await;
    finish(&f, &other_recipient, "Independent answer", RunStatus::Completed).await;
    let before_other = delivery(&f, &other.id);
    assert_eq!(before_other.report.as_ref().unwrap().status, ReportStatus::Queued);
    let unrelated = root(&f, &bots[0]).await;
    let before_b = f.conversation(&b.conversation);
    let before_a = f.conversation(&a.conversation);
    let before_unrelated = f.conversation(&unrelated.conversation);
    let before_exchanges = f.store.read(|state| state.disk().bot_exchanges.clone());

    f.set_time("2026-01-01T00:01:00.000Z");
    let before_selection = f.store.read(|state| state.disk().clone());
    assert_eq!(f.store.mutate(|state| state.cancel_bot_reports(&b.conversation, &[])).await.unwrap_err().status, 400);
    assert!(f.store.mutate(|state| state.cancel_bot_reports(&b.conversation, &[new_id()])).await.unwrap().is_empty());
    assert_eq!(f.store.read(|state| state.disk().clone()), before_selection, "invalid and unmatched selections cannot cancel a waiting aggregate");
    let selected = std::slice::from_ref(&bc.id);
    assert_eq!(f.store.mutate(|state| state.cancel_bot_reports(&b.conversation, selected)).await.unwrap(), selected);
    let cancelled = delivery(&f, &bc.id);
    assert_eq!(cancelled.status, DeliveryStatus::Completed, "the child's completed exchange is historical");
    assert_eq!(cancelled.report.unwrap(), DeliveryReport { status: ReportStatus::Cancelled, finished_at: Some(f.iso_now()), ..DeliveryReport::queued() });
    let parent = delivery(&f, &ab.id);
    assert_eq!(parent.status, DeliveryStatus::Cancelled);
    assert_eq!(parent.execution_run_id.as_deref(), Some(b.run.as_str()));
    assert!(parent.chain_termination.is_none(), "cancelling a pending report must not stop the whole chain");
    assert_eq!(parent.report.unwrap().status, ReportStatus::Queued, "the non-success still goes upstream");
    assert_eq!(exchange(&f, &ab).messages.len(), 1, "never publish B's provisional initial answer");
    let mut expected_b = before_b;
    expected_b.run.status = RunStatus::Cancelled;
    expected_b.run.finished_at = Some(f.iso_now());
    expected_b.updated_at = f.iso_now();
    assert_eq!(f.conversation(&b.conversation), expected_b, "no new run or message is created");
    assert_eq!(f.conversation(&a.conversation), before_a);
    assert_eq!(f.conversation(&unrelated.conversation), before_unrelated, "unrelated global activity is untouched");
    assert_eq!(delivery(&f, &other.id), before_other, "cancelling a nested aggregate cannot retire an independent root's queued report");
    assert_eq!(f.store.read(|state| state.disk().bot_exchanges.clone()), before_exchanges);
    let revision = f.store.snapshot().revision;
    assert!(f.store.mutate(|state| state.cancel_bot_reports(&b.conversation, selected)).await.unwrap().is_empty());
    assert_eq!(f.store.snapshot().revision, revision, "cancellation is idempotent");
    roundtrip(&f).await;

    finish(&f, &unrelated, "Unrelated answer", RunStatus::Completed).await;
    for _ in 0..2 {
        f.store = f.reopen().await.expect("cancelled nested report must reload without replay");
        assert_eq!(delivery(&f, &ab.id).status, DeliveryStatus::Cancelled);
        assert_eq!(delivery(&f, &bc.id).report.unwrap().status, ReportStatus::Cancelled);
        assert_eq!(delivery(&f, &other.id), before_other, "the independent root report remains queued across recovery");
        assert_eq!(markers(&f, &b.conversation), 0);
        f.store.read(|state| {
            assert_eq!(queued_reports(&state.disk().bot_deliveries, &a.conversation).iter().map(|delivery| &delivery.id).collect::<Vec<_>>(), [&ab.id]);
            assert!(queued_reports(&state.disk().bot_deliveries, &b.conversation).is_empty());
        });
    }
    let summary_a = report(&f, &a.conversation).await;
    finish(&f, &summary_a, "B was cancelled before its C summary", RunStatus::Completed).await;
    let settled = f.store.read(|state| state.disk().clone());
    for _ in 0..2 {
        f.store = f.reopen().await.unwrap();
        f.store
            .mutate(|state| {
                state.disk_mut();
                Ok(())
            })
            .await
            .unwrap();
        f.store.read(|state| {
            assert_eq!(state.disk().bot_deliveries, settled.bot_deliveries);
            assert_eq!(state.disk().bot_exchanges, settled.bot_exchanges);
            assert_eq!(state.disk().conversations, settled.conversations);
        });
    }
    assert_eq!(markers(&f, &a.conversation), 1, "report the cancelled aggregate upstream exactly once");
    assert_eq!(markers(&f, &b.conversation), 0);
    roundtrip(&f).await;
}

#[tokio::test]
async fn cancelling_next_nested_report_preserves_previous_completed_receipt_and_reloads() {
    let mut f = Fixture::new().await;
    let bots = bots(&f, 4).await;
    let (a, ab, b, bc) = three(&f, &bots).await;
    let c = receive(&f, &bc, "C answer").await;
    finish(&f, &c, "C answer", RunStatus::Completed).await;
    f.set_time("2026-01-01T00:01:00.000Z");
    let summary = report(&f, &b.conversation).await;
    let bd = send(&f, &summary, &bots[3], "Still need D").await.unwrap();
    finish(&f, &summary, "Waiting for D", RunStatus::Completed).await;
    let consumed = delivery(&f, &bc.id).report.unwrap();
    let d = receive(&f, &bd, "D answer").await;
    finish(&f, &d, "D answer", RunStatus::Completed).await;
    let history = f.conversation(&b.conversation).messages;

    f.set_time("2026-01-01T00:02:00.000Z");
    let before_selection = f.store.read(|state| state.disk().clone());
    assert!(f.store.mutate(|state| state.cancel_bot_reports(&b.conversation, std::slice::from_ref(&bc.id))).await.unwrap().is_empty());
    assert_eq!(f.store.read(|state| state.disk().clone()), before_selection, "selecting an already completed report must leave B waiting for D");
    let requested = [bc.id.clone(), bd.id.clone(), bd.id.clone(), new_id()];
    assert_eq!(f.store.mutate(|state| state.cancel_bot_reports(&b.conversation, &requested)).await.unwrap(), std::slice::from_ref(&bd.id));
    assert_eq!(delivery(&f, &bc.id).report.unwrap(), consumed, "a prior consumed report is immutable");
    assert_eq!(delivery(&f, &bd.id).report.unwrap().status, ReportStatus::Cancelled);
    assert_eq!(delivery(&f, &ab.id).status, DeliveryStatus::Cancelled);
    assert_eq!(f.conversation(&b.conversation).run.id, summary.run);
    assert_eq!(f.conversation(&b.conversation).run.status, RunStatus::Cancelled);
    assert_eq!(f.conversation(&b.conversation).messages, history);
    assert_eq!(exchange(&f, &ab).messages.len(), 1, "neither the initial answer nor the earlier summary is final");
    assert!(delivery(&f, &ab.id).chain_termination.is_none());
    roundtrip(&f).await;

    // The completed receipt override is narrow: a matching no-run cancellation proves why the
    // same latest report run is now cancelled. Unrelated terminal reports/times do not suffice.
    let original = f.disk_json();
    let mut wrong_status = original.clone();
    json_delivery(&mut wrong_status, &bd.id)["report"]["status"] = "skipped".into();
    let mut wrong_time = original.clone();
    json_delivery(&mut wrong_time, &bd.id)["report"]["finishedAt"] = "2026-01-01T00:01:30.000Z".into();
    let mut wrong_error = original.clone();
    json_delivery(&mut wrong_error, &bd.id)["report"]["error"] = "Different cancellation".into();
    for corruption in [wrong_status, wrong_time, wrong_error] {
        let bytes = write_json(&f.state_file(), &corruption);
        assert_load_rejected(&f, &bytes).await;
    }
    write_json(&f.state_file(), &original);
    for _ in 0..2 {
        f.store = f.reopen().await.expect("previous completed receipt plus next cancelled report remains valid");
        assert_eq!(delivery(&f, &bc.id).report.unwrap(), consumed);
        assert_eq!(markers(&f, &b.conversation), 1, "never replay C or start a cancelled D report");
        assert_eq!(delivery(&f, &ab.id).report.unwrap().status, ReportStatus::Queued);
    }
    let summary_a = report(&f, &a.conversation).await;
    finish(&f, &summary_a, "B was cancelled while waiting for D", RunStatus::Completed).await;
    f.store = f.reopen().await.unwrap();
    assert_eq!(markers(&f, &a.conversation), 1);
    assert_eq!(markers(&f, &b.conversation), 1);
    assert_eq!(delivery(&f, &ab.id).status, DeliveryStatus::Cancelled);
    assert_eq!(delivery(&f, &ab.id).report.unwrap().status, ReportStatus::Completed);
    assert_eq!(delivery(&f, &bc.id).report.unwrap(), consumed);
    assert_eq!(delivery(&f, &bd.id).report.unwrap().status, ReportStatus::Cancelled);
    assert!(
        f.store.read(|state| state.disk().bot_deliveries.iter().all(|delivery| { delivery.report.as_ref().is_none_or(|report| report.status.is_terminal()) }))
    );
    roundtrip(&f).await;
}

#[tokio::test]
async fn recipient_first_start_is_stable_and_prior_latest_start_disks_remain_loadable() {
    let f = Fixture::new().await;
    let bots = bots(&f, 3).await;
    let (_, ab, b, bc) = three(&f, &bots).await;
    let first_start = delivery(&f, &ab.id).started_at.unwrap();
    let c = receive(&f, &bc, "C answer").await;
    finish(&f, &c, "C answer", RunStatus::Completed).await;
    f.set_time("2026-01-01T00:01:00.000Z");
    let summary = report(&f, &b.conversation).await;
    for completed in [false, true] {
        if completed {
            finish(&f, &summary, "B summary", RunStatus::Completed).await;
        }
        assert_eq!(delivery(&f, &ab.id).started_at.as_deref(), Some(first_start.as_str()));
        let mut legacy = f.disk_json();
        json_delivery(&mut legacy, &ab.id)["startedAt"] = f.iso_now().into();
        let copy = tempfile::tempdir().unwrap();
        write_json(&copy.path().join("state.json"), &legacy);
        let reopened = Store::open(copy.path(), f.clock()).await.expect("prior PR27 latest-start data must remain loadable");
        reopened.read(|state| {
            let parent = state.disk().bot_deliveries.iter().find(|delivery| delivery.id == ab.id).unwrap();
            assert_eq!(parent.execution_run_id.as_deref(), Some(summary.run.as_str()));
            assert_eq!(parent.started_at.as_deref(), Some(if completed { f.iso_now() } else { first_start.clone() }.as_str()));
        });
        reopened.flush().await.unwrap();
        Store::open(copy.path(), f.clock()).await.expect("normalized or historical state must load again");
    }
    roundtrip(&f).await;
}

#[tokio::test]
async fn legacy_mixed_chain_root_report_retains_first_highest_hop_lineage() {
    let f = Fixture::new().await;
    let bots = bots(&f, 2).await;
    let initial = root(&f, &bots[0]).await;
    let first = send(&f, &initial, &bots[1], "First request").await.unwrap();
    finish(&f, &initial, "Sent", RunStatus::Completed).await;
    let recipient = receive(&f, &first, "First answer").await;
    finish(&f, &recipient, "First answer", RunStatus::Completed).await;
    let second_turn = start(&f, &initial.conversation, None, "Second user turn").await;
    let second = send(&f, &second_turn, &bots[1], "Second request").await.unwrap();
    finish(&f, &second_turn, "Sent", RunStatus::Completed).await;
    let recipient = receive(&f, &second, "Second answer").await;
    finish(&f, &recipient, "Second answer", RunStatus::Completed).await;
    let (run, answer, now) = (new_id(), new_id(), f.iso_now());
    f.store
        .mutate(|state| {
            let conversation = state.conversation_mut(&initial.conversation)?;
            conversation
                .messages
                .push(Message { report_delivery_ids: Some(vec![first.id.clone(), second.id.clone()]), ..message(Role::User, "Legacy batched report", &now) });
            conversation.messages.push(Message { id: answer.clone(), ..message(Role::Assistant, "", &now) });
            conversation.run = RunState { id: run.clone(), status: RunStatus::Running, started_at: Some(now), ..RunState::idle() };
            for delivery in state.disk_mut().bot_deliveries.iter_mut().filter(|delivery| delivery.id == first.id || delivery.id == second.id) {
                delivery.report = Some(DeliveryReport { status: ReportStatus::Running, run_id: Some(run.clone()), ..DeliveryReport::queued() });
            }
            Ok(())
        })
        .await
        .unwrap();
    let old_report = Turn { conversation: initial.conversation.clone(), run, answer };
    let continued = send(&f, &old_report, &bots[1], "Legacy report followup").await.unwrap();
    assert_eq!((continued.chain_id.as_str(), continued.hop), (first.chain_id.as_str(), 2));
    assert_ne!(continued.chain_id, second.chain_id);
    finish(&f, &old_report, "Both relayed", RunStatus::Completed).await;
    roundtrip(&f).await;
}

#[tokio::test]
async fn root_termination_error_is_redacted_in_public_snapshot_and_durable_receipts() {
    let f = Fixture::new().await;
    let key = "private-chain-stop-key";
    f.store
        .mutate(|state| {
            state.update_settings(SettingsInput {
                base_url: "http://127.0.0.1:1234/v1".into(),
                model: "fixture".into(),
                workspace: String::new(),
                api_key: Some(key.into()),
                clear_api_key: None,
                max_concurrent_runs: None,
            })
        })
        .await
        .unwrap();
    let bots = bots(&f, 2).await;
    let a = root(&f, &bots[0]).await;
    let ab = send(&f, &a, &bots[1], "Question").await.unwrap();
    finish(&f, &a, "Sent", RunStatus::Completed).await;
    let ids = HashSet::from([ab.chain_id.clone()]);
    f.store.mutate(|state| state.terminate_bot_chains(&ids, RunStatus::Error, Some(&format!("Failed {key}")), f.now())).await.unwrap();
    let public = serde_json::to_string(&f.store.snapshot()).unwrap();
    assert!(!public.contains(key));
    assert_eq!(delivery(&f, &ab.id).chain_termination.unwrap().error.as_deref(), Some("Failed [redacted]"));
    roundtrip(&f).await;
}
