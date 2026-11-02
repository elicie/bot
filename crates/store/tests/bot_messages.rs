//! Data-rule port of `tests/bot-messages.test.ts`: admission validation, capacity reservation,
//! persisted link validation, delivery reconciliation, and redaction.

mod common;

use common::{Fixture, assert_load_rejected, write_json};
use grok_bot_model::*;
use grok_bot_store::bot_messages::{
    MAX_BOT_MESSAGE_BYTES, bot_delivery_receipt, existing_bot_delivery, find_existing_bot_delivery, validate_bot_message_input,
};

struct Exchange {
    sender: String,
    recipient: String,
    source: String,
    run: String,
    message: String,
}

async fn configure(f: &Fixture, api_key: &str) {
    let settings = SettingsInput {
        base_url: "http://127.0.0.1:12345/v1".into(),
        model: "fixture".into(),
        workspace: String::new(),
        api_key: Some(api_key.into()),
        clear_api_key: None,
    };
    f.store.mutate(|s| s.update_settings(settings)).await.unwrap();
}

/// A sender conversation whose active run has produced an assistant message.
async fn sending(f: &Fixture) -> Exchange {
    let sender = f.bot_id();
    let recipient = f
        .store
        .mutate(|s| s.add_bot(BotInput { name: "Receiver".into(), color: Some("#3b82f6".into()), instructions: None, shape: None, pinned: None }))
        .await
        .unwrap()
        .id;
    let source = f.store.mutate(|s| s.add_conversation(&sender)).await.unwrap().id;
    let (run, message) = (new_id(), new_id());
    let now = f.iso_now();
    f.store
        .mutate(|s| {
            let conversation = s.conversation_mut(&source)?;
            conversation.messages.push(Message {
                id: message.clone(),
                role: Role::Assistant,
                text: String::new(),
                created_at: now.clone(),
                tools: vec![],
                bot_exchange_ids: None,
                source_bot_id: None,
                attachments: None,
                speaker_bot_id: None,
                report_delivery_ids: None,
            });
            conversation.run = RunState { id: run.clone(), status: RunStatus::Running, started_at: Some(now), ..RunState::idle() };
            Ok(())
        })
        .await
        .unwrap();
    Exchange { sender, recipient, source, run, message }
}

fn message_input(to: &str, text: &str) -> BotMessageInput {
    BotMessageInput { to_bot_id: to.into(), text: text.into(), request_id: None }
}

async fn queue(f: &Fixture, x: &Exchange, input: BotMessageInput) -> HttpResult<BotDelivery> {
    f.store.mutate(|s| s.queue_bot_message(&x.source, &x.run, &x.message, input)).await
}

/// The runtime side of a delivery: the recipient conversation receives the message and answers.
async fn answer(f: &Fixture, delivery: &BotDelivery, text: &str, status: RunStatus) {
    let now = f.iso_now();
    let (from, prompt) = (delivery.from_bot_id.clone(), delivery.text.clone());
    f.store
        .mutate(|s| {
            let conversation = s.conversation_mut(&delivery.recipient_conversation_id)?;
            conversation.messages = vec![
                Message {
                    id: new_id(),
                    role: Role::User,
                    text: prompt,
                    created_at: now.clone(),
                    tools: vec![],
                    bot_exchange_ids: None,
                    source_bot_id: Some(from),
                    attachments: None,
                    speaker_bot_id: None,
                    report_delivery_ids: None,
                },
                Message {
                    id: new_id(),
                    role: Role::Assistant,
                    text: text.into(),
                    created_at: now.clone(),
                    tools: vec![],
                    bot_exchange_ids: None,
                    source_bot_id: None,
                    attachments: None,
                    speaker_bot_id: None,
                    report_delivery_ids: None,
                },
            ];
            conversation.run = RunState { id: new_id(), status: RunStatus::Running, started_at: Some(now.clone()), ..RunState::idle() };
            Ok(())
        })
        .await
        .unwrap();
    if status != RunStatus::Running {
        f.store
            .mutate(|s| {
                let run = &mut s.conversation_mut(&delivery.recipient_conversation_id)?.run;
                run.status = status;
                run.finished_at = Some(now);
                Ok(())
            })
            .await
            .unwrap();
    }
}

fn delivery_at(f: &Fixture, index: usize) -> BotDelivery {
    f.store.read(|s| s.disk().bot_deliveries[index].clone())
}

#[tokio::test]
async fn bot_admission_validates_complete_reviewable_messages_and_reserves_capacity_before_mutating() {
    let f = Fixture::new().await;
    configure(&f, "-").await;
    let x = sending(&f).await;
    for (input, status) in [
        (message_input(&x.sender, "self"), 400),
        (message_input(&new_id(), "missing"), 404),
        (message_input(&x.recipient, ""), 400),
        (message_input(&x.recipient, " "), 400),
        (message_input(&x.recipient, &"x".repeat(100001)), 400),
        (message_input(&x.recipient, &"😀".repeat(50000)), 413),
        (BotMessageInput { request_id: Some("a b".into()), ..message_input(&x.recipient, "ok") }, 400),
    ] {
        assert_eq!(queue(&f, &x, input).await.unwrap_err().status, status);
    }
    assert_eq!("😀".repeat(50000).len(), 200000);
    const { assert!(200000 > MAX_BOT_MESSAGE_BYTES) };
    assert!(f.store.read(|s| s.disk().bot_deliveries.is_empty()));
    let stale = f.store.mutate(|s| s.queue_bot_message(&x.source, &new_id(), &x.message, message_input(&x.recipient, "late"))).await.unwrap_err();
    assert_eq!((stale.status, stale.message.as_str()), (409, "The sending run is no longer active."));

    let bytes = f.disk_text().len();
    let source = x.source.clone();
    f.store
        .mutate(|s| {
            s.conversation_mut(&source)?.messages[0].text = "x".repeat(64 * 1024 * 1024 - 500000 - bytes);
            Ok(())
        })
        .await
        .unwrap();
    let before = f.store.snapshot().revision;
    let error = queue(&f, &x, message_input(&x.recipient, "Do not commit over capacity.")).await.unwrap_err();
    assert_eq!(error.status, 413);
    assert!(error.message.contains("64 MiB"));
    assert_eq!(f.store.snapshot().revision, before);
    f.store.read(|s| {
        assert!(s.disk().bot_exchanges.is_empty());
        assert!(s.disk().bot_deliveries.is_empty());
        assert_eq!(s.disk().conversations.len(), 1);
        assert_eq!(s.conversation(&x.source).unwrap().messages[0].bot_exchange_ids, None);
        s.assert_writable().unwrap();
    });
    f.store
        .mutate(|s| {
            s.conversation_mut(&source)?.messages[0].text.clear();
            Ok(())
        })
        .await
        .unwrap();
    let delivery =
        queue(&f, &x, BotMessageInput { request_id: Some(" review-1 ".into()), ..message_input(&x.recipient, " Fits after capacity is available. ") })
            .await
            .unwrap();
    assert_eq!(delivery.text, "Fits after capacity is available.");
    assert_eq!(delivery.request_id.as_deref(), Some("review-1"));
    assert_eq!((delivery.hop, delivery.chain_id.as_str(), delivery.status), (1, delivery.id.as_str(), DeliveryStatus::Queued));
    let snapshot = f.store.snapshot();
    assert_eq!(snapshot.bot_exchanges[0].bot_ids, [x.sender.clone(), x.recipient.clone()]);
    assert_eq!(snapshot.conversations[0].messages[0].bot_exchange_ids, Some(vec![delivery.exchange_id.clone()]));
    assert_eq!(snapshot.bot_deliveries[0].source_message_id, x.message);
    assert_eq!(snapshot.bot_deliveries[0].recipient_conversation_id, delivery.recipient_conversation_id);
    assert_eq!(snapshot.conversations[1].title, "Message from Grok");
    let revision = snapshot.revision;
    assert_eq!(queue(&f, &x, message_input(&x.recipient, &delivery.text)).await.unwrap().id, delivery.id);
    assert_eq!(f.store.snapshot().revision, revision);
    let error = queue(&f, &x, message_input(&x.recipient, "Second distinct message.")).await.unwrap_err();
    assert_eq!(error.status, 409);
    assert!(error.message.contains("one distinct"));
    // The receipt and idempotency helpers used by the runtime.
    let deliveries = f.store.read(|s| s.disk().bot_deliveries.clone());
    assert_eq!(find_existing_bot_delivery(&deliveries, &x.message, &message_input(&x.recipient, &delivery.text)).unwrap().unwrap().id, delivery.id);
    assert_eq!(find_existing_bot_delivery(&deliveries, &x.message, &message_input(&x.recipient, "other")).unwrap_err().status, 409);
    assert!(existing_bot_delivery(&deliveries, &new_id(), &message_input(&x.recipient, "other")).is_none());
    assert_eq!(
        bot_delivery_receipt(&delivery),
        serde_json::json!({ "deliveryId": delivery.id, "exchangeId": delivery.exchange_id, "recipientConversationId": delivery.recipient_conversation_id, "status": "queued" })
    );
}

#[test]
fn bot_message_input_validation_matches_zod_messages() {
    let error = |input: BotMessageInput| validate_bot_message_input(input).unwrap_err().message;
    let to = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
    assert_eq!(error(message_input("x", "t")), "toBotId: Invalid UUID");
    assert_eq!(error(message_input(to, " ")), "text: Too small: expected string to have >=1 characters");
    assert_eq!(error(message_input(to, &"x".repeat(100001))), "text: Too big: expected string to have <=100000 characters");
    assert_eq!(
        error(BotMessageInput { request_id: Some("  ".into()), ..message_input(to, "t") }),
        r"requestId: Too small: expected string to have >=1 characters; requestId: Invalid string: must match pattern /^[\w.-]+$/"
    );
}

#[tokio::test]
async fn bot_message_chains_are_limited_to_four_hops() {
    let f = Fixture::new().await;
    let x = sending(&f).await;
    let mut current = queue(&f, &x, message_input(&x.recipient, "Hop 1")).await.unwrap();
    for hop in 2..=5 {
        // The recipient's run replies by sending back to the other bot.
        let (run, message) = (new_id(), new_id());
        let now = f.iso_now();
        let (from, text) = (current.from_bot_id.clone(), current.text.clone());
        let recipient = current.recipient_conversation_id.clone();
        f.store
            .mutate(|s| {
                let conversation = s.conversation_mut(&recipient)?;
                conversation.messages = vec![
                    Message {
                        id: new_id(),
                        role: Role::User,
                        text,
                        created_at: now.clone(),
                        tools: vec![],
                        bot_exchange_ids: None,
                        source_bot_id: Some(from),
                        attachments: None,
                        speaker_bot_id: None,
                        report_delivery_ids: None,
                    },
                    Message {
                        id: message.clone(),
                        role: Role::Assistant,
                        text: String::new(),
                        created_at: now.clone(),
                        tools: vec![],
                        bot_exchange_ids: None,
                        source_bot_id: None,
                        attachments: None,
                        speaker_bot_id: None,
                        report_delivery_ids: None,
                    },
                ];
                conversation.run = RunState { id: run.clone(), status: RunStatus::Running, started_at: Some(now), ..RunState::idle() };
                Ok(())
            })
            .await
            .unwrap();
        let back = BotMessageInput { to_bot_id: current.from_bot_id.clone(), text: format!("Hop {hop}"), request_id: None };
        let result = f.store.mutate(|s| s.queue_bot_message(&recipient, &run, &message, back)).await;
        if hop == 5 {
            let error = result.unwrap_err();
            assert_eq!((error.status, error.message.as_str()), (409, "Bot message chains are limited to four hops."));
        } else {
            let next = result.unwrap();
            assert_eq!((next.hop, next.chain_id.as_str()), (hop, current.chain_id.as_str()));
            current = next;
        }
    }
    assert_eq!(f.store.read(|s| s.disk().bot_deliveries.len()), 4);
}

#[tokio::test]
async fn bot_messages_migrate_missing_arrays_reject_invalid_links_and_interrupt_running_deliveries() {
    let f = Fixture::new().await;
    let mut original = f.disk_json();
    original.as_object_mut().unwrap().remove("botExchanges");
    original.as_object_mut().unwrap().remove("botDeliveries");
    write_json(&f.state_file(), &original);
    let migrated = f.reopen().await.unwrap();
    assert!(migrated.snapshot().bot_exchanges.is_empty());
    assert!(migrated.snapshot().bot_deliveries.is_empty());
    drop(migrated);
    let f = Fixture { store: f.reopen().await.unwrap(), ..f };
    let x = sending(&f).await;
    queue(&f, &x, message_input(&x.recipient, "Survive a restart.")).await.unwrap();
    let source = x.source.clone();
    f.store
        .mutate(|s| {
            let run = &mut s.conversation_mut(&source)?.run;
            run.status = RunStatus::Completed;
            run.finished_at = Some("2026-01-01T00:00:00.000Z".into());
            Ok(())
        })
        .await
        .unwrap();
    let queued = f.reopen().await.unwrap();
    assert_eq!(queued.read(|s| s.disk().bot_deliveries[0].status), DeliveryStatus::Queued);
    drop(queued);
    let disk = f.disk_json();
    let corruptions: Vec<fn(&mut serde_json::Value)> = vec![
        |s| s["botDeliveries"][0]["extra"] = true.into(),
        |s| s["botDeliveries"][0]["hop"] = 5.into(),
        |s| s["botDeliveries"][0]["chainId"] = new_id().into(),
        |s| s["botDeliveries"][0]["toBotId"] = s["botDeliveries"][0]["fromBotId"].clone(),
        |s| s["botDeliveries"][0]["sourceMessageId"] = new_id().into(),
        |s| s["botExchanges"][0]["messages"][0]["text"] = "Not the approved message".into(),
        |s| {
            let ids = s["botExchanges"][0]["botIds"].as_array_mut().unwrap();
            ids.reverse();
        },
        |s| s["botDeliveries"][0]["status"] = "completed".into(),
    ];
    for corrupt in corruptions {
        let mut state = disk.clone();
        corrupt(&mut state);
        let bytes = write_json(&f.state_file(), &state);
        assert_load_rejected(&f, &bytes).await;
    }
    write_json(&f.state_file(), &disk);
    let f = Fixture { store: f.reopen().await.unwrap(), ..f };
    let queued = delivery_at(&f, 0);
    // Scheduler dispatch: the recipient run starts and the delivery projects to running.
    answer(&f, &queued, "", RunStatus::Running).await;
    let running = delivery_at(&f, 0);
    assert_eq!(running.status, DeliveryStatus::Running);
    assert!(running.execution_run_id.is_some() && running.started_at.is_some());
    let interrupted = f.reopen().await.unwrap();
    interrupted.read(|s| {
        let delivery = &s.disk().bot_deliveries[0];
        assert_eq!(delivery.status, DeliveryStatus::Interrupted);
        assert!(delivery.finished_at.is_some());
        assert_eq!(delivery.error.as_deref(), Some("The server stopped before this run finished."));
        assert_eq!(s.disk().bot_exchanges[0].messages.len(), 1);
        assert_eq!(s.conversation(&delivery.recipient_conversation_id).unwrap().run.permission, None);
    });
    drop(interrupted);
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.read(|s| s.disk().bot_deliveries[0].status), DeliveryStatus::Interrupted);
}

#[tokio::test]
async fn bot_message_snapshots_redact_credentials_while_preserving_durable_approved_text() {
    let f = Fixture::new().await;
    let key = "bot-delivery-private-provider-key";
    configure(&f, key).await;
    let x = sending(&f).await;
    let mut events = f.store.subscribe();
    let delivery = queue(&f, &x, message_input(&x.recipient, &format!("Approved {key}"))).await.unwrap();
    let event = events.recv().await.unwrap();
    assert!(!serde_json::to_string(&*event).unwrap().contains(key));
    let state = f.store.snapshot();
    assert_eq!(state.bot_exchanges[0].messages[0].text, "Approved [redacted]");
    assert_eq!(state.bot_deliveries[0].text, "Approved [redacted]");
    assert_eq!(state.bot_exchanges[0].bot_ids, [x.sender.clone(), x.recipient.clone()]);
    let disk = f.disk_json();
    assert_eq!(disk["settings"]["apiKey"], key);
    assert_eq!(disk["botDeliveries"][0]["text"], format!("Approved {key}"));
    assert_eq!(disk["botExchanges"][0]["messages"][0]["text"], format!("Approved {key}"));
    answer(&f, &delivery, &format!("Recipient answered {key}"), RunStatus::Completed).await;
    let completed = delivery_at(&f, 0);
    assert_eq!(completed.status, DeliveryStatus::Completed);
    let exchange = f.store.read(|s| s.disk().bot_exchanges[0].clone());
    assert_eq!(exchange.messages.len(), 2);
    assert_eq!(exchange.messages[1].bot_id, x.recipient);
    assert_eq!(exchange.updated_at, completed.finished_at.clone().unwrap());
    assert_eq!(f.store.snapshot().bot_exchanges[0].messages[1].text, "Recipient answered [redacted]");
    assert!(!serde_json::to_string(&f.store.snapshot()).unwrap().contains(key));
    // A completed delivery with its projected answer is valid persisted state.
    f.store.flush().await.unwrap();
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn recipient_failures_and_queued_cancellation_are_durable_terminal_results_without_partial_answers() {
    let f = Fixture::new().await;
    let x = sending(&f).await;
    let failed = queue(&f, &x, message_input(&x.recipient, "A durable queued message.")).await.unwrap();
    assert!(f.store.read(|s| s.is_reserved_conversation(&failed.recipient_conversation_id)));
    let now = f.now();
    f.store.mutate(|s| s.fail_bot_delivery(&failed.id, "Fixture engine unavailable.", now)).await.unwrap();
    let failed = delivery_at(&f, 0);
    assert_eq!(failed.status, DeliveryStatus::Error);
    assert_eq!(failed.error.as_deref(), Some("Fixture engine unavailable."));
    assert!(failed.finished_at.is_some());
    assert!(f.conversation(&failed.recipient_conversation_id).messages.is_empty());
    assert_eq!(f.store.read(|s| s.disk().bot_exchanges[0].messages.len()), 1);
    // A second run sends again; the queued delivery is cancelled before it starts.
    let (run, message) = (new_id(), new_id());
    let (source, now) = (x.source.clone(), f.iso_now());
    f.store
        .mutate(|s| {
            let conversation = s.conversation_mut(&source)?;
            conversation.messages.push(Message {
                id: message.clone(),
                role: Role::Assistant,
                text: String::new(),
                created_at: now.clone(),
                tools: vec![],
                bot_exchange_ids: None,
                source_bot_id: None,
                attachments: None,
                speaker_bot_id: None,
                report_delivery_ids: None,
            });
            conversation.run = RunState { id: run.clone(), status: RunStatus::Running, started_at: Some(now), ..RunState::idle() };
            Ok(())
        })
        .await
        .unwrap();
    let second = f.store.mutate(|s| s.queue_bot_message(&source, &run, &message, message_input(&x.recipient, "A durable queued message."))).await.unwrap();
    assert_ne!(second.id, failed.id);
    let source_run = x.source.clone();
    f.store
        .mutate(|s| {
            s.conversation_mut(&source_run)?.run.status = RunStatus::Completed;
            Ok(())
        })
        .await
        .unwrap();
    assert!(!f.store.mutate(|s| s.cancel_queued_work(&x.source)).await.unwrap());
    assert!(f.store.mutate(|s| s.cancel_queued_work(&second.recipient_conversation_id)).await.unwrap());
    let cancelled = delivery_at(&f, 1);
    assert_eq!(cancelled.status, DeliveryStatus::Cancelled);
    assert!(cancelled.finished_at.is_some());
    // Cancelling again finds nothing queued and does not commit.
    let revision = f.store.snapshot().revision;
    assert!(!f.store.mutate(|s| s.cancel_queued_work(&second.recipient_conversation_id)).await.unwrap());
    assert_eq!(f.store.snapshot().revision, revision);
    let reopened = f.reopen().await.unwrap();
    let statuses: Vec<DeliveryStatus> = reopened.read(|s| s.disk().bot_deliveries.iter().map(|delivery| delivery.status).collect());
    assert_eq!(statuses, [DeliveryStatus::Error, DeliveryStatus::Cancelled]);
}
