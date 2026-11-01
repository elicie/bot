//! Bot read state, update notifications, bot deletion rules and cascades, and attachment
//! validation, limits, blob storage, and persistence.

mod common;

use std::os::unix::fs::PermissionsExt;

use common::{Fixture, assert_load_rejected, write_json};
use grok_bot_model::*;
use grok_bot_store::attachments::*;

fn bot_input(name: &str) -> BotInput {
    BotInput { name: name.into(), color: None, instructions: None, shape: None, pinned: None }
}

async fn add_bot(f: &Fixture, name: &str) -> String {
    let input = bot_input(name);
    f.store.mutate(|s| s.add_bot(input)).await.unwrap().id
}

async fn add_conversation(f: &Fixture, bot: &str) -> String {
    let bot = bot.to_owned();
    f.store.mutate(move |s| s.add_conversation(&bot)).await.unwrap().id
}

fn bot(f: &Fixture, id: &str) -> Bot {
    f.store.read(|s| s.bot(id).unwrap().clone())
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
    }
}

#[tokio::test]
async fn unread_and_notification_flags_persist_and_skip_no_op_writes() {
    let f = Fixture::new().await;
    let id = f.bot_id();
    let conversation = add_conversation(&f, &id).await;
    assert_eq!(bot(&f, &id).unread, None);
    assert!(!f.disk_text().contains("unread"), "unset flags are not serialized");

    f.store.mutate(|s| s.mark_conversation_unread(&conversation)).await.unwrap();
    assert_eq!(bot(&f, &id).unread, Some(true));
    let revision = f.store.snapshot().revision;
    f.store.mutate(|s| s.set_bot_unread(&id, true)).await.unwrap();
    assert_eq!(f.store.snapshot().revision, revision, "unchanged state is not rewritten");

    // Allowed while a run is active (unlike other bot edits).
    f.start(&conversation).await;
    f.store.mutate(|s| s.set_bot_notify_on_updates(&id, true)).await.unwrap();
    f.store.mutate(|s| s.set_bot_unread(&id, false)).await.unwrap();
    let error = f.store.mutate(|s| s.update_bot(&id, BotPatch { pinned: Some(true), ..BotPatch::default() })).await.unwrap_err();
    assert_eq!(error.status, 409);
    assert_eq!((bot(&f, &id).unread, bot(&f, &id).notify_on_updates), (None, Some(true)));
    f.finish(&conversation).await;
    f.store.mutate(|s| s.set_bot_unread(&id, true)).await.unwrap();

    let missing = new_id();
    assert_eq!(f.store.mutate(|s| s.set_bot_unread(&missing, true)).await.unwrap_err().status, 404);
    assert_eq!(f.store.mutate(|s| s.mark_conversation_unread(&missing)).await.unwrap_err().status, 404);

    let reopened = f.reopen().await.unwrap();
    let persisted = reopened.read(|s| s.bot(&id).unwrap().clone());
    assert_eq!((persisted.unread, persisted.notify_on_updates), (Some(true), Some(true)));
    let public = serde_json::to_value(reopened.snapshot()).unwrap();
    assert_eq!(public["bots"][0]["unread"], true);
    assert_eq!(public["bots"][0]["notifyOnUpdates"], true);
    f.store.mutate(|s| s.set_bot_notify_on_updates(&id, false)).await.unwrap();
    assert_eq!(bot(&f, &id).notify_on_updates, None);
}

#[tokio::test]
async fn deleting_a_bot_is_refused_for_the_last_bot_active_runs_and_queued_work() {
    let f = Fixture::new().await;
    let first = f.bot_id();
    let error = f.store.mutate(|s| s.delete_bot(&first)).await.unwrap_err();
    assert_eq!((error.status, error.message.as_str()), (409, "Keep at least one bot. Add another bot before deleting this one."));
    let missing = new_id();
    assert_eq!(f.store.mutate(|s| s.delete_bot(&missing)).await.unwrap_err().status, 404);

    let second = add_bot(&f, "Second").await;
    // Any active run blocks bot changes, including on another bot.
    let other = add_conversation(&f, &first).await;
    f.start(&other).await;
    let error = f.store.mutate(|s| s.delete_bot(&second)).await.unwrap_err();
    assert_eq!((error.status, error.message.as_str()), (409, "Stop the active run before changing settings, bots, or files."));
    f.finish(&other).await;

    // A queued routine run of the bot.
    let input = RoutineInput { bot_id: second.clone(), name: "Daily".into(), prompt: "Report".into(), schedule: RoutineSchedule::Interval { minutes: 60 }, enabled: true };
    let routine = f.store.mutate(|s| s.add_routine(input)).await.unwrap().id;
    let now = f.now();
    f.store.mutate(|s| s.run_routine(&routine, now)).await.unwrap();
    let error = f.store.mutate(|s| s.delete_bot(&second)).await.unwrap_err();
    assert_eq!((error.status, error.message.as_str()), (409, "Wait for or cancel this bot’s queued routine runs before deleting it."));
    let queued = f.runs()[0].conversation_id.clone();
    f.store.mutate(|s| s.cancel_queued_work(&queued)).await.unwrap();
    assert!(f.runs()[0].result.is_some());

    // A queued bot message to the bot.
    let x = sending(&f, &first).await;
    let delivery = f
        .store
        .mutate(|s| s.queue_bot_message(&x.source, &x.run, &x.message, BotMessageInput { to_bot_id: second.clone(), text: "Hello".into(), request_id: None }))
        .await
        .unwrap();
    f.finish(&x.source).await;
    let error = f.store.mutate(|s| s.delete_bot(&second)).await.unwrap_err();
    assert_eq!((error.status, error.message.as_str()), (409, "Wait for or cancel the queued bot messages of this bot before deleting it."));
    f.store.mutate(|s| s.cancel_queued_work(&delivery.recipient_conversation_id)).await.unwrap();
    assert_eq!(f.store.read(|s| s.disk().bot_deliveries[0].status), DeliveryStatus::Cancelled);

    let removed = f.store.mutate(|s| s.delete_bot(&second)).await.unwrap();
    assert_eq!(removed.len(), 2, "the routine conversation and the cancelled recipient conversation: {removed:?}");
    let reopened = f.reopen().await.unwrap();
    reopened.read(|s| {
        let disk = s.disk();
        assert_eq!(disk.bots.len(), 1);
        assert!(disk.routines.is_empty() && disk.routine_runs.is_empty() && disk.bot_deliveries.is_empty() && disk.bot_exchanges.is_empty());
        assert!(disk.conversations.iter().all(|c| c.bot_id == first));
        let source = disk.conversations.iter().find(|c| c.id == x.source).unwrap();
        assert!(source.messages.iter().all(|m| m.bot_exchange_ids.is_none()), "links to removed exchanges are dropped");
    });
}

struct Sending {
    source: String,
    run: String,
    message: String,
}

/// A conversation of `bot` whose active run has produced an assistant message.
async fn sending(f: &Fixture, bot: &str) -> Sending {
    let source = add_conversation(f, bot).await;
    sending_from(f, &source).await
}

async fn sending_from(f: &Fixture, source: &str) -> Sending {
    let (run, message_id) = (new_id(), new_id());
    let now = f.iso_now();
    let source_id = source.to_owned();
    let (m, r) = (message_id.clone(), run.clone());
    f.store
        .mutate(move |s| {
            let conversation = s.conversation_mut(&source_id)?;
            conversation.messages.push(Message { id: m, ..message(Role::Assistant, "", &now) });
            conversation.run = RunState { id: r, status: RunStatus::Running, started_at: Some(now), ..RunState::idle() };
            Ok(())
        })
        .await
        .unwrap();
    Sending { source: source.to_owned(), run, message: message_id }
}

/// The recipient run of `delivery` starts and answers; returns the recipient as a sender.
async fn receive(f: &Fixture, delivery: &BotDelivery, finish: bool) -> Sending {
    let now = f.iso_now();
    let (run, answer) = (new_id(), new_id());
    let (from, text, recipient) = (delivery.from_bot_id.clone(), delivery.text.clone(), delivery.recipient_conversation_id.clone());
    let (r, a) = (run.clone(), answer.clone());
    f.store
        .mutate(move |s| {
            let conversation = s.conversation_mut(&recipient)?;
            conversation.messages = vec![
                Message { source_bot_id: Some(from), ..message(Role::User, &text, &now) },
                Message { id: a, ..message(Role::Assistant, "Answer", &now) },
            ];
            conversation.run = RunState { id: r, status: RunStatus::Running, started_at: Some(now), ..RunState::idle() };
            Ok(())
        })
        .await
        .unwrap();
    if finish {
        f.finish(&delivery.recipient_conversation_id).await;
    }
    Sending { source: delivery.recipient_conversation_id.clone(), run, message: answer }
}

async fn send(f: &Fixture, x: &Sending, to: &str, text: &str) -> BotDelivery {
    let input = BotMessageInput { to_bot_id: to.to_owned(), text: text.into(), request_id: None };
    f.store.mutate(|s| s.queue_bot_message(&x.source, &x.run, &x.message, input)).await.unwrap()
}

/// A → B → C chain (both completed) plus an unrelated C conversation and a B routine with history.
async fn chain(f: &Fixture) -> (String, String, String, BotDelivery, BotDelivery) {
    let a = f.bot_id();
    let b = add_bot(f, "B").await;
    let c = add_bot(f, "C").await;
    let x = sending(f, &a).await;
    let first = send(f, &x, &b, "A to B").await;
    f.finish(&x.source).await;
    let y = receive(f, &first, false).await;
    let second = send(f, &y, &c, "B to C").await;
    f.finish(&y.source).await;
    receive(f, &second, true).await;
    add_conversation(f, &c).await;
    let input = RoutineInput { bot_id: b.clone(), name: "B routine".into(), prompt: "Run".into(), schedule: RoutineSchedule::Interval { minutes: 30 }, enabled: true };
    let routine = f.store.mutate(|s| s.add_routine(input)).await.unwrap().id;
    let now = f.now();
    let run = f.store.mutate(|s| s.run_routine(&routine, now)).await.unwrap();
    f.start(&run.conversation_id).await;
    f.finish(&run.conversation_id).await;
    let deliveries = f.store.read(|s| s.disk().bot_deliveries.iter().map(|d| d.status).collect::<Vec<_>>());
    assert_eq!(deliveries, [DeliveryStatus::Completed, DeliveryStatus::Completed]);
    assert_eq!(second.hop, 2);
    (a, b, c, first, second)
}

#[tokio::test]
async fn deleting_a_middle_bot_removes_its_history_and_the_chains_through_it() {
    let f = Fixture::new().await;
    let (a, b, c, first, second) = chain(&f).await;
    let b_conversations: Vec<String> = f.store.read(|s| s.disk().conversations.iter().filter(|c| c.bot_id == b).map(|c| c.id.clone()).collect());
    assert_eq!(b_conversations.len(), 2, "recipient conversation of A→B and the routine conversation");
    let skill = SkillInput { name: "b-skill".into(), description: "B only".into(), instructions: "Do B things.".into() };
    f.store.mutate(|s| s.add_skill(&b, skill, false)).await.unwrap();

    let mut removed = f.store.mutate(|s| s.delete_bot(&b)).await.unwrap();
    let mut expected = [b_conversations.clone(), vec![second.recipient_conversation_id.clone()]].concat();
    expected.sort();
    removed.sort();
    assert_eq!(removed, expected, "B's conversations and C's recipient conversation from B");

    let reopened = f.reopen().await.expect("the cascaded state passes reference validation");
    reopened.read(|s| {
        let disk = s.disk();
        assert_eq!(disk.bots.iter().map(|bot| bot.name.as_str()).collect::<Vec<_>>(), ["Grok", "C"]);
        assert!(disk.conversations.iter().all(|conversation| conversation.bot_id != b));
        assert!(disk.conversations.iter().any(|conversation| conversation.id == first.source_conversation_id), "A's own conversation stays");
        assert_eq!(disk.conversations.iter().filter(|conversation| conversation.bot_id == c).count(), 1, "C keeps its unrelated conversation");
        assert!(disk.routines.is_empty() && disk.routine_runs.is_empty());
        assert!(disk.bot_deliveries.is_empty() && disk.bot_exchanges.is_empty());
        let source = s.conversation(&first.source_conversation_id).unwrap();
        assert!(source.messages.iter().all(|m| m.bot_exchange_ids.is_none()));
        assert!(s.bot(&a).is_ok());
    });
}

#[tokio::test]
async fn deleting_the_first_sender_removes_descendant_deliveries_and_keeps_unrelated_ones() {
    let f = Fixture::new().await;
    let (a, b, c, first, second) = chain(&f).await;
    // An unrelated C → B delivery that does not involve A survives.
    let z = sending(&f, &c).await;
    let unrelated = send(&f, &z, &b, "C to B").await;
    f.finish(&z.source).await;
    receive(&f, &unrelated, true).await;

    let removed = f.store.mutate(|s| s.delete_bot(&a)).await.unwrap();
    assert!(removed.contains(&first.recipient_conversation_id) && removed.contains(&second.recipient_conversation_id), "{removed:?}");
    assert!(removed.contains(&first.source_conversation_id));
    let reopened = f.reopen().await.expect("valid after deleting the chain root");
    reopened.read(|s| {
        let disk = s.disk();
        assert_eq!(disk.bot_deliveries.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), [unrelated.id.as_str()]);
        assert_eq!(disk.bot_exchanges.len(), 1);
        assert!(s.conversation(&unrelated.recipient_conversation_id).is_ok());
        assert_eq!(disk.routine_runs.len(), 1, "B's routine history stays");
        assert!(s.bot(&b).is_ok() && s.bot(&c).is_ok() && s.bot(&a).is_err());
    });
    // Deleting the last two bots in turn: the final one is kept.
    f.store.mutate(|s| s.delete_bot(&b)).await.unwrap();
    assert_eq!(f.store.mutate(|s| s.delete_bot(&c)).await.unwrap_err().status, 409);
    f.reopen().await.unwrap();
}

fn upload(name: &str, mime_type: &str, bytes: &[u8]) -> AttachmentUpload {
    AttachmentUpload { name: name.into(), mime_type: mime_type.into(), data: base64_encode(bytes) }
}

/// PNG signature and the IHDR chunk of a 32×32 image (enough for type detection).
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x20, 0x08, 0x02,
    0x00, 0x00, 0x00, 0xfc, 0x18, 0xed, 0xa3,
];

#[test]
fn attachment_uploads_are_validated_against_types_and_limits() {
    let prepared = prepare_attachments(vec![
        upload("../notes.md", "text/markdown", "# 제목\n본문 @here".as_bytes()),
        upload("pixel.png", "image/png", PNG),
        upload("data.json", "application/octet-stream", b"{\"a\":1}"),
        upload("photo.jpg", "", PNG),
    ])
    .unwrap();
    let metas: Vec<(&str, &str, AttachmentKind)> = prepared.iter().map(|p| (p.meta.name.as_str(), p.meta.mime_type.as_str(), p.meta.kind)).collect();
    assert_eq!(
        metas,
        [
            ("notes.md", "text/markdown", AttachmentKind::Text),
            ("pixel.png", "image/png", AttachmentKind::Image),
            ("data.json", "text/plain", AttachmentKind::Text),
            ("photo.jpg", "image/png", AttachmentKind::Image),
        ],
        "image types come from the bytes"
    );
    assert_eq!(prepared[0].text(), Some("# 제목\n본문 @here"));
    assert_eq!(prepared[1].meta.size, PNG.len() as u64);

    let error = |uploads: Vec<AttachmentUpload>| {
        let error = prepare_attachments(uploads).unwrap_err();
        (error.status, error.message)
    };
    assert_eq!(error((0..9).map(|i| upload(&format!("{i}.txt"), "text/plain", b"x")).collect()), (400, "Attach at most 8 files to one message.".into()));
    assert_eq!(error(vec![upload("big.txt", "text/plain", &vec![b'a'; MAX_TEXT_ATTACHMENT_BYTES + 1])]), (413, "\"big.txt\" is too large to attach (maximum 256 KB for text files).".into()));
    let mut big_image = PNG.to_vec();
    big_image.resize(MAX_IMAGE_ATTACHMENT_BYTES + 1, 0);
    assert_eq!(error(vec![upload("big.png", "image/png", &big_image)]), (413, "\"big.png\" is too large to attach (maximum 5 MB).".into()));
    let texts = (0..3).map(|i| upload(&format!("{i}.txt"), "text/plain", &vec![b'a'; 200 * 1024])).collect();
    assert_eq!(error(texts), (413, "Text attachments of one message may total at most 512 KB.".into()));
    let mut image = PNG.to_vec();
    image.resize(4 * 1024 * 1024, 0);
    let images = (0..3).map(|i| upload(&format!("{i}.png"), "image/png", &image)).collect();
    assert_eq!(error(images), (413, "Attachments of one message may total at most 10 MB.".into()));
    assert_eq!(error(vec![upload("empty.txt", "text/plain", b"")]), (400, "\"empty.txt\" is empty and was not attached.".into()));
    assert_eq!(error(vec![upload("fake.png", "image/png", b"<svg onload=alert(1)>")]), (400, "\"fake.png\" is not a PNG, JPEG, GIF, or WebP image.".into()));
    assert_eq!(error(vec![upload("blob.bin", "application/octet-stream", &[0xff, 0xfe, 0])]).0, 400);
    assert_eq!(error(vec![upload("nul.txt", "text/plain", b"a\0b")]).0, 400);
    assert_eq!(error(vec![AttachmentUpload { name: "a.txt".into(), mime_type: "text/plain".into(), data: "not base64!".into() }]).0, 400);
    assert_eq!(error(vec![upload("", "text/plain", b"x")]).0, 400);
    assert!(prepare_attachments(Vec::new()).unwrap().is_empty());
}

#[tokio::test]
async fn attachment_blobs_are_private_and_metadata_persists_with_the_message() {
    let f = Fixture::new().await;
    let id = f.bot_id();
    let conversation = add_conversation(&f, &id).await;
    let prepared = prepare_attachments(vec![upload("notes.txt", "text/plain", b"hello"), upload("pixel.png", "image/png", PNG)]).unwrap();
    write_attachments(&f.data_dir, &conversation, &prepared).unwrap();
    let directory = f.data_dir.join("attachments").join(&conversation);
    assert_eq!(std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777, 0o700);
    let blob = directory.join(&prepared[1].meta.id);
    assert_eq!(std::fs::metadata(&blob).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(read_attachment(&f.data_dir, &conversation, &prepared[1].meta.id).unwrap(), PNG);
    assert_eq!(read_attachment(&f.data_dir, &conversation, &new_id()).unwrap_err().status, 404);
    assert_eq!(read_attachment(&f.data_dir, "../x", &prepared[0].meta.id).unwrap_err().status, 404);
    assert_eq!(read_attachment(&f.data_dir, &conversation, "../../state.json").unwrap_err().status, 404);
    std::os::unix::fs::symlink(f.state_file(), directory.join("7c9e6679-7425-40de-944b-e07fc1f90ae7")).unwrap();
    assert_eq!(read_attachment(&f.data_dir, &conversation, "7c9e6679-7425-40de-944b-e07fc1f90ae7").unwrap_err().status, 404, "links are refused");

    let metas: Vec<Attachment> = prepared.iter().map(|p| p.meta.clone()).collect();
    let now = f.iso_now();
    let stored = metas.clone();
    f.store
        .mutate(|s| {
            s.conversation_mut(&conversation)?.messages.push(Message { attachments: Some(stored), ..message(Role::User, "", &now) });
            Ok(())
        })
        .await
        .unwrap();
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.read(|s| s.conversation(&conversation).unwrap().messages[0].attachments.clone()), Some(metas.clone()));
    assert!(!f.disk_text().contains("aGVsbG8"), "blob contents stay out of state.json");

    discard_attachments(&f.data_dir, &conversation, &metas[..1]);
    assert!(!directory.join(&metas[0].id).exists() && blob.exists());
    remove_conversation_files(&f.data_dir, &conversation);
    assert!(!directory.exists());
    drop(reopened);

    // Persisted metadata is validated on load.
    let mut state = f.disk_json();
    let original = state.clone();
    for (field, value) in [
        ("name", serde_json::json!("../escape.txt")),
        ("mimeType", serde_json::json!("image/svg+xml")),
        ("size", serde_json::json!(0)),
        ("id", serde_json::json!("not-a-uuid")),
        ("kind", serde_json::json!("binary")),
    ] {
        state["conversations"][0]["messages"][0]["attachments"][1][field] = value;
        let bytes = write_json(&f.state_file(), &state);
        assert_load_rejected(&f, &bytes).await;
        state = original.clone();
    }
    write_json(&f.state_file(), &original);
    f.reopen().await.unwrap();
}
