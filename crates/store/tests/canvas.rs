mod common;

use common::{Fixture, assert_load_rejected, write_json};
use grok_bot_model::*;
use grok_bot_store::MAX_STATE_BYTES;
use grok_bot_store::canvas::{MAX_CANVAS_BYTES, MAX_CANVAS_HISTORY, MAX_CANVAS_REVISION, replace_range, validate_canvas_input};
use serde_json::json;

fn input(revision: u64, content: &str) -> CanvasInput {
    CanvasInput { revision, title: "Plan".into(), language: "markdown".into(), content: content.into() }
}

async fn conversation(f: &Fixture) -> String {
    f.store.mutate(|state| state.add_conversation(&f.bot_id())).await.unwrap().id
}

fn canvas(f: &Fixture, id: &str) -> Canvas {
    f.store.read(|state| state.canvas(id).unwrap().clone())
}

fn message(text: &str, now: &str) -> Message {
    Message {
        id: new_id(),
        role: Role::User,
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

#[tokio::test]
async fn saves_are_conversation_scoped_persisted_and_allowed_during_runs_without_lifting_other_idle_guards() {
    let f = Fixture::new().await;
    let first = conversation(&f).await;
    let second = conversation(&f).await;
    assert_eq!(f.store.read(|state| state.canvas(&first).unwrap_err().status), 404);
    assert!(f.disk_json()["conversations"][0].get("canvas").is_none());
    let created = f
        .store
        .mutate(|state| state.save_canvas(&first, CanvasInput { title: " \u{feff}Plan \n".into(), ..input(0, "# Original\r\n🙂") }, CanvasSource::User))
        .await
        .unwrap();
    assert_eq!((created.revision, created.title.as_str(), created.source), (1, "Plan", CanvasSource::User));
    assert!(created.history.is_empty());
    assert!(!created.redacted);
    assert!(f.conversation(&second).canvas.is_none());
    assert!(f.store.snapshot().settings.workspace.is_empty());
    f.start(&first).await;
    f.set_time("2026-01-01T00:01:00.000Z");
    let saved = f.store.mutate(|state| state.save_canvas(&first, input(1, "# Edited during run\r\n🙂"), CanvasSource::User)).await.unwrap();
    assert_eq!(saved.revision, 2);
    assert_eq!(saved.history, vec![created.version()]);
    assert_eq!(saved.updated_at, f.iso_now());
    assert_eq!(f.conversation(&first).run.status, RunStatus::Running);
    assert_eq!(f.store.read(|state| state.assert_idle().unwrap_err().status), 409);
    assert!(f.store.snapshot().settings.workspace.is_empty());
    let before = f.disk_text();
    let revision = f.store.snapshot().revision;
    let noop = f.store.mutate(|state| state.save_canvas(&first, input(2, &saved.content), CanvasSource::Bot)).await.unwrap();
    assert_eq!(noop, saved);
    assert_eq!(f.store.snapshot().revision, revision);
    assert_eq!(f.disk_text(), before);
    for stale in [0, 1, 3] {
        assert_eq!(f.store.mutate(|state| state.save_canvas(&first, input(stale, "lost update"), CanvasSource::User)).await.unwrap_err().status, 409);
    }
    assert_eq!(f.store.mutate(|state| state.save_canvas(&new_id(), input(0, "unknown"), CanvasSource::User)).await.unwrap_err().status, 404);
    assert_eq!(f.disk_text(), before);
    f.finish(&first).await;
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.read(|state| state.canvas(&first).unwrap().clone()), saved);
    assert!(reopened.read(|state| state.conversation(&second).unwrap().canvas.is_none()));
}

#[test]
fn input_bounds_use_utf16_titles_and_utf8_content_and_exact_ascii_languages() {
    assert!(validate_canvas_input(CanvasInput { title: "🙂".repeat(50), ..input(0, "") }).is_ok());
    assert_eq!(validate_canvas_input(CanvasInput { title: "🙂".repeat(51), ..input(0, "") }).unwrap_err().status, 400);
    assert_eq!(validate_canvas_input(CanvasInput { title: " \n".into(), ..input(0, "") }).unwrap_err().status, 400);
    assert!(validate_canvas_input(input(MAX_CANVAS_REVISION, &"é".repeat(MAX_CANVAS_BYTES / 2))).is_ok());
    assert_eq!(validate_canvas_input(input(MAX_CANVAS_REVISION + 1, "x")).unwrap_err().status, 400);
    assert_eq!(validate_canvas_input(input(0, &"é".repeat(MAX_CANVAS_BYTES / 2 + 1))).unwrap_err().status, 413);
    assert_eq!(validate_canvas_input(input(0, "a\0b")).unwrap_err().status, 400);
    for language in ["", "Rust", "é", "c++", "-rust", " rust", "rust ", &"r".repeat(33)] {
        assert_eq!(validate_canvas_input(CanvasInput { language: language.into(), ..input(0, "x") }).unwrap_err().status, 400, "{language}");
    }
    for language in ["text", "markdown", "rust", "cpp", "typescript", "c-sharp", "a1", &"r".repeat(32)] {
        assert!(validate_canvas_input(CanvasInput { language: language.into(), ..input(0, "x") }).is_ok(), "{language}");
    }
}

#[tokio::test]
async fn bot_range_replacements_preserve_surroundings_and_reject_stale_missing_ambiguous_and_overlapping_anchors() {
    let f = Fixture::new().await;
    let id = conversation(&f).await;
    let original = "前置🙂\r\nconst x = 1;\r\n後置🙂\r\n";
    f.store.mutate(|state| state.save_canvas(&id, input(0, original), CanvasSource::User)).await.unwrap();
    let replaced = f.store.mutate(|state| state.replace_canvas_range(&id, 1, "const x = 1", "const x = 2")).await.unwrap();
    assert_eq!(replaced.content, "前置🙂\r\nconst x = 2;\r\n後置🙂\r\n");
    assert_eq!(replaced.source, CanvasSource::Bot);
    assert_eq!(replaced.history[0].content, original);
    let before = f.disk_text();
    for (revision, old_text, replacement, status) in [
        (1, "const x = 2", "stale", 409),
        (2, "", "blank", 400),
        (2, "not present", "absent", 409),
        (2, "🙂", "ambiguous", 409),
        (2, "const x = 2", "NUL\0", 400),
        (0, "const x = 2", "bad revision", 400),
    ] {
        assert_eq!(f.store.mutate(|state| state.replace_canvas_range(&id, revision, old_text, replacement)).await.unwrap_err().status, status);
    }
    assert_eq!(f.disk_text(), before);
    let noop = f.store.mutate(|state| state.replace_canvas_range(&id, 2, "const x = 2", "const x = 2")).await.unwrap();
    assert_eq!(noop, replaced);
    assert_eq!(f.disk_text(), before);
    let deleted = f.store.mutate(|state| state.replace_canvas_range(&id, 2, "const x = 2;", "")).await.unwrap();
    assert_eq!(deleted.content, "前置🙂\r\n\r\n後置🙂\r\n");
    assert_eq!(deleted.revision, 3);
    for (text, old_text) in [("aaa", "aa"), ("🙂🙂🙂", "🙂🙂")] {
        assert_eq!(replace_range(text, old_text, "x").unwrap_err().status, 409);
    }
    assert_eq!(replace_range("🙂", "🙂", "").unwrap(), "");
    assert_eq!(replace_range(&"x".repeat(MAX_CANVAS_BYTES), "x", "more").unwrap_err().status, 409);
    assert_eq!(replace_range("unique", "unique", &"x".repeat(MAX_CANVAS_BYTES + 1)).unwrap_err().status, 413);
}

#[tokio::test]
async fn history_retains_twenty_old_snapshots_and_restore_always_creates_a_fresh_revision() {
    let f = Fixture::new().await;
    let id = conversation(&f).await;
    for revision in 0..25 {
        let input = CanvasInput {
            title: format!("Title {}", revision + 1),
            language: if revision % 2 == 0 { "rust" } else { "markdown" }.into(),
            ..input(revision, &format!("Version {}", revision + 1))
        };
        f.store.mutate(|state| state.save_canvas(&id, input, CanvasSource::User)).await.unwrap();
    }
    let current = canvas(&f, &id);
    assert_eq!(current.revision, 25);
    assert_eq!(current.history.len(), MAX_CANVAS_HISTORY);
    assert_eq!(current.history.iter().map(|old| old.revision).collect::<Vec<_>>(), (5..25).collect::<Vec<_>>());
    let target = current.history[0].clone();
    let restored = f.store.mutate(|state| state.restore_canvas(&id, 25, target.revision)).await.unwrap();
    assert_eq!(restored.revision, 26);
    assert_eq!(
        (restored.title.as_str(), restored.language.as_str(), restored.content.as_str()),
        (target.title.as_str(), target.language.as_str(), target.content.as_str())
    );
    assert_eq!(restored.source, CanvasSource::Restore);
    assert_eq!(restored.history.last(), Some(&current.version()));
    assert_eq!(restored.history.first().unwrap().revision, 6);
    let before = f.disk_text();
    assert_eq!(f.store.mutate(|state| state.restore_canvas(&id, 25, 24)).await.unwrap_err().status, 409);
    assert_eq!(f.store.mutate(|state| state.restore_canvas(&id, 26, 5)).await.unwrap_err().status, 404);
    assert_eq!(f.disk_text(), before);
    let repeat = f.store.mutate(|state| state.restore_canvas(&id, 26, 24)).await.unwrap();
    let again = f.store.mutate(|state| state.restore_canvas(&id, 27, 24)).await.unwrap();
    assert_eq!((repeat.content.as_str(), again.content.as_str(), again.revision), ("Version 24", "Version 24", 28));
    assert_eq!(again.history.len(), MAX_CANVAS_HISTORY);
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.read(|state| state.canvas(&id).unwrap().clone()), again);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_compare_and_swap_saves_commit_exactly_once() {
    let f = Fixture::new().await;
    let id = conversation(&f).await;
    f.store.mutate(|state| state.save_canvas(&id, input(0, "Original"), CanvasSource::User)).await.unwrap();
    let revision = f.store.snapshot().revision;
    let tasks: Vec<_> = (0..16)
        .map(|index| {
            let (store, id) = (f.store.clone(), id.clone());
            tokio::spawn(async move { store.mutate(|state| state.save_canvas(&id, input(1, &format!("Winner {index}")), CanvasSource::User)).await })
        })
        .collect();
    let mut winners = Vec::new();
    for task in tasks {
        match task.await.unwrap() {
            Ok(winner) => winners.push(winner),
            Err(error) => assert_eq!(error.status, 409),
        }
    }
    assert_eq!(winners.len(), 1);
    assert_eq!(canvas(&f, &id), winners[0]);
    assert_eq!(winners[0].revision, 2);
    assert_eq!(winners[0].history.len(), 1);
    assert_eq!(f.store.snapshot().revision, revision + 1);
    assert_eq!(f.disk_json()["conversations"][0]["canvas"]["content"], winners[0].content);
}

#[tokio::test]
async fn canvas_capacity_preflight_counts_vault_fields_and_rejects_without_touching_state() {
    let f = Fixture::new().await;
    let id = conversation(&f).await;
    f.store.mutate(|state| state.add_secret(SecretInput { name: "CAPACITY_SECRET".into(), value: "s".repeat(8192), bot_id: None }, true)).await.unwrap();
    f.store.mutate(|state| state.save_canvas(&id, input(0, "Small original"), CanvasSource::User)).await.unwrap();
    let now = f.iso_now();
    f.store
        .mutate(|state| {
            state.conversation_mut(&id)?.messages.push(message("", &now));
            let bytes = serde_json::to_vec(state.disk()).unwrap().len();
            state.conversation_mut(&id)?.messages[0].text = "p".repeat(MAX_STATE_BYTES - 2048 - bytes);
            Ok(())
        })
        .await
        .unwrap();
    let original = canvas(&f, &id);
    let revision = f.store.read(|state| state.disk().revision);
    let before = grok_bot_files::content_hash(f.disk_text().as_bytes());
    // The 8 KiB vault field must count: an old conversation-only prospect would admit this edit.
    let error = f.store.mutate(|state| state.save_canvas(&id, input(1, &"x".repeat(4096)), CanvasSource::User)).await.unwrap_err();
    assert_eq!(error.status, 413);
    assert_eq!(canvas(&f, &id), original);
    assert_eq!(f.store.read(|state| state.disk().revision), revision);
    assert_eq!(grok_bot_files::content_hash(f.disk_text().as_bytes()), before);
    f.store
        .mutate(|state| {
            let text = &mut state.conversation_mut(&id)?.messages[0].text;
            text.truncate(text.len() - 8192);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(f.store.mutate(|state| state.save_canvas(&id, input(1, &"x".repeat(4096)), CanvasSource::User)).await.unwrap().revision, 2);
}

#[tokio::test]
async fn current_and_historical_secret_redaction_is_read_only_per_version_and_does_not_destroy_raw_text() {
    let f = Fixture::new().await;
    let safe = conversation(&f).await;
    let secret = conversation(&f).await;
    let fresh = conversation(&f).await;
    let old_key = "HISTORICAL_CANVAS_SECRET_42";
    let current_key = "CURRENT_CANVAS_SECRET_77";
    let language_key = "canvassecret";
    f.store.mutate(|state| state.save_canvas(&safe, input(0, old_key), CanvasSource::User)).await.unwrap();
    f.store.mutate(|state| state.save_canvas(&safe, input(1, "Current text is safe."), CanvasSource::User)).await.unwrap();
    f.store.mutate(|state| state.save_canvas(&secret, input(0, "Initial safe revision"), CanvasSource::User)).await.unwrap();
    f.store
        .mutate(|state| {
            state.save_canvas(&secret, CanvasInput { title: current_key.into(), language: language_key.into(), ..input(1, current_key) }, CanvasSource::User)
        })
        .await
        .unwrap();
    for (name, key) in [("HISTORICAL", old_key), ("CURRENT", current_key), ("LANGUAGE", language_key)] {
        f.store.mutate(|state| state.add_secret(SecretInput { name: name.into(), value: key.into(), bot_id: None }, true)).await.unwrap();
    }
    let public = f.store.snapshot();
    let safe_public = public.conversations.iter().find(|conversation| conversation.id == safe).unwrap().canvas.as_ref().unwrap();
    assert!(!safe_public.redacted);
    assert!(safe_public.history[0].redacted);
    assert_eq!(safe_public.history[0].content, "[redacted]");
    let secret_public = public.conversations.iter().find(|conversation| conversation.id == secret).unwrap().canvas.as_ref().unwrap();
    assert!(secret_public.redacted);
    assert_eq!((&*secret_public.title, &*secret_public.language, &*secret_public.content), ("[redacted]", "[redacted]", "[redacted]"));
    let encoded = serde_json::to_string(&public).unwrap();
    assert!(!encoded.contains(old_key) && !encoded.contains(current_key) && !encoded.contains(language_key));
    assert_eq!(canvas(&f, &secret).content, current_key);
    assert!(!canvas(&f, &secret).redacted);
    let before = f.disk_text();
    assert_eq!(f.store.mutate(|state| state.save_canvas(&secret, input(2, "clobber masked content"), CanvasSource::User)).await.unwrap_err().status, 403);
    assert_eq!(f.store.mutate(|state| state.replace_canvas_range(&secret, 2, "[redacted]", "lost secret")).await.unwrap_err().status, 403);
    assert_eq!(f.store.mutate(|state| state.restore_canvas(&secret, 2, 1)).await.unwrap_err().status, 403);
    assert_eq!(f.store.mutate(|state| state.restore_canvas(&safe, 2, 1)).await.unwrap_err().status, 403);
    for input in [
        input(0, current_key),
        CanvasInput { title: current_key.into(), ..input(0, "safe") },
        CanvasInput { language: language_key.into(), ..input(0, "safe") },
    ] {
        assert_eq!(f.store.mutate(|state| state.save_canvas(&fresh, input, CanvasSource::User)).await.unwrap_err().status, 403);
    }
    assert_eq!(f.store.mutate(|state| state.replace_canvas_range(&safe, 2, "safe", current_key)).await.unwrap_err().status, 403);
    assert_eq!(f.disk_text(), before);
    let saved = f.store.mutate(|state| state.save_canvas(&safe, input(2, "Safe history does not block current edits."), CanvasSource::User)).await.unwrap();
    assert_eq!(saved.revision, 3);
    assert!(!saved.redacted);
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.read(|state| state.canvas(&secret).unwrap().content.clone()), current_key);
    assert!(!f.disk_text().contains("\"redacted\""));
}

#[tokio::test]
async fn matching_a_public_canvas_timestamp_does_not_mask_structural_metadata_or_block_edits() {
    let f = Fixture::new().await;
    let id = conversation(&f).await;
    f.store.mutate(|state| state.save_canvas(&id, input(0, "Original text"), CanvasSource::User)).await.unwrap();
    f.store.mutate(|state| state.save_canvas(&id, input(1, "Current text"), CanvasSource::Bot)).await.unwrap();
    f.store
        .mutate(|state| {
            state.update_settings(SettingsInput {
                base_url: "http://127.0.0.1:12345/v1".into(),
                model: "fixture".into(),
                workspace: String::new(),
                api_key: Some("2026-01".into()),
                clear_api_key: None,
                max_concurrent_runs: None,
            })
        })
        .await
        .unwrap();
    let public = f.store.snapshot().conversations.into_iter().find(|conversation| conversation.id == id).unwrap().canvas.unwrap();
    assert!(!public.redacted);
    assert_eq!(public.updated_at, f.iso_now());
    assert_eq!(public.source, CanvasSource::Bot);
    assert!(!public.history[0].redacted);
    assert_eq!(public.history[0].updated_at, f.iso_now());
    assert_eq!(public.history[0].source, CanvasSource::User);
    assert_eq!(f.store.mutate(|state| state.save_canvas(&id, input(2, "Another safe edit"), CanvasSource::User)).await.unwrap().revision, 3);
    let restored = f.store.mutate(|state| state.restore_canvas(&id, 3, 1)).await.unwrap();
    assert_eq!((restored.revision, restored.source), (4, CanvasSource::Restore));
    assert_eq!(restored.updated_at, f.iso_now());
}

#[tokio::test]
async fn group_and_thread_documents_are_isolated_and_follow_existing_deletion_lifetimes() {
    let f = Fixture::new().await;
    let second =
        f.store.mutate(|state| state.add_bot(BotInput { name: "Second".into(), color: None, instructions: None, shape: None, pinned: None })).await.unwrap();
    let group = f
        .store
        .mutate(|state| state.add_group(GroupInput { name: "Group".into(), color: None, shape: None, member_bot_ids: vec![f.bot_id(), second.id.clone()] }))
        .await
        .unwrap();
    let parent = f.store.mutate(|state| state.add_group_conversation(&group.id)).await.unwrap();
    let post = message("Thread this message", &f.iso_now());
    f.store
        .mutate(|state| {
            state.conversation_mut(&parent.id)?.messages.push(post.clone());
            Ok(())
        })
        .await
        .unwrap();
    let thread = f.store.mutate(|state| state.create_thread(&parent.id, &post.id)).await.unwrap();
    f.store.mutate(|state| state.save_canvas(&parent.id, input(0, "Parent document"), CanvasSource::User)).await.unwrap();
    f.store.mutate(|state| state.save_canvas(&thread.id, input(0, "Thread document"), CanvasSource::User)).await.unwrap();
    let personal = f.store.mutate(|state| state.add_conversation(&second.id)).await.unwrap();
    f.store.mutate(|state| state.save_canvas(&personal.id, input(0, "Personal document"), CanvasSource::User)).await.unwrap();
    assert_eq!(canvas(&f, &parent.id).content, "Parent document");
    assert_eq!(canvas(&f, &thread.id).content, "Thread document");
    let removed = f.store.mutate(|state| state.delete_group(&group.id)).await.unwrap();
    assert!(removed.contains(&parent.id) && removed.contains(&thread.id));
    assert_eq!(canvas(&f, &personal.id).content, "Personal document");
    let removed = f.store.mutate(|state| state.delete_bot(&second.id)).await.unwrap();
    assert!(removed.contains(&personal.id));
    assert_eq!(f.store.read(|state| state.canvas(&personal.id).unwrap_err().status), 404);
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn recipient_conversations_cannot_create_or_edit_canvases() {
    let f = Fixture::new().await;
    let recipient =
        f.store.mutate(|state| state.add_bot(BotInput { name: "Recipient".into(), color: None, instructions: None, shape: None, pinned: None })).await.unwrap();
    let source = conversation(&f).await;
    let (run, post) = (new_id(), message("", &f.iso_now()));
    f.store
        .mutate(|state| {
            let conversation = state.conversation_mut(&source)?;
            let mut post = post.clone();
            post.role = Role::Assistant;
            conversation.messages.push(post);
            conversation.run = RunState { id: run.clone(), status: RunStatus::Running, started_at: Some(f.iso_now()), ..RunState::idle() };
            Ok(())
        })
        .await
        .unwrap();
    let delivery = f
        .store
        .mutate(|state| {
            state.queue_bot_message(&source, &run, &post.id, BotMessageInput { to_bot_id: recipient.id.clone(), text: "Hello".into(), request_id: None })
        })
        .await
        .unwrap();
    let before = f.disk_text();
    let id = &delivery.recipient_conversation_id;
    assert_eq!(f.store.mutate(|state| state.save_canvas(id, input(0, "No recipient document"), CanvasSource::User)).await.unwrap_err().status, 403);
    assert_eq!(f.store.mutate(|state| state.replace_canvas_range(id, 1, "old", "new")).await.unwrap_err().status, 403);
    assert_eq!(f.store.mutate(|state| state.restore_canvas(id, 1, 1)).await.unwrap_err().status, 403);
    assert_eq!(f.disk_text(), before);
}

#[tokio::test]
async fn persisted_canvas_bounds_and_history_invariants_fail_closed_without_replacing_state() {
    let f = Fixture::new().await;
    let id = conversation(&f).await;
    f.store.mutate(|state| state.save_canvas(&id, input(0, "Original"), CanvasSource::User)).await.unwrap();
    f.store.mutate(|state| state.save_canvas(&id, input(1, "Current"), CanvasSource::Bot)).await.unwrap();
    let original = f.disk_json();
    for (field, value) in [
        ("revision", json!(0)),
        ("revision", json!(MAX_CANVAS_REVISION + 1)),
        ("title", json!(" ")),
        ("language", json!("Rust")),
        ("content", json!("a\0b")),
        ("redacted", json!(true)),
        ("updatedAt", json!("not-a-time")),
        ("history", json!([])),
    ] {
        let mut disk = original.clone();
        disk["conversations"][0]["canvas"][field] = value;
        let bytes = write_json(&f.state_file(), &disk);
        assert_load_rejected(&f, &bytes).await;
    }
    for field in ["revision", "redacted"] {
        let mut disk = original.clone();
        disk["conversations"][0]["canvas"]["history"][0][field] = if field == "revision" { json!(2) } else { json!(true) };
        let bytes = write_json(&f.state_file(), &disk);
        assert_load_rejected(&f, &bytes).await;
    }
    let mut disk = original.clone();
    disk["conversations"][0]["canvas"]["content"] = json!("x".repeat(MAX_CANVAS_BYTES + 1));
    let bytes = write_json(&f.state_file(), &disk);
    assert_load_rejected(&f, &bytes).await;
    write_json(&f.state_file(), &original);
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.read(|state| state.canvas(&id).unwrap().revision), 2);
}
