//! Bot read state, update notifications, bot deletion rules and cascades, and attachment
//! validation, limits, blob storage, and persistence.

mod common;

use std::os::unix::fs::PermissionsExt;

use common::{Fixture, assert_load_rejected, write_json};
use grok_bot_model::*;
use grok_bot_store::attachments::*;
use grok_bot_store::bot_messages::{MAX_BOT_REPORTS_PER_CHAIN, queued_reports};
use serde_json::{Value, json};

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
        speaker_bot_id: None,
        report_delivery_ids: None,
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
    let input = RoutineInput {
        bot_id: second.clone(),
        name: "Daily".into(),
        prompt: "Report".into(),
        schedule: RoutineSchedule::Interval { minutes: 60 },
        enabled: true,
    };
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
            conversation.messages =
                vec![Message { source_bot_id: Some(from), ..message(Role::User, &text, &now) }, Message { id: a, ..message(Role::Assistant, "Answer", &now) }];
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
    let input =
        RoutineInput { bot_id: b.clone(), name: "B routine".into(), prompt: "Run".into(), schedule: RoutineSchedule::Interval { minutes: 30 }, enabled: true };
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
    assert_eq!(
        error(vec![upload("big.txt", "text/plain", &vec![b'a'; MAX_TEXT_ATTACHMENT_BYTES + 1])]),
        (413, "\"big.txt\" is too large to attach (maximum 256 KB for text files).".into())
    );
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

#[tokio::test]
async fn deleting_a_bot_removes_its_scoped_permission_rules_only() {
    let f = Fixture::new().await;
    let keep = add_bot(&f, "Keep").await;
    let gone = add_bot(&f, "Gone").await;
    for bot_id in [Some(gone.clone()), Some(keep.clone()), None] {
        let input = PermissionRuleInput { category: PermissionCategory::FileWrite, action: PermissionAction::Allow, bot_id, server: None };
        f.store.mutate(move |s| s.add_permission_rule(input, true)).await.unwrap();
    }
    let target = gone.clone();
    f.store.mutate(move |s| s.delete_bot(&target)).await.unwrap();
    let rules = f.store.read(|s| s.permission_rules().to_vec());
    assert_eq!(rules.len(), 2, "{rules:?}");
    assert!(rules.iter().all(|rule| rule.bot_id.as_deref() != Some(gone.as_str())));
    assert!(rules.iter().any(|rule| rule.bot_id.as_deref() == Some(keep.as_str())));
}

#[tokio::test]
async fn deleting_a_bot_removes_it_from_groups() {
    let f = Fixture::new().await;
    let a = add_bot(&f, "A").await;
    let b = add_bot(&f, "B").await;
    let members = vec![f.bot_id(), a.clone(), b.clone()];
    let group = f.store.mutate(move |s| s.add_group(GroupInput { name: "Team".into(), color: None, shape: None, member_bot_ids: members })).await.unwrap();
    let target = a.clone();
    f.store.mutate(move |s| s.delete_bot(&target)).await.unwrap();
    let saved = f.store.read(|s| s.disk().groups.iter().find(|g| g.id == group.id).cloned().unwrap());
    assert_eq!(saved.member_bot_ids, vec![f.bot_id(), b]);
}

// ---------------------------------------------------------------------------------------------
// Follow-up ("report") turns of finished deliveries

fn report_of(f: &Fixture, delivery: &str) -> Option<DeliveryReport> {
    f.store.read(|s| s.disk().bot_deliveries.iter().find(|d| d.id == delivery).unwrap().report.clone())
}

fn report_status(f: &Fixture, delivery: &str) -> Option<ReportStatus> {
    report_of(f, delivery).map(|report| report.status)
}

fn delivery_status(f: &Fixture, delivery: &str) -> DeliveryStatus {
    f.store.read(|s| s.disk().bot_deliveries.iter().find(|d| d.id == delivery).unwrap().status)
}

/// The runtime side of a report turn (`Runtime::start_report`): the hidden input marked with the
/// queued reports, the reply message, the active run, and the reports running. Returns the reply as
/// a sender and the reported delivery IDs.
async fn start_report(f: &Fixture, conversation: &str) -> (Sending, Vec<String>) {
    let now = f.iso_now();
    let (run, input, answer) = (new_id(), new_id(), new_id());
    let (id, r, a) = (conversation.to_owned(), run.clone(), answer.clone());
    let ids = f
        .store
        .mutate(move |s| {
            let ids: Vec<String> = queued_reports(&s.disk().bot_deliveries, &id).iter().map(|d| d.id.clone()).collect();
            assert!(!ids.is_empty(), "a report is queued");
            let conversation = s.conversation_mut(&id)?;
            let marker = Some(ids.clone());
            conversation.messages.push(Message { id: input, report_delivery_ids: marker, ..message(Role::User, "{\"botReplies\":[]}", &now) });
            conversation.messages.push(Message { id: a, ..message(Role::Assistant, "Relayed the answer.", &now) });
            conversation.run = RunState { id: r.clone(), status: RunStatus::Running, started_at: Some(now), ..RunState::idle() };
            for delivery in s.disk_mut().bot_deliveries.iter_mut().filter(|d| ids.contains(&d.id)) {
                delivery.report = Some(DeliveryReport { status: ReportStatus::Running, run_id: Some(r.clone()), ..DeliveryReport::queued() });
            }
            Ok(ids)
        })
        .await
        .unwrap();
    (Sending { source: conversation.to_owned(), run, message: answer }, ids)
}

/// End the active run of `id` with `status` (an error carries a message).
async fn end(f: &Fixture, id: &str, status: RunStatus) {
    let now = f.iso_now();
    f.store
        .mutate(|s| {
            let run = &mut s.conversation_mut(id)?.run;
            run.status = status;
            run.error = (status == RunStatus::Error).then(|| "Recipient provider failed.".to_owned());
            run.finished_at = Some(now);
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn finished_deliveries_queue_reports_only_for_plain_conversations_and_back_up_state_first() {
    let f = Fixture::new().await;
    let backup = f.data_dir.join(grok_bot_store::PRE_REPORTS_BACKUP);
    let a = f.bot_id();
    let b = add_bot(&f, "B").await;
    let c = add_bot(&f, "C").await;
    let x = sending(&f, &a).await;
    let first = send(&f, &x, &b, "A to B").await;
    f.finish(&x.source).await;
    assert_eq!(report_of(&f, &first.id), None, "queued and running deliveries are not reported");
    let y = receive(&f, &first, false).await;
    assert_eq!(report_of(&f, &first.id), None);

    // B forwards to C from its recipient conversation; that level is not reported.
    let second = send(&f, &y, &c, "B to C").await;
    let before_reports = f.disk_text();
    assert!(!backup.exists());
    f.finish(&y.source).await;
    assert_eq!(report_status(&f, &first.id), Some(ReportStatus::Queued), "the completed delivery from A's chat is reported");
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), before_reports, "the state before the first report is kept");
    assert_eq!(std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777, 0o600);
    receive(&f, &second, true).await;
    assert_eq!(second.hop, 2);
    assert_eq!(report_of(&f, &second.id), None, "a recipient conversation's delivery is not reported");
    let public = serde_json::to_value(f.store.snapshot()).unwrap();
    assert_eq!(public["botDeliveries"][0]["report"], json!({"status": "queued"}));

    // A failed recipient is reported too.
    let z = sending(&f, &a).await;
    let failed = send(&f, &z, &c, "A to C").await;
    f.finish(&z.source).await;
    receive(&f, &failed, false).await;
    end(&f, &failed.recipient_conversation_id, RunStatus::Error).await;
    assert_eq!(delivery_status(&f, &failed.id), DeliveryStatus::Error);
    assert_eq!(report_status(&f, &failed.id), Some(ReportStatus::Queued));

    // Routine conversations are not reported to.
    let input =
        RoutineInput { bot_id: a.clone(), name: "A routine".into(), prompt: "Run".into(), schedule: RoutineSchedule::Interval { minutes: 30 }, enabled: true };
    let routine = f.store.mutate(|s| s.add_routine(input)).await.unwrap().id;
    let now = f.now();
    let claimed = f.store.mutate(|s| s.run_routine(&routine, now)).await.unwrap();
    let w = sending_from(&f, &claimed.conversation_id).await;
    let from_routine = send(&f, &w, &b, "Routine to B").await;
    f.finish(&w.source).await;
    receive(&f, &from_routine, true).await;
    assert_eq!(report_of(&f, &from_routine.id), None);
    assert!(f.store.read(|s| !s.reports_to_conversation(&claimed.conversation_id) && s.reports_to_conversation(&x.source)));
    assert!(f.store.read(|s| !s.reports_to_conversation(&first.recipient_conversation_id)));

    // Later writes never replace the backup.
    let other = add_conversation(&f, &a).await;
    f.start(&other).await;
    f.finish(&other).await;
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), before_reports);
    let reopened = f.reopen().await.expect("queued reports pass reference validation");
    reopened.read(|s| {
        assert_eq!(queued_reports(&s.disk().bot_deliveries, &x.source).iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), [first.id.as_str()]);
        assert_eq!(queued_reports(&s.disk().bot_deliveries, &z.source).iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), [failed.id.as_str()]);
    });
}

#[tokio::test]
async fn cancelling_queued_reports_is_scoped_idempotent_and_survives_restart() {
    let f = Fixture::new().await;
    let a = f.bot_id();
    let b = add_bot(&f, "B").await;
    let x = sending(&f, &a).await;
    let completed = send(&f, &x, &b, "Already reported").await;
    f.finish(&x.source).await;
    receive(&f, &completed, true).await;
    start_report(&f, &x.source).await;
    f.finish(&x.source).await;

    // Two different user turns leave two reports queued in the same conversation.
    let mut cancelled = Vec::new();
    for question in ["Pending 1", "Pending 2"] {
        let turn = sending_from(&f, &x.source).await;
        let delivery = send(&f, &turn, &b, question).await;
        f.finish(&x.source).await;
        receive(&f, &delivery, true).await;
        cancelled.push(delivery.id);
    }
    let turn = sending_from(&f, &x.source).await;
    let pending = send(&f, &turn, &b, "Recipient has not run yet").await;
    f.finish(&x.source).await;
    let y = sending(&f, &a).await;
    let other = send(&f, &y, &b, "Other conversation").await;
    f.finish(&y.source).await;
    receive(&f, &other, true).await;
    let z = sending(&f, &a).await;
    let running = send(&f, &z, &b, "Already reporting").await;
    f.finish(&z.source).await;
    receive(&f, &running, true).await;
    start_report(&f, &z.source).await;

    f.set_time("2026-01-01T00:01:00.000Z");
    let before = f.store.read(|s| s.disk().clone());
    let requested =
        [cancelled.clone(), vec![cancelled[0].clone(), completed.id.clone(), pending.id.clone(), other.id.clone(), running.id.clone(), new_id()]].concat();
    let actual = f.store.mutate(|s| s.cancel_bot_reports(&x.source, &requested)).await.unwrap();
    assert_eq!(actual, cancelled, "only still-queued requested IDs are returned, once each");
    let after = f.store.read(|s| s.disk().clone());
    assert_eq!(after.revision, before.revision + 1, "one durable mutation cancels the selected queued reports");
    assert_eq!(after.conversations, before.conversations, "no run or hidden report message is created or changed");
    assert_eq!(after.bot_exchanges, before.bot_exchanges, "recipient answers stay in their exchanges");
    let mut expected = before.bot_deliveries;
    for delivery in expected.iter_mut().filter(|d| cancelled.contains(&d.id)) {
        delivery.report = Some(DeliveryReport { status: ReportStatus::Cancelled, finished_at: Some(f.iso_now()), ..DeliveryReport::queued() });
    }
    assert_eq!(after.bot_deliveries, expected, "only queued reports of the selected source change");
    assert!(f.store.read(|s| queued_reports(&s.disk().bot_deliveries, &x.source).is_empty()));
    assert_eq!(report_status(&f, &other.id), Some(ReportStatus::Queued));
    assert_eq!(report_status(&f, &running.id), Some(ReportStatus::Running));
    assert_eq!(report_status(&f, &completed.id), Some(ReportStatus::Completed));
    assert_eq!((delivery_status(&f, &pending.id), report_of(&f, &pending.id)), (DeliveryStatus::Queued, None));

    let revision = after.revision;
    assert!(f.store.mutate(|s| s.cancel_bot_reports(&x.source, &requested)).await.unwrap().is_empty());
    assert!(f.store.mutate(|s| s.cancel_bot_reports(&z.source, std::slice::from_ref(&running.id))).await.unwrap().is_empty());
    assert_eq!(f.store.snapshot().revision, revision, "repeated cancellation and a running-only report are no-ops");
    let missing = new_id();
    assert_eq!(f.store.mutate(|s| s.cancel_bot_reports(&missing, &requested)).await.unwrap_err().status, 404);
    assert_eq!(f.store.snapshot().revision, revision, "invalid requests do not write");

    f.finish(&z.source).await;
    for _ in 0..2 {
        let reopened = f.reopen().await.expect("cancelled reports without runs pass schema and reference validation");
        reopened.read(|s| {
            assert!(queued_reports(&s.disk().bot_deliveries, &x.source).is_empty(), "cancelled reports are never queued again");
            for delivery in s.disk().bot_deliveries.iter().filter(|d| cancelled.contains(&d.id)) {
                let report = delivery.report.as_ref().unwrap();
                assert_eq!(
                    (report.status, report.run_id.as_deref(), report.finished_at.as_deref(), report.error.as_deref()),
                    (ReportStatus::Cancelled, None, Some(f.iso_now().as_str()), None)
                );
            }
            assert_eq!(s.conversation(&x.source).unwrap(), &f.conversation(&x.source));
        });
    }
}

#[tokio::test]
async fn a_selected_report_does_not_cancel_a_later_arrival_and_backup_precedes_the_first_shape_write() {
    let f = Fixture::new().await;
    let backup = f.data_dir.join(grok_bot_store::PRE_UNSTARTED_REPORT_CANCELLATION_BACKUP);
    let a = f.bot_id();
    let b = add_bot(&f, "B").await;
    let x = sending(&f, &a).await;
    let first = send(&f, &x, &b, "Selected A").await;
    f.finish(&x.source).await;
    receive(&f, &first, true).await;
    let selected = vec![first.id.clone()];
    // The UI selected A before B's report arrived in this same source conversation.
    let next = sending_from(&f, &x.source).await;
    let later = send(&f, &next, &b, "Later B").await;
    f.finish(&x.source).await;
    receive(&f, &later, true).await;
    let before = f.disk_text();
    let original_backup = std::fs::read(f.data_dir.join(grok_bot_store::PRE_REPORTS_BACKUP)).unwrap();
    for invalid in [vec![], vec![first.id.clone(), "not-a-uuid".into()], vec![first.id.clone(); grok_bot_store::bot_messages::MAX_BOT_DELIVERIES + 1]] {
        assert_eq!(f.store.mutate(|s| s.cancel_bot_reports(&x.source, &invalid)).await.unwrap_err().status, 400);
        assert_eq!(f.disk_text(), before, "invalid input never falls back to broad cancellation");
    }
    assert!(f.store.mutate(|s| s.cancel_bot_reports(&x.source, &[new_id()])).await.unwrap().is_empty());
    assert_eq!(f.disk_text(), before);
    assert!(!backup.exists(), "rejected selections and actual no-ops create no backup");
    assert_eq!(f.store.mutate(|s| s.cancel_bot_reports(&x.source, &selected)).await.unwrap(), selected);
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), before, "the exact compatible bytes precede the first incompatible write");
    assert_eq!(std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read(f.data_dir.join(grok_bot_store::PRE_REPORTS_BACKUP)).unwrap(), original_backup);
    assert_eq!(report_status(&f, &first.id), Some(ReportStatus::Cancelled));
    assert_eq!(report_status(&f, &later.id), Some(ReportStatus::Queued));
    let (_, ids) = start_report(&f, &x.source).await;
    assert_eq!(ids, std::slice::from_ref(&later.id), "only B is still eligible to execute");
    f.finish(&x.source).await;
    assert_eq!(report_status(&f, &later.id), Some(ReportStatus::Completed));
    let reopened = f.reopen().await.unwrap();
    assert!(reopened.mutate(|s| s.cancel_bot_reports(&x.source, &selected)).await.unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), before, "later writes and restarts never replace the backup");
}

#[tokio::test]
async fn cancellation_keeps_an_existing_private_backup_and_does_not_fabricate_one_after_upgrade() {
    let f = Fixture::new().await;
    let a = f.bot_id();
    let b = add_bot(&f, "B").await;
    let x = sending(&f, &a).await;
    let first = send(&f, &x, &b, "Question").await;
    f.finish(&x.source).await;
    receive(&f, &first, true).await;
    let before = f.disk_json();
    let backup = f.data_dir.join(grok_bot_store::PRE_UNSTARTED_REPORT_CANCELLATION_BACKUP);
    let saved = serde_json::to_vec(&before).unwrap();
    std::fs::write(&backup, &saved).unwrap();
    std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600)).unwrap();
    f.store.mutate(|s| s.cancel_bot_reports(&x.source, std::slice::from_ref(&first.id))).await.unwrap();
    f.reopen().await.unwrap();
    assert_eq!(std::fs::read(&backup).unwrap(), saved, "a pre-existing private backup is never overwritten");

    // Simulate an install that already wrote the new shape without this backup implementation.
    // Reopening must not pretend that those incompatible bytes are a usable rollback backup.
    let g = Fixture::new().await;
    let b = add_bot(&g, "B").await;
    let y = sending(&g, &g.bot_id()).await;
    let delivery = send(&g, &y, &b, "Already cancelled on disk").await;
    g.finish(&y.source).await;
    receive(&g, &delivery, true).await;
    let mut existing = g.disk_json();
    delivery_json(&mut existing, &delivery.id)["report"] = json!({"status": "cancelled", "finishedAt": g.iso_now()});
    write_json(&g.state_file(), &existing);
    let reopened = g.reopen().await.unwrap();
    assert_eq!(reopened.read(|s| s.disk().bot_deliveries[0].report.as_ref().unwrap().status), ReportStatus::Cancelled);
    assert!(!g.data_dir.join(grok_bot_store::PRE_UNSTARTED_REPORT_CANCELLATION_BACKUP).exists());
}

#[tokio::test]
async fn a_required_cancellation_backup_failure_leaves_compatible_state_on_disk() {
    for obstruction in [
        "directory",
        "symlink",
        "hard-link",
        "public-file",
        "truncated-file",
        "incompatible-file",
        "version-two",
        "version-99",
        "empty-bots",
        "dangling-conversation-bot",
    ] {
        let f = Fixture::new().await;
        let a = f.bot_id();
        let b = add_bot(&f, "B").await;
        let x = sending(&f, &a).await;
        let first = send(&f, &x, &b, "Question").await;
        f.finish(&x.source).await;
        receive(&f, &first, true).await;
        let before = f.disk_text();
        let backup = f.data_dir.join(grok_bot_store::PRE_UNSTARTED_REPORT_CANCELLATION_BACKUP);
        let outside = f.root.path().join("untouched");
        std::fs::write(&outside, &before).unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o600)).unwrap();
        match obstruction {
            "directory" => std::fs::create_dir(&backup).unwrap(),
            "symlink" => std::os::unix::fs::symlink(&outside, &backup).unwrap(),
            "hard-link" => std::fs::hard_link(&outside, &backup).unwrap(),
            _ => {
                let mut state = f.disk_json();
                match obstruction {
                    "incompatible-file" => delivery_json(&mut state, &first.id)["report"] = json!({"status": "cancelled", "finishedAt": f.iso_now()}),
                    "version-two" => state["version"] = json!(2),
                    "version-99" => state["version"] = json!(99),
                    "empty-bots" => state["bots"] = json!([]),
                    "dangling-conversation-bot" => state["conversations"][0]["botId"] = json!(new_id()),
                    _ => {}
                }
                let bytes = if obstruction == "truncated-file" { "{\"version\":1,".into() } else { serde_json::to_string(&state).unwrap() };
                std::fs::write(&backup, bytes).unwrap();
                let mode = if obstruction == "public-file" { 0o644 } else { 0o600 };
                std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(mode)).unwrap();
            }
        }
        let backup_before = (obstruction != "directory").then(|| std::fs::read(&backup).unwrap());
        let error = f.store.mutate(|s| s.cancel_bot_reports(&x.source, std::slice::from_ref(&first.id))).await.unwrap_err();
        assert_eq!(error.status, 500, "{obstruction}");
        assert_eq!(f.disk_text(), before, "failed backup cannot persist an unreadable new shape: {obstruction}");
        assert_eq!(f.store.read(|s| s.assert_writable()).unwrap_err().status, 503);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), before, "{obstruction}");
        if let Some(backup_before) = backup_before {
            assert_eq!(std::fs::read(&backup).unwrap(), backup_before, "the invalid backup is never overwritten: {obstruction}");
        }
    }
}

#[tokio::test]
async fn report_turns_continue_the_chain_and_a_chain_gets_two_reports() {
    let f = Fixture::new().await;
    let a = f.bot_id();
    let b = add_bot(&f, "B").await;
    let x = sending(&f, &a).await;
    let first = send(&f, &x, &b, "Question 1").await;
    f.finish(&x.source).await;
    receive(&f, &first, true).await;

    // Report 1: the reply sends again; the delivery continues the reported chain at hop 2.
    let (report, ids) = start_report(&f, &x.source).await;
    assert_eq!(ids, std::slice::from_ref(&first.id));
    assert_eq!(report_of(&f, &first.id).unwrap().run_id.as_deref(), Some(report.run.as_str()));
    let second = send(&f, &report, &b, "Question 2").await;
    assert_eq!((second.chain_id.as_str(), second.hop), (first.chain_id.as_str(), 2));
    f.reopen().await.expect("a running report and its child delivery are valid");
    f.finish(&x.source).await;
    let done = report_of(&f, &first.id).unwrap();
    assert_eq!((done.status, done.finished_at.is_some(), done.error), (ReportStatus::Completed, true, None));
    receive(&f, &second, true).await;
    assert_eq!(report_status(&f, &second.id), Some(ReportStatus::Queued));

    // Report 2 sends hop 3: delivered, but the chain used its reports. Stopping it is final.
    let (report, _) = start_report(&f, &x.source).await;
    let third = send(&f, &report, &b, "Question 3").await;
    assert_eq!((third.chain_id.as_str(), third.hop), (first.chain_id.as_str(), 3));
    end(&f, &x.source, RunStatus::Cancelled).await;
    assert_eq!(report_status(&f, &second.id), Some(ReportStatus::Cancelled), "a stopped report turn is not retried");
    let answered = receive(&f, &third, false).await;
    // B can still forward hop 4 from its recipient conversation; hop 5 is refused below.
    let fourth = send(&f, &answered, &a, "Hop 4").await;
    assert_eq!(fourth.hop, 4);
    f.finish(&answered.source).await;
    assert_eq!(delivery_status(&f, &third.id), DeliveryStatus::Completed);
    let skipped = report_of(&f, &third.id).unwrap();
    assert_eq!((skipped.status, skipped.run_id, skipped.finished_at.is_some()), (ReportStatus::Skipped, None, true));
    assert!(f.store.read(|s| queued_reports(&s.disk().bot_deliveries, &x.source).is_empty()));
    assert_eq!(MAX_BOT_REPORTS_PER_CHAIN, 2);

    let last = receive(&f, &fourth, false).await;
    let input = BotMessageInput { to_bot_id: b.clone(), text: "Hop 5".into(), request_id: None };
    let error = f.store.mutate(|s| s.queue_bot_message(&last.source, &last.run, &last.message, input)).await.unwrap_err();
    assert_eq!((error.status, error.message.as_str()), (409, "Bot message chains are limited to four hops."));
    f.finish(&last.source).await;
    f.reopen().await.expect("report chains pass reference validation");
}

/// Edits a valid `state.json` into one that must not load.
type Forgery<'a> = Box<dyn Fn(&mut Value) + 'a>;

fn delivery_json<'a>(state: &'a mut Value, id: &str) -> &'a mut Value {
    state["botDeliveries"].as_array_mut().unwrap().iter_mut().find(|d| d["id"] == id).unwrap()
}

/// The conversation `id` and the index of its first report message.
fn report_message(state: &mut Value, id: &str) -> (usize, usize) {
    let index = state["conversations"].as_array().unwrap().iter().position(|c| c["id"] == id).unwrap();
    let message = state["conversations"][index]["messages"].as_array().unwrap().iter().position(|m| m.get("reportDeliveryIds").is_some()).unwrap();
    (index, message)
}

#[tokio::test]
async fn forged_report_state_is_rejected_on_load() {
    let f = Fixture::new().await;
    let a = f.bot_id();
    let b = add_bot(&f, "B").await;
    let x = sending(&f, &a).await;
    let first = send(&f, &x, &b, "Question 1").await;
    f.finish(&x.source).await;
    let y = receive(&f, &first, false).await;
    // An unreported delivery from B's recipient conversation.
    let forward = send(&f, &y, &a, "Forwarded").await;
    f.finish(&y.source).await;
    receive(&f, &forward, true).await;
    let (report, _) = start_report(&f, &x.source).await;
    let second = send(&f, &report, &b, "Question 2").await;
    f.finish(&x.source).await;
    receive(&f, &second, true).await;
    let (report, _) = start_report(&f, &x.source).await;
    let third = send(&f, &report, &b, "Question 3").await;
    f.finish(&x.source).await;
    receive(&f, &third, true).await;
    assert_eq!(report_status(&f, &third.id), Some(ReportStatus::Skipped));
    let original = f.disk_json();

    let source = x.source.clone();
    let forgeries: Vec<(&str, Forgery)> = vec![
        ("unknown report field", Box::new(|s| delivery_json(s, &first.id)["report"]["extra"] = 1.into())),
        (
            "running report of an earlier run",
            Box::new(|s| {
                let report = &mut delivery_json(s, &first.id)["report"];
                report["status"] = "running".into();
                report.as_object_mut().unwrap().remove("finishedAt");
            }),
        ),
        ("queued report that already has a report message", Box::new(|s| delivery_json(s, &first.id)["report"] = json!({"status": "queued"}))),
        ("cancelled report without a finish timestamp", Box::new(|s| delivery_json(s, &first.id)["report"] = json!({"status": "cancelled"}))),
        (
            "cancelled report without a run but with a hidden report message",
            Box::new(|s| delivery_json(s, &first.id)["report"] = json!({"status": "cancelled", "finishedAt": f.iso_now()})),
        ),
        (
            "report on an unfinished delivery",
            Box::new(|s| {
                let delivery = delivery_json(s, &forward.id);
                delivery["status"] = "running".into();
                delivery["report"] = json!({"status": "queued"});
            }),
        ),
        ("report on a delivery from a recipient conversation", Box::new(|s| delivery_json(s, &forward.id)["report"] = json!({"status": "queued"}))),
        ("third report in a chain", Box::new(|s| delivery_json(s, &third.id)["report"] = json!({"status": "queued"}))),
        (
            "report marker on an assistant message",
            Box::new(|s| {
                let (conversation, index) = report_message(s, &source);
                let messages = s["conversations"][conversation]["messages"].as_array_mut().unwrap();
                let marker = messages[index].as_object_mut().unwrap().remove("reportDeliveryIds").unwrap();
                messages[index + 1]["reportDeliveryIds"] = marker;
            }),
        ),
        (
            "report marker naming another conversation's delivery",
            Box::new(|s| {
                let (conversation, index) = report_message(s, &source);
                s["conversations"][conversation]["messages"][index]["reportDeliveryIds"] = json!([forward.id]);
            }),
        ),
        (
            "report child restarting its chain at hop 1",
            Box::new(|s| {
                let delivery = delivery_json(s, &second.id);
                delivery["chainId"] = second.id.clone().into();
                delivery["hop"] = 1.into();
            }),
        ),
    ];
    for (name, forge) in &forgeries {
        let mut state = original.clone();
        forge(&mut state);
        assert_ne!(state, original, "{name} changes the state");
        let bytes = write_json(&f.state_file(), &state);
        assert_load_rejected(&f, &bytes).await;
    }
    write_json(&f.state_file(), &original);
    f.reopen().await.expect("the untouched state loads");
}

#[tokio::test]
async fn deleting_bots_drops_their_pending_reports_and_report_children() {
    let f = Fixture::new().await;
    let a = f.bot_id();
    let b = add_bot(&f, "B").await;
    let c = add_bot(&f, "C").await;

    // A queued report, dropped later with its delivery.
    let x = sending(&f, &a).await;
    let queued = send(&f, &x, &c, "To C").await;
    f.finish(&x.source).await;
    receive(&f, &queued, true).await;
    assert_eq!(report_status(&f, &queued.id), Some(ReportStatus::Queued));

    // A reported A → B delivery whose report turn sent A → C.
    let y = sending(&f, &a).await;
    let first = send(&f, &y, &b, "To B").await;
    f.finish(&y.source).await;
    receive(&f, &first, true).await;
    let (report, ids) = start_report(&f, &y.source).await;
    assert_eq!(ids, std::slice::from_ref(&first.id));
    let child = send(&f, &report, &c, "Follow-up to C").await;
    assert_eq!((child.chain_id.as_str(), child.hop), (first.chain_id.as_str(), 2));
    f.finish(&y.source).await;
    receive(&f, &child, true).await;

    let removed = f.store.mutate(|s| s.delete_bot(&b)).await.unwrap();
    assert!(removed.contains(&first.recipient_conversation_id) && removed.contains(&child.recipient_conversation_id), "{removed:?}");
    let reopened = f.reopen().await.expect("the cascade stays loadable");
    reopened.read(|s| {
        let ids: Vec<&str> = s.disk().bot_deliveries.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, [queued.id.as_str()], "the report child goes with the reported delivery");
        let source = s.conversation(&y.source).unwrap();
        let marker = source.messages.iter().find(|m| m.report_delivery_ids.is_some()).expect("the report message stays hidden");
        assert_eq!(marker.report_delivery_ids.as_deref(), Some(&[][..]));
        assert!(source.messages.iter().all(|m| m.bot_exchange_ids.is_none()));
    });
    drop(reopened);
    f.store.mutate(|s| s.delete_bot(&c)).await.unwrap();
    assert!(f.store.read(|s| s.disk().bot_deliveries.is_empty()), "the queued report went with C's delivery");
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn restart_keeps_queued_reports_interrupts_running_ones_and_failed_starts_are_final() {
    let f = Fixture::new().await;
    let a = f.bot_id();
    let b = add_bot(&f, "B").await;
    let x = sending(&f, &a).await;
    let first = send(&f, &x, &b, "Question").await;
    f.finish(&x.source).await;
    receive(&f, &first, true).await;
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.read(|s| s.disk().bot_deliveries[0].report.clone()).map(|r| r.status), Some(ReportStatus::Queued), "queued stays queued");
    drop(reopened);

    let (report, _) = start_report(&f, &x.source).await;
    let reopened = f.reopen().await.unwrap();
    let interrupted = reopened.read(|s| s.disk().bot_deliveries[0].report.clone()).unwrap();
    assert_eq!(interrupted.status, ReportStatus::Interrupted);
    assert_eq!(interrupted.run_id.as_deref(), Some(report.run.as_str()));
    assert_eq!(interrupted.error.as_deref(), Some("The server stopped before this run finished."));
    assert!(reopened.read(|s| queued_reports(&s.disk().bot_deliveries, &x.source).is_empty()), "never retried");
    drop(reopened);
    let again = f.reopen().await.unwrap();
    assert_eq!(again.read(|s| s.disk().bot_deliveries[0].report.clone()).unwrap().status, ReportStatus::Interrupted);
    drop(again);

    // A follow-up that cannot start ends its reports with the error.
    let g = Fixture::new().await;
    let a = g.bot_id();
    let b = add_bot(&g, "B").await;
    let y = sending(&g, &a).await;
    let delivery = send(&g, &y, &b, "Question").await;
    g.finish(&y.source).await;
    receive(&g, &delivery, true).await;
    let now = g.now();
    let source = y.source.clone();
    g.store.mutate(move |s| s.fail_bot_reports(&source, "Configure a provider and model in settings first.", now)).await.unwrap();
    let failed = report_of(&g, &delivery.id).unwrap();
    assert_eq!((failed.status, failed.run_id, failed.error.as_deref()), (ReportStatus::Error, None, Some("Configure a provider and model in settings first.")));
    assert!(g.store.read(|s| queued_reports(&s.disk().bot_deliveries, &y.source).is_empty()));
    g.reopen().await.expect("a report that never started is valid");
}
