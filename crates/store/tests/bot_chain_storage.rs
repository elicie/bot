//! Focused PR27 storage upgrade, capacity, and admission regressions.
mod common;

use std::collections::HashSet;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use common::Fixture;
use grok_bot_model::*;
use grok_bot_store::bot_messages::{BOT_CHAIN_CAPACITY_ERROR, MAX_BOT_DELIVERIES_PER_CHAIN, MAX_BOT_REPLY_CHARACTERS, MAX_BOT_TURNS_PER_CHAIN, queued_reports};
use grok_bot_store::{MAX_STATE_BYTES, PRE_BOT_CHAINS_BACKUP, PRE_REPORTS_BACKUP};
use serde_json::json;

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
        attachments: None,
        speaker_bot_id: None,
        report_delivery_ids: None,
    }
}

async fn bots(f: &Fixture, count: usize) -> Vec<String> {
    let mut bots = vec![f.bot_id()];
    for index in 1..count {
        bots.push(
            f.store
                .mutate(|s| s.add_bot(BotInput { name: format!("Bot {index}"), color: None, instructions: None, shape: None, pinned: None }))
                .await
                .unwrap()
                .id,
        );
    }
    bots
}

async fn start(f: &Fixture, conversation: &str, delivery: Option<&BotDelivery>, text: &str) -> Turn {
    let (run, answer, now) = (new_id(), new_id(), f.iso_now());
    f.store
        .mutate(|s| {
            let conversation = s.conversation_mut(conversation)?;
            if let Some(delivery) = delivery {
                conversation.messages.push(Message { source_bot_id: Some(delivery.from_bot_id.clone()), ..message(Role::User, &delivery.text, &now) });
            }
            conversation.messages.push(Message { id: answer.clone(), ..message(Role::Assistant, text, &now) });
            conversation.run = RunState { id: run.clone(), status: RunStatus::Running, started_at: Some(now), ..RunState::idle() };
            Ok(())
        })
        .await
        .unwrap();
    Turn { conversation: conversation.into(), run, answer }
}

async fn root(f: &Fixture, bot: &str) -> Turn {
    let conversation = f.store.mutate(|s| s.add_conversation(bot)).await.unwrap();
    start(f, &conversation.id, None, "Ready").await
}

async fn send(f: &Fixture, turn: &Turn, bot: &str, text: &str) -> HttpResult<BotDelivery> {
    f.store
        .mutate(|s| {
            s.queue_bot_message(&turn.conversation, &turn.run, &turn.answer, BotMessageInput { to_bot_id: bot.into(), text: text.into(), request_id: None })
        })
        .await
}

async fn report(f: &Fixture, conversation_id: &str, prompt: &str) -> Turn {
    let (run, answer, now) = (new_id(), new_id(), f.iso_now());
    f.store
        .mutate(|s| {
            let queued = queued_reports(&s.disk().bot_deliveries, conversation_id);
            let chain = queued.first().unwrap().chain_id.clone();
            let ids: Vec<_> = queued.iter().filter(|d| d.chain_id == chain).map(|d| d.id.clone()).collect();
            s.check_bot_report_capacity(conversation_id, prompt, &ids)?;
            let conversation = s.conversation_mut(conversation_id)?;
            conversation.messages.push(Message { report_delivery_ids: Some(ids.clone()), ..message(Role::User, prompt, &now) });
            conversation.messages.push(Message { id: answer.clone(), ..message(Role::Assistant, "", &now) });
            conversation.run = RunState { id: run.clone(), status: RunStatus::Running, started_at: Some(now), ..RunState::idle() };
            for delivery in s.disk_mut().bot_deliveries.iter_mut().filter(|d| ids.contains(&d.id)) {
                delivery.report = Some(DeliveryReport { status: ReportStatus::Running, run_id: Some(run.clone()), ..DeliveryReport::queued() });
            }
            Ok(())
        })
        .await
        .unwrap();
    Turn { conversation: conversation_id.into(), run, answer }
}

async fn finish(f: &Fixture, turn: &Turn, text: &str) {
    f.store
        .mutate(|s| {
            let conversation = s.conversation_mut(&turn.conversation)?;
            conversation.messages.iter_mut().find(|m| m.id == turn.answer).unwrap().text = text.into();
            conversation.run.status = RunStatus::Completed;
            conversation.run.finished_at = Some(f.iso_now());
            Ok(())
        })
        .await
        .unwrap();
}

async fn old_report(f: &Fixture, bots: &[String]) {
    let origin = root(f, &bots[0]).await;
    let delivery = send(f, &origin, &bots[1], "An already reported PR9 request").await.unwrap();
    finish(f, &origin, "Sent").await;
    let recipient = start(f, &delivery.recipient_conversation_id, Some(&delivery), "Old answer").await;
    finish(f, &recipient, "Old answer").await;
    let summary = report(f, &origin.conversation, "PR9 report input").await;
    finish(f, &summary, "Old report summary").await;
    assert!(f.disk_text().contains("reportDeliveryIds"));
    assert!(f.data_dir.join(PRE_REPORTS_BACKUP).exists());
    assert!(!f.data_dir.join(PRE_BOT_CHAINS_BACKUP).exists());
}

fn assert_backup(f: &Fixture, expected: &str, legacy: &str) -> u64 {
    let path = f.data_dir.join(PRE_BOT_CHAINS_BACKUP);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
    let metadata = std::fs::symlink_metadata(path).unwrap();
    assert!(metadata.is_file());
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(metadata.nlink(), 1);
    assert_eq!(std::fs::read_to_string(f.data_dir.join(PRE_REPORTS_BACKUP)).unwrap(), legacy);
    metadata.ino()
}

#[tokio::test]
async fn reports_upgrade_backs_up_the_immediate_previous_disk_before_first_waiting_children() {
    let f = Fixture::new().await;
    let bots = bots(&f, 3).await;
    old_report(&f, &bots).await;
    let legacy = std::fs::read_to_string(f.data_dir.join(PRE_REPORTS_BACKUP)).unwrap();
    let f = Fixture { store: f.reopen().await.unwrap(), ..f };
    f.store
        .mutate(|s| {
            s.set_runtime(RuntimeInfo { available: true, bot_message_chains: Some(true), version: None, error: None });
            Ok(())
        })
        .await
        .unwrap();
    assert!(!f.disk_text().contains("botMessageChains"), "runtime capability is not persisted or a migration trigger");
    assert!(!f.data_dir.join(PRE_BOT_CHAINS_BACKUP).exists());
    let origin = root(&f, &bots[0]).await;
    let parent = send(&f, &origin, &bots[1], "New parent").await.unwrap();
    finish(&f, &origin, "Sent").await;
    let recipient = start(&f, &parent.recipient_conversation_id, Some(&parent), "Initial answer").await;
    let child = send(&f, &recipient, &bots[2], "New child").await.unwrap();
    let before = f.disk_text();
    assert!(!before.contains("waiting_children"));
    finish(&f, &recipient, "Waiting for child").await;
    let inode = assert_backup(&f, &before, &legacy);
    assert_eq!(f.store.read(|s| s.disk().bot_deliveries.iter().find(|d| d.id == parent.id).unwrap().status), DeliveryStatus::WaitingChildren);
    let child_turn = start(&f, &child.recipient_conversation_id, Some(&child), "Child answer").await;
    finish(&f, &child_turn, "Child answer").await;
    let summary = report(&f, &recipient.conversation, "New internal report").await;
    finish(&f, &summary, "Final parent summary").await;
    assert_eq!(assert_backup(&f, &before, &legacy), inode, "later reports never replace the rollback point");
    let reopened = f.reopen().await.unwrap();
    reopened.flush().await.unwrap();
    assert_eq!(assert_backup(&f, &before, &legacy), inode, "reopen does not replace the rollback point");
}

#[tokio::test]
async fn reports_upgrade_also_backs_up_before_the_first_chain_termination() {
    let f = Fixture::new().await;
    let bots = bots(&f, 2).await;
    old_report(&f, &bots).await;
    let legacy = std::fs::read_to_string(f.data_dir.join(PRE_REPORTS_BACKUP)).unwrap();
    let origin = root(&f, &bots[0]).await;
    let delivery = send(&f, &origin, &bots[1], "Stop before dispatch").await.unwrap();
    finish(&f, &origin, "Sent").await;
    let before = f.disk_text();
    let ids = HashSet::from([delivery.chain_id.clone()]);
    f.store.mutate(|s| s.terminate_bot_chains(&ids, RunStatus::Cancelled, None, f.now())).await.unwrap();
    let inode = assert_backup(&f, &before, &legacy);
    assert!(f.disk_text().contains("chainTermination"));
    f.store.mutate(|s| s.terminate_bot_chains(&ids, RunStatus::Error, Some("Do not rewrite history"), f.now())).await.unwrap();
    assert_eq!(assert_backup(&f, &before, &legacy), inode);
    f.reopen().await.unwrap().flush().await.unwrap();
    assert_eq!(assert_backup(&f, &before, &legacy), inode);
}

#[tokio::test]
async fn an_existing_chain_backup_is_never_overwritten_and_symlinks_fail_closed() {
    for symlink in [false, true] {
        let f = Fixture::new().await;
        let bots = bots(&f, 2).await;
        let history = f.disk_text();
        let backup = f.data_dir.join(PRE_BOT_CHAINS_BACKUP);
        let target = f.root.path().join("old-history.json");
        std::fs::write(&target, &history).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        if symlink {
            std::os::unix::fs::symlink(&target, &backup).unwrap();
        } else {
            std::fs::write(&backup, &history).unwrap();
            std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let origin = root(&f, &bots[0]).await;
        let delivery = send(&f, &origin, &bots[1], "Stop").await.unwrap();
        finish(&f, &origin, "Sent").await;
        let before = f.disk_text();
        let ids = HashSet::from([delivery.chain_id]);
        let result = f.store.mutate(|s| s.terminate_bot_chains(&ids, RunStatus::Cancelled, None, f.now())).await;
        if symlink {
            assert_eq!(result.unwrap_err().status, 500);
            assert_eq!(f.disk_text(), before, "failed backup must leave the old-readable disk untouched");
            assert_eq!(f.store.read(|s| s.assert_writable()).unwrap_err().status, 503);
            assert!(std::fs::symlink_metadata(&backup).unwrap().file_type().is_symlink());
        } else {
            result.unwrap();
            f.reopen().await.unwrap().flush().await.unwrap();
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), history);
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), history);
    }
}

#[tokio::test]
async fn a_successful_backup_does_not_mask_a_failed_state_write() {
    let f = Fixture::new().await;
    let bots = bots(&f, 2).await;
    let origin = root(&f, &bots[0]).await;
    let delivery = send(&f, &origin, &bots[1], "Stop").await.unwrap();
    finish(&f, &origin, "Sent").await;
    // A pre-existing valid backup allows the actual state writer to reach its own failure.
    let backup = f.data_dir.join(PRE_BOT_CHAINS_BACKUP);
    let history = f.disk_text();
    std::fs::write(&backup, &history).unwrap();
    std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::remove_file(f.state_file()).unwrap();
    std::fs::create_dir(f.state_file()).unwrap();
    let ids = HashSet::from([delivery.chain_id]);
    let error = f.store.mutate(|s| s.terminate_bot_chains(&ids, RunStatus::Cancelled, None, f.now())).await.unwrap_err();
    assert_eq!(error.status, 500);
    assert_eq!(std::fs::read_to_string(backup).unwrap(), history);
    assert_eq!(f.store.read(|s| s.assert_writable()).unwrap_err().status, 503);
}

#[tokio::test]
async fn fresh_four_hop_maximum_text_chain_and_every_reverse_report_fit_below_64_mib() {
    let f = Fixture::new().await;
    let bots = bots(&f, 5).await;
    let answer = "\0".repeat(MAX_BOT_REPLY_CHARACTERS);
    let request = "\0".repeat(100_000);
    assert_eq!(serde_json::to_vec(&answer).unwrap().len(), 6 * MAX_BOT_REPLY_CHARACTERS + 2);
    assert_eq!("😀".repeat(100_000).encode_utf16().count(), MAX_BOT_REPLY_CHARACTERS);
    let origin = root(&f, &bots[0]).await;
    let mut turn = origin.clone();
    let mut deliveries = Vec::new();
    for (index, bot) in bots[1..].iter().enumerate() {
        let delivery = send(&f, &turn, bot, &request).await.unwrap();
        finish(&f, &turn, &answer).await;
        turn = start(&f, &delivery.recipient_conversation_id, Some(&delivery), &answer).await;
        assert_eq!(delivery.hop, index as u32 + 1);
        deliveries.push(delivery);
    }
    // The review's fresh-four-request 413 claim does not reproduce even with the OLD reserve.
    let current = f.disk_text().len();
    let old_reserve: usize = f.store.read(|s| {
        s.disk()
            .bot_deliveries
            .iter()
            .map(|d| {
                (if d.status == DeliveryStatus::Queued { serde_json::to_vec(&d.text).unwrap().len() } else { 0 })
                    + MAX_BOT_REPLY_CHARACTERS * 24
                    + 128 * 1024
                    + 8192
            })
            .sum()
    });
    assert!(current + old_reserve < MAX_STATE_BYTES, "four-hop current={current}, old reserve={old_reserve}");
    finish(&f, &turn, &answer).await;
    // Quoted raw controls expand once into report JSON and once again into state.json.
    let prompt = json!({ "botReplies": [{ "yourMessage": "\0".repeat(8 * 1024), "answer": "\0".repeat(48 * 1024) }] }).to_string();
    let prompt_bytes = serde_json::to_vec(&prompt).unwrap().len();
    assert!(prompt_bytes > 7 * 56 * 1024);
    for delivery in deliveries.iter().rev() {
        let summary = report(&f, &delivery.source_conversation_id, &prompt).await;
        finish(&f, &summary, &answer).await;
        f.store.read(|s| s.assert_writable()).unwrap();
        assert!(f.disk_text().len() < MAX_STATE_BYTES);
    }
    let final_bytes = f.disk_text().len();
    eprintln!(
        "four-hop worst-JSON text: before final recipient={current}; old reservation={old_reserve}; quoted prompt={prompt_bytes}; final={final_bytes} bytes"
    );
    f.reopen().await.unwrap().flush().await.unwrap();
}

#[tokio::test]
async fn report_capacity_rejection_is_atomic_and_leaves_the_store_writable() {
    let f = Fixture::new().await;
    let bots = bots(&f, 2).await;
    let origin = root(&f, &bots[0]).await;
    let delivery = send(&f, &origin, &bots[1], "Request").await.unwrap();
    finish(&f, &origin, "Sent").await;
    let recipient = start(&f, &delivery.recipient_conversation_id, Some(&delivery), "Answer").await;
    finish(&f, &recipient, "Answer").await;
    // A large but loader-valid legacy name can make a real report prompt far exceed raw-answer
    // budgets. Keep current history near the limit while retaining a modest report's full budget.
    let history = f.store.mutate(|s| s.add_conversation(&bots[0])).await.unwrap().id;
    f.store
        .mutate(|s| {
            s.disk_mut().bots[1].name = "n".repeat(1024 * 1024);
            s.conversation_mut(&history)?.messages.push(message(Role::User, "", &f.iso_now()));
            let bytes = serde_json::to_vec(s.disk()).unwrap().len();
            s.conversation_mut(&history)?.messages[0].text = "h".repeat(MAX_STATE_BYTES - bytes - 3 * 1024 * 1024);
            Ok(())
        })
        .await
        .unwrap();
    let name = f.store.read(|s| s.disk().bots[1].name.clone());
    let prompt = format!("Automatic report from {name}: {}", json!({ "botReplies": [{ "bot": name, "answer": "Answer" }] }));
    let before = f.store.read(|s| s.disk().clone());
    let disk_before = f.disk_text();
    let ids = vec![delivery.id.clone()];
    let error = f.store.mutate(|s| s.check_bot_report_capacity(&origin.conversation, &prompt, &ids)).await.unwrap_err();
    assert_eq!(error.status, 413);
    assert_eq!(f.store.read(|s| s.disk().clone()), before);
    assert_eq!(f.disk_text(), disk_before);
    f.store.read(|s| s.assert_writable()).unwrap();
    f.store.mutate(|s| s.check_bot_report_capacity(&origin.conversation, "Small report", &ids)).await.unwrap();
    // The scheduler can durably record failed report admission; no permanent write-failed latch.
    f.store.mutate(|s| s.fail_bot_reports(&origin.conversation, &error.message, f.now())).await.unwrap();
    assert_eq!(f.store.read(|s| s.disk().bot_deliveries[0].report.as_ref().unwrap().status), ReportStatus::Error);
    f.reopen().await.unwrap().flush().await.unwrap();
}

// Admission is not a load-time schema restriction. Seed structurally valid legacy history with
// extra recipient-local turns (empty report markers were accepted before this admission limit).
async fn legacy_children(f: &Fixture, template: &BotDelivery, count: usize) {
    f.store
        .mutate(|s| {
            let template_exchange = s.disk().bot_exchanges.iter().find(|e| e.id == template.exchange_id).unwrap().clone();
            let template_conversation = s.conversation(&template.recipient_conversation_id)?.clone();
            for _ in 0..count {
                let answer = new_id();
                let mut recipient = template_conversation.clone();
                recipient.id = new_id();
                let mut exchange = template_exchange.clone();
                exchange.id = new_id();
                exchange.source_message_id = answer.clone();
                exchange.messages[0].id = new_id();
                let mut delivery = template.clone();
                delivery.id = new_id();
                delivery.exchange_id = exchange.id.clone();
                delivery.source_message_id = answer.clone();
                delivery.recipient_conversation_id = recipient.id.clone();
                let source = s.conversation_mut(&template.source_conversation_id)?;
                source.messages.push(Message { report_delivery_ids: Some(vec![]), ..message(Role::User, "Legacy local turn", &f.iso_now()) });
                source.messages.push(Message {
                    id: answer,
                    bot_exchange_ids: Some(vec![exchange.id.clone()]),
                    ..message(Role::Assistant, "Legacy send", &f.iso_now())
                });
                let disk = s.disk_mut();
                disk.conversations.push(recipient);
                disk.bot_exchanges.push(exchange);
                disk.bot_deliveries.push(delivery);
            }
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn fourteen_delivery_admission_is_idempotent_chain_local_and_not_a_load_time_limit() {
    assert_eq!(MAX_BOT_DELIVERIES_PER_CHAIN, 14);
    assert_eq!(MAX_BOT_TURNS_PER_CHAIN, 2 * MAX_BOT_DELIVERIES_PER_CHAIN + 1);
    let f = Fixture::new().await;
    let bots = bots(&f, 2).await;
    let origin = root(&f, &bots[0]).await;
    let parent = send(&f, &origin, &bots[1], "Root").await.unwrap();
    finish(&f, &origin, "Sent").await;
    let recipient = start(&f, &parent.recipient_conversation_id, Some(&parent), "Delegating").await;
    let first = send(&f, &recipient, &bots[0], "Legacy child").await.unwrap();
    finish(&f, &recipient, "Waiting").await;
    legacy_children(&f, &first, MAX_BOT_DELIVERIES_PER_CHAIN - 3).await;
    let f = Fixture { store: f.reopen().await.unwrap(), ..f };
    assert_eq!(f.store.read(|s| s.disk().bot_deliveries.len()), 13);
    let child = start(&f, &first.recipient_conversation_id, Some(&first), "Answer").await;
    finish(&f, &child, "Answer").await;
    let summary = report(&f, &recipient.conversation, "First internal report").await;
    let fourteenth = send(&f, &summary, &bots[0], "Fourteenth").await.unwrap();
    let revision = f.store.snapshot().revision;
    assert_eq!(send(&f, &summary, &bots[0], "Fourteenth").await.unwrap().id, fourteenth.id);
    assert_eq!(f.store.snapshot().revision, revision, "retry at the cap is a no-op");
    finish(&f, &summary, "Waiting again").await;
    let next = f.store.read(|s| s.disk().bot_deliveries.iter().find(|d| d.hop == 2 && d.status == DeliveryStatus::Queued).unwrap().clone());
    let child = start(&f, &next.recipient_conversation_id, Some(&next), "Answer").await;
    finish(&f, &child, "Answer").await;
    let summary = report(&f, &recipient.conversation, "Second internal report").await;
    let before = f.store.read(|s| s.disk().clone());
    let bytes = f.disk_text();
    let error = send(&f, &summary, &bots[0], "Fifteenth distinct delivery").await.unwrap_err();
    assert_eq!((error.status, error.message.as_str()), (409, BOT_CHAIN_CAPACITY_ERROR));
    assert_eq!(f.store.read(|s| s.disk().clone()), before);
    assert_eq!(f.disk_text(), bytes);
    f.store.read(|s| s.assert_writable()).unwrap();
    finish(&f, &summary, "No further delivery admitted").await;
    let unrelated = root(&f, &bots[0]).await;
    let other = send(&f, &unrelated, &bots[1], "Different chain").await.unwrap();
    assert_ne!(other.chain_id, parent.chain_id);
    finish(&f, &unrelated, "Sent").await;
    legacy_children(&f, &first, 2).await;
    let f = Fixture { store: f.reopen().await.expect("legacy chain above fourteen deliveries must still load"), ..f };
    assert_eq!(f.store.read(|s| s.disk().bot_deliveries.iter().filter(|d| d.chain_id == parent.chain_id).count()), 16);
    let next = f
        .store
        .read(|s| s.disk().bot_deliveries.iter().find(|d| d.chain_id == parent.chain_id && d.hop == 2 && d.status == DeliveryStatus::Queued).unwrap().clone());
    let child = start(&f, &next.recipient_conversation_id, Some(&next), "Answer").await;
    finish(&f, &child, "Answer").await;
    let summary = report(&f, &recipient.conversation, "Legacy oversized chain report").await;
    assert_eq!(send(&f, &summary, &bots[0], "Still no new delivery").await.unwrap_err().message, BOT_CHAIN_CAPACITY_ERROR);
}

#[tokio::test]
async fn near_capacity_delivery_rejection_restores_links_and_includes_vault_bytes() {
    let f = Fixture::new().await;
    let bots = bots(&f, 2).await;
    let turn = root(&f, &bots[0]).await;
    f.store
        .mutate(|s| {
            for index in 0..100 {
                s.disk_mut().secrets.push(VaultSecret {
                    id: new_id(),
                    name: format!("SECRET_{index}"),
                    bot_id: None,
                    value: "v".repeat(8192),
                    created_at: f.iso_now(),
                    updated_at: f.iso_now(),
                });
            }
            let bytes = serde_json::to_vec(s.disk()).unwrap().len();
            s.conversation_mut(&turn.conversation)?.messages[0].text = "h".repeat(MAX_STATE_BYTES - bytes - 2_600_000);
            Ok(())
        })
        .await
        .unwrap();
    // The ~820 KiB vault is part of current bytes, not free capacity. Use a message whose new
    // copies/reservation cross the true limit but would fit if current vault fields were omitted.
    let before = f.store.read(|s| s.disk().clone());
    let disk_before = f.disk_text();
    let error = send(&f, &turn, &bots[1], &"x".repeat(100_000)).await.unwrap_err();
    assert_eq!(error.status, 413);
    assert_eq!(f.store.read(|s| s.disk().clone()), before);
    assert_eq!(f.disk_text(), disk_before);
    f.store.read(|s| s.assert_writable()).unwrap();
    send(&f, &turn, &bots[1], "A short message fits").await.unwrap();
    f.store.flush().await.unwrap();
}
