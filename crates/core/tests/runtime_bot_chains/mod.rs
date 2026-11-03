//! Nested bot-message regressions, run sequentially by `runtime.rs` against the real engine.
//! Dispatch is manual so waiting, queued-report, and running-report boundaries stay observable.

use std::sync::Arc;
use std::time::{Duration, Instant};

use grok_bot_core::queue::CommandQueue;
use grok_bot_core::scheduler::{RoutineScheduler, SchedulerOptions};
use grok_bot_model::{BotDelivery, BotInput, Clock, Conversation, DeliveryStatus, ReportStatus, Role, RunStatus, system_clock};
use grok_bot_store::Store;
use grok_bot_store::bot_messages::BOT_CHAIN_TIMEOUT_ERROR;
use serde_json::json;

use super::{Fixture, Reply, last_text, text, tool_outcomes, use_tool};

fn scheduler(f: &Fixture, now: Clock) -> Arc<RoutineScheduler> {
    RoutineScheduler::new(
        Arc::clone(&f.store),
        Arc::clone(&f.runtime),
        CommandQueue::new(),
        SchedulerOptions { now, poll_interval: Duration::from_secs(3600), auto_start: false },
    )
}

async fn bot(f: &Fixture, name: &str) -> String {
    let input = BotInput { name: name.into(), instructions: None, color: None, shape: None, pinned: None };
    f.store.mutate(|s| s.add_bot(input)).await.unwrap().id
}

fn chain(f: &Fixture, root: &BotDelivery) -> Vec<BotDelivery> {
    f.store.read(|s| s.disk().bot_deliveries.iter().filter(|d| d.chain_id == root.chain_id).cloned().collect())
}

fn delivery(f: &Fixture, item: &BotDelivery) -> BotDelivery {
    f.store.read(|s| s.disk().bot_deliveries.iter().find(|d| d.id == item.id).unwrap().clone())
}

fn exchange_texts(f: &Fixture, item: &BotDelivery) -> Vec<String> {
    f.store.read(|s| s.disk().bot_exchanges.iter().find(|e| e.id == item.exchange_id).unwrap().messages.iter().map(|m| m.text.clone()).collect())
}

/// Do not let an old completed run satisfy `Fixture::completed` immediately after a scheduler tick.
async fn dispatch(f: &Fixture, scheduler: &RoutineScheduler, id: &str, replies: Vec<Reply>) -> Conversation {
    let before = f.get(id);
    f.provider.plan(replies);
    scheduler.tick().await.unwrap();
    f.wait(id, "a newly dispatched turn", |c| c.messages.len() > before.messages.len() && c.run.id != before.run.id).await
}

async fn answer(f: &Fixture, scheduler: &RoutineScheduler, id: &str, reply: &str) -> Conversation {
    dispatch(f, scheduler, id, vec![text(reply)]).await;
    f.completed(id).await
}

async fn held(f: &Fixture, scheduler: &RoutineScheduler, id: &str) {
    let before = f.provider.inference_len();
    dispatch(f, scheduler, id, vec![Reply::Hold]).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    while f.provider.inference_len() == before {
        assert!(Instant::now() < deadline, "held inference never reached the provider");
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    f.wait(id, "held inference", |c| c.run.status == RunStatus::Running).await;
}

async fn root(f: &Fixture, to: &str) -> BotDelivery {
    let source = f.conversation(&f.bot_id()).await;
    f.provider.plan(vec![use_tool("send_message", json!({"toBotId": to, "text": "Review this with your colleague."})), text("A delegated the review.")]);
    f.runtime.start(&source, "Ask for a nested review.").await.unwrap();
    f.decide(&source, "allow_once").await;
    f.completed(&source).await;
    f.store.read(|s| s.disk().bot_deliveries.iter().find(|d| d.source_conversation_id == source).unwrap().clone())
}

async fn child(f: &Fixture, scheduler: &RoutineScheduler, parent: &BotDelivery, to: &str) -> BotDelivery {
    let id = &parent.recipient_conversation_id;
    dispatch(
        f,
        scheduler,
        id,
        vec![use_tool("send_message", json!({"toBotId": to, "text": "Give the final nested verdict."})), text("DELEGATED_ONLY_NOT_FINAL")],
    )
    .await;
    f.decide(id, "allow_once").await;
    f.completed(id).await;
    let parent = delivery(f, parent);
    assert_eq!(parent.status, DeliveryStatus::WaitingChildren);
    assert!(parent.report.is_none(), "an early recipient answer must not be reported");
    assert_eq!(exchange_texts(f, &parent).len(), 1, "the exchange has no final answer yet");
    let descendants = chain(f, &parent);
    descendants.into_iter().find(|d| d.source_conversation_id == *id).unwrap()
}

/// Reopen a checkpoint copy, never the live writer's path. An active checkpoint is expected to
/// recover as interrupted, while waiting/queued/final checkpoints must preserve their work.
async fn reopened(f: &Fixture, label: &str) -> Arc<Store> {
    f.store.flush().await.unwrap();
    let copy = f.root.join(format!("checkpoint-{label}"));
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::copy(f.root.join("data/state.json"), copy.join("state.json")).unwrap();
    Store::open(&copy, system_clock()).await.unwrap()
}

fn no_pending(items: &[BotDelivery]) {
    assert!(items.iter().all(|d| d.status.is_terminal() && d.report.as_ref().is_none_or(|r| r.status.is_terminal())), "{items:?}");
}

async fn no_replay(f: &Fixture, scheduler: &RoutineScheduler, root: &BotDelivery, label: &str) {
    let before = f.public().conversations;
    let requests = f.provider.inference_len();
    scheduler.tick().await.unwrap();
    assert_eq!(f.public().conversations, before, "terminal chains must leave no dispatchable work");
    assert_eq!(f.provider.inference_len(), requests);
    let copy = reopened(f, label).await;
    copy.read(|s| {
        let items: Vec<_> = s.disk().bot_deliveries.iter().filter(|d| d.chain_id == root.chain_id).cloned().collect();
        no_pending(&items);
        assert_eq!(items, chain(f, root));
    });
}

pub(super) async fn nested_bot_reports(f: &Fixture) {
    let scheduler = scheduler(f, system_clock());
    let (b, c) = (bot(f, "B").await, bot(f, "C").await);
    let root = root(f, &b).await;
    let leaf = child(f, &scheduler, &root, &c).await;
    assert!(f.store.read(|s| s.active().is_none()), "waiting parents release the engine slot");
    let waiting = reopened(f, "waiting").await;
    assert_eq!(waiting.read(|s| s.disk().bot_deliveries.clone()), chain(f, &root));

    answer(f, &scheduler, &leaf.recipient_conversation_id, "C_FINAL_CANARY: approved.").await;
    assert_eq!(delivery(f, &leaf).report.unwrap().status, ReportStatus::Queued);
    assert_eq!(delivery(f, &root).status, DeliveryStatus::WaitingChildren);
    assert!(delivery(f, &root).report.is_none());
    assert_eq!(exchange_texts(f, &root).len(), 1);
    let queued = reopened(f, "queued-report").await;
    assert_eq!(queued.read(|s| s.disk().bot_deliveries.clone()), chain(f, &root));

    // Keep B's report at an approval boundary while its new execution id is persisted.
    let id = &root.recipient_conversation_id;
    let first_run = f.get(id).run.id;
    dispatch(
        f,
        &scheduler,
        id,
        vec![
            use_tool("send_message", json!({"toBotId": c, "text": "An extra question that will be denied."})),
            text("B_SUMMARY_CANARY: C approved the review."),
        ],
    )
    .await;
    f.pending(id).await;
    let running = f.get(id);
    assert_ne!(running.run.id, first_run);
    assert_eq!(delivery(f, &root).execution_run_id.as_deref(), Some(running.run.id.as_str()));
    assert_eq!(running.messages[2].report_delivery_ids.as_deref(), Some(&[leaf.id.clone()][..]));
    let interrupted = reopened(f, "running-report").await;
    interrupted.read(|s| {
        assert_eq!(s.conversation(id).unwrap().run.status, RunStatus::Interrupted);
        assert_eq!(s.disk().bot_deliveries.iter().find(|d| d.id == leaf.id).unwrap().report.as_ref().unwrap().status, ReportStatus::Interrupted);
        assert_ne!(s.disk().bot_deliveries.iter().find(|d| d.id == root.id).unwrap().status, DeliveryStatus::Completed);
    });
    f.decide(id, "reject_once").await;
    let b_done = f.completed(id).await;
    assert_eq!(last_text(&b_done), "B_SUMMARY_CANARY: C approved the review.");
    assert_eq!(delivery(f, &root).status, DeliveryStatus::Completed);
    assert_eq!(exchange_texts(f, &root), [root.text.clone(), last_text(&b_done)]);

    let a_done = answer(f, &scheduler, &root.source_conversation_id, "A_SUMMARY_CANARY: B and C approved.").await;
    assert_eq!(a_done.messages.iter().map(|m| m.role).collect::<Vec<_>>(), [Role::User, Role::Assistant, Role::User, Role::Assistant]);
    assert_eq!(a_done.messages[2].report_delivery_ids.as_deref(), Some(&[root.id.clone()][..]));
    assert!(a_done.messages[2].text.contains("B_SUMMARY_CANARY"));
    assert!(!a_done.messages[2].text.contains("DELEGATED_ONLY_NOT_FINAL"));
    assert_eq!(delivery(f, &root).report.unwrap().status, ReportStatus::Completed);
    no_replay(f, &scheduler, &root, "final").await;

    // A waiting logical chain does not reserve its source conversation from a new user turn.
    // Issue #20 also dispatches eligible C when A starts; hold C first so both Bot slots are explicit
    // and the shared scripted provider cannot accidentally answer the wrong conversation.
    let another = self::root(f, &b).await;
    let nested = child(f, &scheduler, &another, &c).await;
    held(f, &scheduler, &nested.recipient_conversation_id).await;
    let c_run = f.get(&nested.recipient_conversation_id).run.clone();
    f.provider.plan(vec![text("An independent answer while C is active.")]);
    f.runtime.start(&another.source_conversation_id, "A separate user question.").await.unwrap();
    f.wait(&another.source_conversation_id, "the independent user turn", |conversation| conversation.messages.len() == 4).await;
    let independent = f.completed(&another.source_conversation_id).await;
    assert_eq!(last_text(&independent), "An independent answer while C is active.");
    assert_eq!(f.get(&nested.recipient_conversation_id).run, c_run, "the other Bot's live turn remains independent");
    assert_eq!(delivery(f, &another).status, DeliveryStatus::WaitingChildren);
    assert!(delivery(f, &another).chain_termination.is_none());
    f.provider.release.notify_one();
    f.completed(&nested.recipient_conversation_id).await;
    answer(f, &scheduler, &another.recipient_conversation_id, "B includes the late C answer.").await;
    let final_a = answer(f, &scheduler, &another.source_conversation_id, "A reports the late result once.").await;
    assert_eq!(final_a.messages.len(), 6);
    assert_eq!(final_a.messages[2].text, "A separate user question.");
    assert_eq!(final_a.messages[4].report_delivery_ids.as_deref(), Some(&[another.id.clone()][..]));
    no_replay(f, &scheduler, &another, "independent-user-final").await;
    scheduler.close().await;
}

pub(super) async fn stop_nested_bot_chains(f: &Fixture) {
    let scheduler = scheduler(f, system_clock());
    let (b, c) = (bot(f, "B").await, bot(f, "C").await);
    for boundary in ["queued-leaf", "leaf-approval", "leaf-inference", "recipient-report", "root-report", "unrelated-user"] {
        let root = root(f, &b).await;
        let leaf = child(f, &scheduler, &root, &c).await;
        let mut active = None;
        let mut stale = None;
        // Start this user before A's report becomes ready: canonical Bot FIFO must not be bypassed.
        let unrelated = if boundary == "unrelated-user" {
            let id = f.conversation(&f.bot_id()).await;
            // Pause C at an approval first so accepting this user cannot consume C's reply script.
            dispatch(
                f,
                &scheduler,
                &leaf.recipient_conversation_id,
                vec![use_tool("send_message", json!({"toBotId": b, "text": "This extra message will be denied."}))],
            )
            .await;
            f.pending(&leaf.recipient_conversation_id).await;
            let before = f.provider.inference_len();
            f.provider.plan(vec![Reply::Hold]);
            f.runtime.start(&id, "An independent user question.").await.unwrap();
            f.wait(&id, "independent inference", |c| c.run.status == RunStatus::Running && f.provider.inference_len() > before).await;
            let unrelated = Some((id.clone(), f.get(&id).run.id));
            f.provider.plan(vec![text("C answered while the independent user is active.")]);
            f.decide(&leaf.recipient_conversation_id, "reject_once").await;
            f.completed(&leaf.recipient_conversation_id).await;
            unrelated
        } else {
            None
        };
        match boundary {
            "leaf-approval" => {
                dispatch(
                    f,
                    &scheduler,
                    &leaf.recipient_conversation_id,
                    vec![use_tool("send_message", json!({"toBotId": b, "text": "An unapproved follow-up."}))],
                )
                .await;
                stale = f.pending(&leaf.recipient_conversation_id).await.run.permission;
                active = Some(leaf.recipient_conversation_id.clone());
            }
            "leaf-inference" => {
                held(f, &scheduler, &leaf.recipient_conversation_id).await;
                active = Some(leaf.recipient_conversation_id.clone());
            }
            "recipient-report" | "root-report" | "unrelated-user" => {
                if boundary != "unrelated-user" {
                    answer(f, &scheduler, &leaf.recipient_conversation_id, "C answered.").await;
                }
                if boundary == "recipient-report" {
                    held(f, &scheduler, &root.recipient_conversation_id).await;
                    active = Some(root.recipient_conversation_id.clone());
                } else {
                    answer(f, &scheduler, &root.recipient_conversation_id, "B summarized C.").await;
                    assert_eq!(delivery(f, &root).report.unwrap().status, ReportStatus::Queued);
                }
            }
            _ => {}
        }
        let source_before = f.get(&root.source_conversation_id);
        let descendants_before = [f.get(&root.recipient_conversation_id), f.get(&leaf.recipient_conversation_id)];
        let leaf_before = delivery(f, &leaf);
        let root_before = delivery(f, &root);
        let queued_descendants: Vec<_> = f
            .public()
            .work_queue
            .into_iter()
            .filter(|work| work.conversation_id == root.recipient_conversation_id || work.conversation_id == leaf.recipient_conversation_id)
            .collect();
        tokio::time::timeout(Duration::from_secs(5), f.runtime.cancel(&root.source_conversation_id))
            .await
            .expect("local Stop must not wait for another Bot's descendant")
            .unwrap();
        // Revised issue #20 scope: retain all six former whole-chain Stop boundaries, but only
        // queued work in A is removed. B/C's queued, approval, inference, and report turns survive.
        assert_eq!(f.get(&root.source_conversation_id), source_before, "an idle/queued-report Stop creates no run or hidden input: {boundary}");
        for before in descendants_before {
            assert_eq!(f.get(&before.id), before, "root Stop must leave the other Bot conversation unchanged: {boundary}");
        }
        assert_eq!(delivery(f, &leaf), leaf_before, "root Stop must not change a descendant receipt: {boundary}");
        assert_eq!(delivery(f, &root).status, root_before.status, "root Stop cannot terminate another Bot's aggregate: {boundary}");
        assert!(delivery(f, &root).chain_termination.is_none(), "local Stop never latches a whole-chain cancellation: {boundary}");
        assert!(!f.public().work_queue.iter().any(|work| work.conversation_id == root.source_conversation_id));
        for queued in queued_descendants {
            assert!(f.public().work_queue.contains(&queued), "other Bot queued work remains dispatchable: {boundary}");
        }
        if matches!(boundary, "root-report" | "unrelated-user") {
            assert_eq!(delivery(f, &root).report.unwrap().status, ReportStatus::Cancelled, "Stop removes A's selected queued report");
        }
        if let Some(id) = active {
            assert!(f.get(&id).run.status.is_active(), "the descendant remains live until its own Stop: {boundary}");
            f.runtime.cancel(&id).await.unwrap();
            assert_eq!(f.settled(&id).await.run.status, RunStatus::Cancelled, "local descendant cleanup: {boundary}");
            if let Some(permission) = stale {
                assert_eq!(f.runtime.resolve(&id, &permission.id, &permission.options[0].id, false).await.unwrap_err().status, 409);
            }
        }
        if let Some((id, run_id)) = unrelated {
            assert_eq!((f.get(&id).run.id, f.get(&id).run.status), (run_id, RunStatus::Running), "root Stop must not cancel another user's same-Bot turn");
            f.provider.release.notify_waiters();
            f.completed(&id).await;
        } else {
            f.provider.release.notify_waiters();
        }
        // Clean each conversation up through its own Stop, including any newly arrived upstream
        // report. The no-replay and write-safety checks still cover the former broad cases.
        for id in [&leaf.recipient_conversation_id, &root.recipient_conversation_id, &root.source_conversation_id] {
            f.runtime.cancel(id).await.unwrap();
        }
        assert!(delivery(f, &root).chain_termination.is_none());
        no_pending(&chain(f, &root));
        assert_eq!(f.notes(), "original\n");
        no_replay(f, &scheduler, &root, boundary).await;
    }

    // Rule 7 also applies when Stop targets an intermediate Bot's live turn after it has already
    // delegated: neither queued nor running C is B's cancellation scope, despite the shared chain.
    for boundary in ["queued-child", "running-child"] {
        let root = root(f, &b).await;
        let requests = f.provider.inference_len();
        dispatch(
            f,
            &scheduler,
            &root.recipient_conversation_id,
            vec![use_tool("send_message", json!({"toBotId": c, "text": "Finish independently if B is stopped."})), Reply::Hold],
        )
        .await;
        f.decide(&root.recipient_conversation_id, "allow_once").await;
        f.wait(&root.recipient_conversation_id, "B's held turn after its approved delegation", |conversation| {
            conversation.run.status == RunStatus::Running && f.provider.inference_len() >= requests + 2
        })
        .await;
        let leaf = chain(f, &root).into_iter().find(|item| item.source_conversation_id == root.recipient_conversation_id).unwrap();
        if boundary == "running-child" {
            held(f, &scheduler, &leaf.recipient_conversation_id).await;
        }
        let descendant = f.get(&leaf.recipient_conversation_id);
        let receipt = delivery(f, &leaf);
        let queued: Vec<_> = f.public().work_queue.into_iter().filter(|work| work.conversation_id == leaf.recipient_conversation_id).collect();
        tokio::time::timeout(Duration::from_secs(5), f.runtime.cancel(&root.recipient_conversation_id)).await.expect("B's Stop must not wait for C").unwrap();
        assert_eq!(f.get(&root.recipient_conversation_id).run.status, RunStatus::Cancelled);
        assert_eq!(f.get(&leaf.recipient_conversation_id), descendant, "stopping B leaves C independent: {boundary}");
        assert_eq!(delivery(f, &leaf), receipt, "stopping B cannot cancel C's delivery: {boundary}");
        assert_eq!(f.public().work_queue.into_iter().filter(|work| work.conversation_id == leaf.recipient_conversation_id).collect::<Vec<_>>(), queued);
        assert!(delivery(f, &root).chain_termination.is_none());
        f.runtime.cancel(&leaf.recipient_conversation_id).await.unwrap();
        assert_eq!(f.get(&leaf.recipient_conversation_id).run.status, RunStatus::Cancelled, "C stops only through its own Stop");
        f.provider.release.notify_waiters();
        f.runtime.cancel(&root.recipient_conversation_id).await.unwrap();
        f.runtime.cancel(&root.source_conversation_id).await.unwrap();
        no_pending(&chain(f, &root));
        assert_eq!(f.notes(), "original\n");
        no_replay(f, &scheduler, &root, boundary).await;
    }
    scheduler.close().await;
}

pub(super) async fn expire_nested_bot_chains(f: &Fixture) {
    let scheduler = scheduler(f, system_clock());
    let (b, c) = (bot(f, "B").await, bot(f, "C").await);
    for boundary in ["queued", "inference", "approval", "report-only"] {
        let root = root(f, &b).await;
        let active = if boundary == "report-only" {
            answer(f, &scheduler, &root.recipient_conversation_id, "Only a queued report remains.").await;
            assert_eq!(delivery(f, &root).status, DeliveryStatus::Completed);
            assert_eq!(delivery(f, &root).report.unwrap().status, ReportStatus::Queued);
            None
        } else {
            let leaf = child(f, &scheduler, &root, &c).await;
            match boundary {
                "inference" => held(f, &scheduler, &leaf.recipient_conversation_id).await,
                "approval" => {
                    dispatch(
                        f,
                        &scheduler,
                        &leaf.recipient_conversation_id,
                        vec![use_tool("send_message", json!({"toBotId": b, "text": "This approval will expire."}))],
                    )
                    .await;
                    f.pending(&leaf.recipient_conversation_id).await;
                }
                _ => {}
            }
            (boundary != "queued").then_some(leaf.recipient_conversation_id)
        };
        let lineage: Vec<_> = chain(f, &root).iter().map(|d| (d.hop, d.created_at.clone())).collect();
        let deadline = chrono::DateTime::parse_from_rfc3339(&root.created_at).unwrap().with_timezone(&chrono::Utc) + chrono::Duration::minutes(31);
        if boundary == "report-only" {
            f.runtime.expire_bot_chains(deadline).await.unwrap();
        } else {
            let future = self::scheduler(f, Arc::new(move || deadline));
            tokio::time::timeout(Duration::from_secs(5), future.tick())
                .await
                .expect("a scheduler tick must not wait for engine shutdown inside its queue")
                .unwrap();
            future.close().await;
        }
        if boundary == "queued" {
            // Reproduce work selected before expiration but admitted after it, without racing a clock.
            let leaf = chain(f, &root).into_iter().find(|d| d.hop == 2).unwrap();
            let requests = f.provider.inference_len();
            let expired_run = f.get(&leaf.recipient_conversation_id).run;
            assert!(f.get(&leaf.recipient_conversation_id).messages.is_empty());
            let rejected = f.runtime.start(&leaf.recipient_conversation_id, &leaf.text).await.unwrap_err();
            assert_eq!(rejected.status, 409, "an expired reserved recipient cannot become an ordinary prompt");
            assert!(f.get(&leaf.recipient_conversation_id).messages.is_empty(), "stale dispatch must not append messages");
            assert_eq!(f.provider.inference_len(), requests, "stale dispatch must not reach inference");
            // The scheduler's stale failure handler must not overwrite the timeout receipt with its
            // earlier tick time. The no_replay checkpoint below also validates the chain on reopen.
            let old_now = deadline - chrono::Duration::minutes(31);
            f.store.mutate(|s| s.fail_bot_delivery(&leaf.id, "stale selected work", old_now)).await.unwrap();
            assert_eq!(f.get(&leaf.recipient_conversation_id).run, expired_run, "stale failure handling must preserve the expired run");
        }
        if let Some(id) = active {
            // An immediate Stop after timeout must retain the first canonical failure cause.
            f.runtime.cancel(&root.source_conversation_id).await.unwrap();
            let done = f.settled(&id).await;
            assert_eq!(done.run.status, RunStatus::Error, "{boundary}: {:?}", done.run);
            assert_eq!(done.run.error.as_deref(), Some(BOT_CHAIN_TIMEOUT_ERROR));
        }
        f.provider.release.notify_waiters();
        assert_eq!(delivery(f, &root).chain_termination.unwrap().status, RunStatus::Error);
        assert_eq!(
            chain(f, &root).iter().map(|d| (d.hop, d.created_at.clone())).collect::<Vec<_>>(),
            lineage,
            "timeouts never reset a per-hop clock or lineage"
        );
        no_pending(&chain(f, &root));
        no_replay(f, &scheduler, &root, boundary).await;
    }
    // Update maintenance preserves even already-expired durable work until it resumes.
    let paused = self::root(f, &b).await;
    f.wait(&paused.source_conversation_id, "maintenance admission after the origin settles", |_| f.runtime.pause_if_idle().is_ok()).await;
    let before = f.public();
    let deadline = chrono::DateTime::parse_from_rfc3339(&paused.created_at).unwrap().with_timezone(&chrono::Utc) + chrono::Duration::minutes(31);
    f.runtime.expire_bot_chains(deadline).await.unwrap();
    assert_eq!(f.runtime.start(&paused.recipient_conversation_id, &paused.text).await.unwrap_err().status, 503);
    assert_eq!(f.public(), before, "maintenance must reject admission without changing queued chains");
    f.runtime.resume().unwrap();
    f.runtime.expire_bot_chains(deadline).await.unwrap();
    no_replay(f, &scheduler, &paused, "maintenance-expired").await;
    scheduler.close().await;
}

pub(super) async fn four_hop_bot_reports(f: &Fixture) {
    let scheduler = scheduler(f, system_clock());
    let mut bots = Vec::new();
    for name in ["B", "C", "D", "E"] {
        bots.push(bot(f, name).await);
    }
    let root = root(f, &bots[0]).await;
    let mut path = vec![root.clone()];
    for to in &bots[1..] {
        path.push(child(f, &scheduler, path.last().unwrap(), to).await);
    }
    let leaf = path.last().unwrap();
    let before = f.provider.inference_len();
    dispatch(
        f,
        &scheduler,
        &leaf.recipient_conversation_id,
        vec![use_tool("send_message", json!({"toBotId": f.bot_id(), "text": "Hop five must be denied."})), text("E final verdict.")],
    )
    .await;
    f.completed(&leaf.recipient_conversation_id).await;
    assert!(tool_outcomes(&f.provider.inference()[before..]).iter().any(|t| t.contains("limited to four hops")));
    assert_eq!(chain(f, &root).iter().map(|d| d.hop).collect::<Vec<_>>(), [1, 2, 3, 4]);
    for item in path.iter().rev() {
        let done = answer(f, &scheduler, &item.source_conversation_id, &format!("Summary of hop {}.", item.hop)).await;
        assert_eq!(done.messages[2].report_delivery_ids.as_deref(), Some(&[item.id.clone()][..]));
        assert_eq!(delivery(f, item).report.unwrap().status, ReportStatus::Completed, "internal reports do not consume the root's two-report budget");
    }
    assert_eq!(chain(f, &root).iter().filter(|d| d.report.as_ref().is_some_and(|r| r.status == ReportStatus::Completed)).count(), 4);
    no_replay(f, &scheduler, &root, "four-hop-final").await;
    scheduler.close().await;
}

pub(super) async fn nested_report_start_failure(f: &Fixture) {
    let scheduler = scheduler(f, system_clock());
    let (b, c) = (bot(f, "B").await, bot(f, "C").await);
    let root = root(f, &b).await;
    let leaf = child(f, &scheduler, &root, &c).await;
    answer(f, &scheduler, &leaf.recipient_conversation_id, "C's answer cannot be summarized by B.").await;
    f.runtime.set_origin(String::new());
    scheduler.tick().await.unwrap();
    f.runtime.set_origin(f.origin.clone());
    let failed = delivery(f, &root);
    assert_eq!(failed.status, DeliveryStatus::Error);
    assert!(failed.error.unwrap().contains("Runtime is not connected"));
    assert_eq!(delivery(f, &leaf).report.unwrap().status, ReportStatus::Error);
    assert_eq!(f.get(&root.recipient_conversation_id).messages.len(), 2, "a failed start adds no hidden input or stale answer");
    assert!(!exchange_texts(f, &root).iter().any(|text| text.contains("DELEGATED_ONLY_NOT_FINAL")));
    let done = answer(f, &scheduler, &root.source_conversation_id, "B could not finish the nested review.").await;
    assert!(done.messages[2].text.contains(r#""status":"error""#));
    assert!(done.messages[2].text.contains("Runtime is not connected"));
    assert!(!done.messages[2].text.contains("DELEGATED_ONLY_NOT_FINAL"));
    no_replay(f, &scheduler, &root, "failed-start").await;

    // PR24's queued-report cancel also closes a waiting recipient without publishing its
    // provisional delegation message as a successful answer to the original bot.
    let cancelled = self::root(f, &b).await;
    let child_delivery = child(f, &scheduler, &cancelled, &c).await;
    answer(f, &scheduler, &child_delivery.recipient_conversation_id, "C final that B will not consume.").await;
    let selected = [child_delivery.id.clone()];
    let cancelled_ids = f.store.mutate(|state| state.cancel_bot_reports(&cancelled.recipient_conversation_id, &selected)).await.unwrap();
    assert_eq!(cancelled_ids, selected);
    assert_eq!(delivery(f, &cancelled).status, DeliveryStatus::Cancelled);
    assert_eq!(delivery(f, &child_delivery).report.unwrap().status, ReportStatus::Cancelled);
    assert_eq!(exchange_texts(f, &cancelled).as_slice(), std::slice::from_ref(&cancelled.text));
    assert!(delivery(f, &cancelled).chain_termination.is_none(), "cancel only the queued report, not the entire root chain");
    let reported = answer(f, &scheduler, &cancelled.source_conversation_id, "B's nested summary was cancelled.").await;
    assert!(reported.messages[2].text.contains(r#""status":"cancelled""#));
    assert!(!reported.messages[2].text.contains("DELEGATED_ONLY_NOT_FINAL"));
    no_replay(f, &scheduler, &cancelled, "cancelled-nested-report").await;
    scheduler.close().await;
}
