//! Per-bot memories, knowledge files, and bot templates.

mod common;

use std::os::unix::fs::PermissionsExt;

use common::Fixture;
use grok_bot_model::*;
use grok_bot_store::knowledge::{MAX_KNOWLEDGE_FILE_BYTES, knowledge_root};
use grok_bot_store::memory::{MAX_MEMORIES_PER_BOT, bot_context_fingerprint, preview_memory_import, split_memory_import};
use grok_bot_store::templates::validate_template;

async fn configure_key(f: &Fixture, api_key: &str) {
    let settings = SettingsInput {
        base_url: "http://127.0.0.1:12345/v1".into(),
        model: "local-test-model".into(),
        workspace: String::new(),
        api_key: Some(api_key.into()),
        clear_api_key: None,
    };
    f.store.mutate(|s| s.update_settings(settings)).await.unwrap();
}

async fn remember(f: &Fixture, texts: &[&str]) -> HttpResult<grok_bot_store::memory::MemoriesAdded> {
    let (bot, texts) = (f.bot_id(), texts.iter().map(|text| text.to_string()).collect());
    f.store.mutate(move |s| s.add_memories(&bot, texts, MemorySource::User, false)).await
}

fn memories(f: &Fixture) -> Vec<Memory> {
    f.store.read(|s| s.memories(&f.bot_id()).unwrap().to_vec())
}

#[tokio::test]
async fn memories_persist_skip_duplicates_and_delete() {
    let f = Fixture::new().await;
    let before = f.disk_text();
    assert!(!before.contains("memories") && !before.contains("knowledge"), "absent fields are not serialized");
    let added = remember(&f, &["  Prefers Korean replies.  ", "Works in UTC+9"]).await.unwrap();
    assert_eq!(added.added.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), ["Prefers Korean replies.", "Works in UTC+9"]);
    assert!(added.added.iter().all(|m| m.source == MemorySource::User && m.created_at == f.iso_now()));
    let again = remember(&f, &["prefers   KOREAN replies."]).await.unwrap();
    assert!(again.added.is_empty());
    assert_eq!(again.existing[0].id, added.added[0].id);
    let revision = f.store.read(|s| s.disk().revision);
    remember(&f, &["works in utc+9"]).await.unwrap();
    assert_eq!(f.store.read(|s| s.disk().revision), revision, "a duplicate-only request writes nothing");

    let reopened = f.reopen().await.unwrap();
    let persisted = reopened.read(|s| s.memories(&f.bot_id()).unwrap().to_vec());
    assert_eq!(persisted, memories(&f));
    drop(reopened);

    let (bot, id) = (f.bot_id(), added.added[0].id.clone());
    let removed = f.store.mutate(move |s| s.delete_memory(&bot, &id, false)).await.unwrap();
    assert_eq!(removed.text, "Prefers Korean replies.");
    let (bot, id) = (f.bot_id(), added.added[0].id.clone());
    let missing = f.store.mutate(move |s| s.delete_memory(&bot, &id, false)).await.unwrap_err();
    assert_eq!((missing.status, missing.message.as_str()), (404, "Memory does not exist for this bot."));
    let (bot, id) = (f.bot_id(), added.added[1].id.clone());
    f.store.mutate(move |s| s.delete_memory(&bot, &id, false)).await.unwrap();
    assert!(!f.disk_text().contains("\"memories\""), "an empty memory list is dropped");
    let unknown = f.store.mutate(|s| s.add_memories("1b4e28ba-2fa1-4d2e-8b3c-9d8e7f6a5b4c", vec!["x".into()], MemorySource::User, false)).await.unwrap_err();
    assert_eq!(unknown.status, 404);
}

#[tokio::test]
async fn memory_limits_and_validation() {
    let f = Fixture::new().await;
    let invalid = remember(&f, &["ok", "   ", "bad\u{7}bell"]).await.unwrap_err();
    assert_eq!(invalid.status, 400);
    assert!(invalid.message.contains("texts.1: Too small") && invalid.message.contains("texts.2: Memories must be nonempty text"), "{}", invalid.message);
    assert!(memories(&f).is_empty(), "invalid batches write nothing");
    let long = "가".repeat(501);
    assert!(remember(&f, &[&long]).await.unwrap_err().message.contains("<=500 characters"));
    assert!(remember(&f, &[]).await.unwrap_err().message.contains("between 1 and 200"));
    remember(&f, &["line one\nline two\twith tab"]).await.unwrap();

    // The serialized set the engine receives is capped at 16 KiB.
    let large: Vec<String> = (0..40).map(|index| format!("{index:03} {}", "x".repeat(480))).collect();
    let large: Vec<&str> = large.iter().map(String::as_str).collect();
    let error = remember(&f, &large).await.unwrap_err();
    assert_eq!(error.status, 413, "{}", error.message);
    assert_eq!(memories(&f).len(), 1);

    let small: Vec<String> = (1..MAX_MEMORIES_PER_BOT).map(|index| format!("m{index}")).collect();
    for chunk in small.chunks(50).take(4) {
        let chunk: Vec<&str> = chunk.iter().map(String::as_str).collect();
        let _ = remember(&f, &chunk).await;
    }
    assert_eq!(memories(&f).len(), MAX_MEMORIES_PER_BOT);
    let full = remember(&f, &["one more"]).await.unwrap_err();
    assert_eq!((full.status, full.message.as_str()), (409, "A bot can remember at most 200 items. Forget some memories before adding more."));
}

#[tokio::test]
async fn memories_are_redacted_in_the_public_snapshot_and_change_the_session_fingerprint() {
    let f = Fixture::new().await;
    configure_key(&f, "MEMORY_KEY_CANARY").await;
    let plain = f.store.read(|s| s.bot(&f.bot_id()).unwrap().clone());
    assert_eq!(bot_context_fingerprint("skills", &plain), "skills", "bots without memories keep their fingerprint");
    remember(&f, &["Token is MEMORY_KEY_CANARY"]).await.unwrap();
    let snapshot = serde_json::to_string(&f.store.snapshot()).unwrap();
    assert!(!snapshot.contains("MEMORY_KEY_CANARY") && snapshot.contains("Token is [redacted]"));
    let bot = f.store.read(|s| s.bot(&f.bot_id()).unwrap().clone());
    let fingerprint = bot_context_fingerprint("skills", &bot);
    assert_ne!(fingerprint, "skills");
    assert_ne!(fingerprint, bot_context_fingerprint("other skills", &bot));
    let mut edited = bot.clone();
    edited.memories.as_mut().unwrap()[0].text.push('!');
    assert_ne!(fingerprint, bot_context_fingerprint("skills", &edited));
}

#[test]
fn memory_import_splits_lists_and_marks_duplicates() {
    let text = "# Memories from another assistant\n\n- Prefers concise answers\n* Lives in Seoul\n  and commutes by subway\n1. Uses Rust daily\n2) Has a cat named 보리\n[ ] Checkbox item\n---\n```\nignored fence marker\n```\nPlain line\n- prefers CONCISE answers\n";
    assert_eq!(
        split_memory_import(text).unwrap(),
        [
            "Prefers concise answers",
            "Lives in Seoul and commutes by subway",
            "Uses Rust daily",
            "Has a cat named 보리",
            "Checkbox item",
            "ignored fence marker",
            "Plain line",
            "prefers CONCISE answers"
        ]
    );
    assert_eq!(split_memory_import(r#"["One", {"text": "Two"}, {"content": " Three "}, ""]"#).unwrap(), ["One", "Two", "Three"]);
    assert_eq!(split_memory_import("[not json\n- item").unwrap(), ["[not json", "item"]);
    let existing = vec![Memory { id: new_id(), text: "Uses Rust daily".into(), source: MemorySource::User, created_at: "2026-01-01T00:00:00.000Z".into() }];
    let preview = preview_memory_import(&format!("- uses rust daily\n- New fact\n- new FACT\n- {}\n- bad\u{1}", "a".repeat(600)), &existing).unwrap();
    assert_eq!(preview.iter().map(|item| item.duplicate).collect::<Vec<_>>(), [true, false, true, false, false]);
    assert!(preview[3].error.as_deref().is_some_and(|error| error.contains("<=500")));
    assert!(preview[4].error.is_some() && preview[1].error.is_none());
    assert_eq!(split_memory_import(&"a".repeat(64 * 1024 + 1)).unwrap_err().status, 413);
    assert_eq!(preview_memory_import(&(0..201).map(|i| format!("- {i}\n")).collect::<String>(), &[]).unwrap_err().status, 413);
}

fn knowledge(name: &str, content: &str) -> KnowledgeInput {
    KnowledgeInput { name: name.into(), content: content.into() }
}

#[tokio::test]
async fn knowledge_files_are_private_verified_and_removed() {
    let f = Fixture::new().await;
    let bot = f.bot_id();
    let file = f.store.add_knowledge(&bot, knowledge("  team-handbook.md ", "# 핸드북\n휴가는 연 15일입니다.\n")).await.unwrap();
    assert_eq!(file.name, "team-handbook.md");
    assert_eq!(file.bytes, "# 핸드북\n휴가는 연 15일입니다.\n".len() as u64);
    let root = knowledge_root(&f.data_dir);
    let path = root.join(&bot).join(format!("{}.txt", file.id));
    let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!((mode(&root), mode(&root.join(&bot)), mode(&path)), (0o700, 0o700, 0o600));
    assert!(!f.disk_text().contains("휴가는"), "contents stay out of state.json");
    let (metadata, content) = f.store.read_knowledge(&bot, &file.id).await.unwrap();
    assert_eq!((metadata, content.as_str()), (file.clone(), "# 핸드북\n휴가는 연 15일입니다.\n"));

    let duplicate = f.store.add_knowledge(&bot, knowledge("TEAM-handbook.md", "x")).await.unwrap_err();
    assert_eq!((duplicate.status, duplicate.message.as_str()), (409, "A knowledge file with this name already exists for the bot."));
    for (input, status) in [
        (knowledge("", "x"), 400),
        (knowledge("a/b.txt", "x"), 400),
        (knowledge("empty.txt", "  \n"), 400),
        (knowledge("nul.txt", "a\0b"), 400),
        (knowledge("big.txt", &"a".repeat(MAX_KNOWLEDGE_FILE_BYTES + 1)), 413),
    ] {
        assert_eq!(f.store.add_knowledge(&bot, input).await.unwrap_err().status, status);
    }
    assert_eq!(f.store.add_knowledge("not-a-uuid", knowledge("x.txt", "x")).await.unwrap_err().status, 404);
    assert_eq!(std::fs::read_dir(root.join(&bot)).unwrap().count(), 1, "rejected uploads leave no contents");

    // Tampered contents are refused rather than returned.
    std::fs::write(&path, "# 핸드북\n휴가는 연 99일입니다.\n").unwrap();
    assert_eq!(f.store.read_knowledge(&bot, &file.id).await.unwrap_err().status, 409);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(f.store.read_knowledge(&bot, &file.id).await.unwrap_err().status, 409);

    let second = f.store.add_knowledge(&bot, knowledge("faq.txt", "Q: A")).await.unwrap();
    // An orphan from an interrupted add and a deleted bot's directory are pruned on open.
    std::fs::write(root.join(&bot).join("orphan.txt"), "x").unwrap();
    std::fs::create_dir(root.join("1b4e28ba-2fa1-4d2e-8b3c-9d8e7f6a5b4c")).unwrap();
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.read(|s| s.knowledge(&bot).unwrap().len()), 2);
    assert!(!root.join(&bot).join("orphan.txt").exists() && !root.join("1b4e28ba-2fa1-4d2e-8b3c-9d8e7f6a5b4c").exists());
    assert_eq!(reopened.read_knowledge(&bot, &second.id).await.unwrap().1, "Q: A");
    drop(reopened);

    let removed = f.store.delete_knowledge(&bot, &second.id).await.unwrap();
    assert_eq!(removed.id, second.id);
    assert!(!root.join(&bot).join(format!("{}.txt", second.id)).exists());
    assert_eq!(f.store.delete_knowledge(&bot, &second.id).await.unwrap_err().status, 404);
    f.store.delete_knowledge(&bot, &file.id).await.unwrap();
    assert!(!f.disk_text().contains("\"knowledge\""));
}

#[tokio::test]
async fn knowledge_count_and_size_limits() {
    let f = Fixture::new().await;
    let bot = f.bot_id();
    let large = "a".repeat(MAX_KNOWLEDGE_FILE_BYTES);
    for index in 0..16 {
        f.store.add_knowledge(&bot, knowledge(&format!("part-{index}.txt"), &large)).await.unwrap();
    }
    let full = f.store.add_knowledge(&bot, knowledge("one-byte.txt", "x")).await.unwrap_err();
    assert_eq!((full.status, full.message.as_str()), (413, "Knowledge files are limited to 8 MiB per bot. Delete a file before adding more."));
    let other = f.store.mutate(|s| s.add_bot(BotInput { name: "Other".into(), color: None, instructions: None, shape: None, pinned: None })).await.unwrap();
    for index in 0..50 {
        f.store.add_knowledge(&other.id, knowledge(&format!("small-{index}.txt"), "x")).await.unwrap();
    }
    let count = f.store.add_knowledge(&other.id, knowledge("one-more.txt", "x")).await.unwrap_err();
    assert_eq!((count.status, count.message.as_str()), (409, "A bot can hold at most 50 knowledge files. Delete one before adding more."));
    assert_eq!(std::fs::read_dir(knowledge_root(&f.data_dir).join(&other.id)).unwrap().count(), 50);
    assert_eq!(f.store.read(|s| s.knowledge(&bot).unwrap().len()), 16, "limits are per bot");
}

#[tokio::test]
async fn tampered_metadata_fails_to_load() {
    let f = Fixture::new().await;
    f.store.add_knowledge(&f.bot_id(), knowledge("a.txt", "a")).await.unwrap();
    f.store.flush().await.unwrap();
    let mut disk = f.disk_json();
    disk["bots"][0]["knowledge"][0]["hash"] = "not-a-hash".into();
    std::fs::write(f.state_file(), disk.to_string()).unwrap();
    assert!(f.reopen().await.is_err());
    disk["bots"][0]["knowledge"] = serde_json::json!([]);
    disk["bots"][0]["memories"] = serde_json::json!([{ "id": "bad", "text": "x", "source": "user", "createdAt": "2026-01-01T00:00:00.000Z" }]);
    std::fs::write(f.state_file(), disk.to_string()).unwrap();
    assert!(f.reopen().await.is_err());
    disk["bots"][0]["memories"][0]["id"] = new_id().into();
    std::fs::write(f.state_file(), disk.to_string()).unwrap();
    assert!(f.reopen().await.is_ok());
}

fn skill(name: &str) -> SkillInput {
    SkillInput { name: name.into(), description: format!("Use for {name}."), instructions: format!("{name} instructions") }
}

#[tokio::test]
async fn templates_round_trip_with_fresh_ids_and_redaction() {
    let f = Fixture::new().await;
    configure_key(&f, "TEMPLATE_KEY_CANARY").await;
    let bot = f.bot_id();
    let patch = BotPatch { instructions: Some("Answer as a reviewer. TEMPLATE_KEY_CANARY".into()), shape: Some(AvatarShape::Star), color: Some("#22c55e".into()), ..BotPatch::default() };
    f.store.mutate(|s| s.update_bot(&f.bot_id(), patch)).await.unwrap();
    let first = f.store.mutate(|s| s.add_skill(&f.bot_id(), skill("review"), false)).await.unwrap();
    f.store.mutate(|s| s.add_skill(&f.bot_id(), skill("disabled"), false)).await.unwrap();
    let disabled = f.store.read(|s| s.bot(&f.bot_id()).unwrap().skills.clone().unwrap()[1].clone());
    let patch = SkillPatch { name: None, description: None, instructions: None, revision: disabled.revision, enabled: Some(false) };
    f.store.mutate(|s| s.update_skill(&f.bot_id(), &disabled.id, patch)).await.unwrap();
    remember(&f, &["Remembered fact"]).await.unwrap();
    f.store.add_knowledge(&bot, knowledge("kb.txt", "knowledge body")).await.unwrap();

    let template = f.store.read(|s| s.export_template(&bot, false)).unwrap();
    let text = serde_json::to_string(&template).unwrap();
    assert!(!text.contains("TEMPLATE_KEY_CANARY") && text.contains("[redacted]"));
    assert!(!text.contains(&bot) && !text.contains(&first.id) && !text.contains("createdAt") && !text.contains("revision"), "{text}");
    assert!(!text.contains("memories") && !text.contains("knowledge"), "{text}");
    assert_eq!(template.skills.iter().map(|s| (s.name.as_str(), s.enabled)).collect::<Vec<_>>(), [("review", true), ("disabled", false)]);
    let with_memories = f.store.read(|s| s.export_template(&bot, true)).unwrap();
    assert_eq!(with_memories.memories, [TemplateMemory { text: "Remembered fact".into() }]);

    // A file written by `JSON.stringify` imports into a new bot with new IDs.
    let parsed: BotTemplate = serde_json::from_str(&serde_json::to_string(&with_memories).unwrap()).unwrap();
    let created = f.store.mutate(|s| s.add_bot_from_template(parsed)).await.unwrap();
    assert_ne!(created.id, bot);
    assert_eq!((created.name.as_str(), created.color.as_str(), created.shape), ("Grok", "#22c55e", Some(AvatarShape::Star)));
    assert_eq!(created.instructions, "Answer as a reviewer. [redacted]");
    let skills = created.skills.clone().unwrap();
    assert!(skills.iter().all(|s| s.id != first.id && s.id != disabled.id && s.revision == 1));
    assert_eq!(skills.iter().map(|s| s.enabled).collect::<Vec<_>>(), [true, false]);
    let memories = created.memories.clone().unwrap();
    assert_eq!((memories[0].text.as_str(), memories[0].source), ("Remembered fact", MemorySource::Import));
    assert!(created.knowledge.is_none() && created.pinned.is_none());
    assert_eq!(f.store.read(|s| s.disk().bots.len()), 2);
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.read(|s| s.bot(&created.id).unwrap().clone()), created);
}

#[tokio::test]
async fn template_import_is_strict() {
    let f = Fixture::new().await;
    let valid = serde_json::json!({
        "format": "grok-bot.bot-template", "version": 1,
        "bot": { "name": "Reviewer", "color": "#8b5cf6", "instructions": "" },
        "skills": [{ "name": "a", "description": "d", "instructions": "i", "enabled": true }],
    });
    let parse = |value: serde_json::Value| serde_json::from_value::<BotTemplate>(value);
    assert!(parse(valid.clone()).is_ok());
    for (pointer, value) in [("/id", serde_json::json!("x")), ("/bot/id", serde_json::json!("x")), ("/skills/0/id", serde_json::json!("x")), ("/bot/shape", serde_json::json!("triangle"))] {
        let mut invalid = valid.clone();
        let (parent, key) = pointer.rsplit_once('/').unwrap();
        invalid.pointer_mut(if parent.is_empty() { "" } else { parent }).unwrap()[key] = value;
        assert!(parse(invalid).is_err(), "{pointer}");
    }
    let mut missing = valid.clone();
    missing["bot"].as_object_mut().unwrap().remove("color");
    assert!(parse(missing).is_err());

    let attempt = |change: &dyn Fn(&mut serde_json::Value)| {
        let mut value = valid.clone();
        change(&mut value);
        validate_template(parse(value).unwrap()).map(|_| ())
    };
    assert_eq!(attempt(&|v| v["format"] = "other".into()).unwrap_err().message, "This file is not a Grok Bot template.");
    assert_eq!(attempt(&|v| v["version"] = 2.into()).unwrap_err().message, "This template version is not supported.");
    assert!(attempt(&|v| v["bot"]["color"] = "red".into()).unwrap_err().message.starts_with("bot.color:"));
    assert!(attempt(&|v| v["bot"]["name"] = " ".into()).unwrap_err().message.starts_with("bot.name:"));

    let import = |change: &dyn Fn(&mut serde_json::Value)| {
        let mut value = valid.clone();
        change(&mut value);
        let template = parse(value).unwrap();
        async { f.store.mutate(|s| s.add_bot_from_template(template)).await }
    };
    let error = import(&|v| v["skills"][0]["instructions"] = "".into()).await.unwrap_err();
    assert!(error.message.starts_with("skills.0.instructions:"), "{}", error.message);
    let error = import(&|v| v["skills"] = serde_json::json!([{ "name": "a", "description": "d", "instructions": "i", "enabled": true }, { "name": "A", "description": "d", "instructions": "i", "enabled": false }])).await.unwrap_err();
    assert_eq!(error.status, 409);
    let error = import(&|v| v["memories"] = serde_json::json!([{ "text": "" }])).await.unwrap_err();
    assert!(error.message.starts_with("memories.0.text:"), "{}", error.message);
    assert_eq!(f.store.read(|s| s.disk().bots.len()), 1, "rejected templates create nothing");
    let created = import(&|v| v["memories"] = serde_json::json!([{ "text": "fact" }, { "text": "FACT" }])).await.unwrap();
    assert_eq!(created.memories.unwrap().len(), 1, "duplicate memories collapse");
}
