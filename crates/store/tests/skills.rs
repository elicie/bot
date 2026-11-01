//! Store-level port of `tests/skills.test.ts` and the `server/skills.ts` rules.

mod common;

use common::Fixture;
use grok_bot_model::*;
use grok_bot_store::skills::{import_skill_markdown, skill_fingerprint, skill_response_text, validate_skill_input};

fn input() -> SkillInput {
    SkillInput {
        name: "plan-review".into(),
        description: "Use when reviewing the local test plan.".into(),
        instructions: "SKILL_BODY_CANARY\nReview every item and report remaining gaps.".into(),
    }
}

fn patch(revision: u64) -> SkillPatch {
    SkillPatch { name: None, description: None, instructions: None, revision, enabled: None }
}

async fn configure(f: &Fixture, api_key: &str) {
    let settings = SettingsInput {
        base_url: "http://127.0.0.1:12345/v1".into(),
        model: "local-test-model".into(),
        workspace: String::new(),
        api_key: Some(api_key.into()),
        clear_api_key: None,
    };
    f.store.mutate(|s| s.update_settings(settings)).await.unwrap();
}

#[tokio::test]
async fn private_skills_persist_per_bot_with_optimistic_revisions_and_no_partial_updates() {
    let f = Fixture::new().await;
    let bot = f.bot_id();
    let second = f
        .store
        .mutate(|s| s.add_bot(BotInput { name: "Other".into(), color: Some("#ff2543".into()), instructions: None, shape: None, pinned: None }))
        .await
        .unwrap();
    let skill = f.store.mutate(|s| s.add_skill(&bot, input(), false)).await.unwrap();
    assert_eq!(
        (skill.name.as_str(), skill.description.as_str(), skill.instructions.as_str()),
        (input().name.as_str(), input().description.as_str(), input().instructions.as_str())
    );
    assert!(skill.enabled);
    assert_eq!(skill.revision, 1);
    let duplicate = f.store.mutate(|s| s.add_skill(&bot, SkillInput { name: "PLAN-review".into(), ..input() }, false)).await.unwrap_err();
    assert_eq!((duplicate.status, duplicate.message.as_str()), (409, "A skill with this name already exists for the bot."));
    let foreign = f.store.mutate(|s| s.update_skill(&second.id, &skill.id, SkillPatch { enabled: Some(false), ..patch(1) })).await.unwrap_err();
    assert_eq!((foreign.status, foreign.message.as_str()), (404, "Private skill does not exist for this bot."));
    f.store.mutate(|s| s.update_skill(&bot, &skill.id, SkillPatch { enabled: Some(false), ..patch(1) })).await.unwrap();
    assert_eq!(f.store.read(|s| s.skill(&bot, &skill.id).unwrap().revision), 2);
    let before = f.disk_text();
    let stale = f.store.mutate(|s| s.update_skill(&bot, &skill.id, SkillPatch { name: Some("lost-update".into()), ..patch(1) })).await.unwrap_err();
    assert_eq!((stale.status, stale.message.as_str()), (409, "The skill changed since it was opened. Reload its latest version before saving."));
    let stale = f.store.mutate(|s| s.delete_skill(&bot, &skill.id, 1)).await.unwrap_err();
    assert_eq!((stale.status, stale.message.as_str()), (409, "The skill changed since it was opened. Reload before deleting."));
    let empty = f.store.mutate(|s| s.update_skill(&bot, &skill.id, patch(2))).await.unwrap_err();
    assert_eq!((empty.status, empty.message.as_str()), (400, "Provide a skill field to update."));
    assert_eq!(f.store.mutate(|s| s.delete_skill(&bot, &skill.id, 0)).await.unwrap_err().message, "revision: Too small: expected number to be >=1");
    assert_eq!(f.disk_text(), before);
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.read(|s| s.bot(&bot).unwrap().skills.clone()), f.store.read(|s| s.bot(&bot).unwrap().skills.clone()));
    f.store.mutate(|s| s.delete_skill(&bot, &skill.id, 2)).await.unwrap();
    assert_eq!(f.store.read(|s| s.bot(&bot).unwrap().skills.clone()), Some(vec![]));
    let blank = validate_skill_input(SkillInput { instructions: " ".into(), ..input() }).unwrap_err();
    assert_eq!(blank.message, "instructions: Provide nonempty skill instructions without NUL bytes.");
}

#[tokio::test]
async fn skill_writes_require_an_idle_store_except_model_proposals_within_a_run() {
    let f = Fixture::new().await;
    let bot = f.bot_id();
    let conversation = f.store.mutate(|s| s.add_conversation(&bot)).await.unwrap();
    f.start(&conversation.id).await;
    let racing = f.store.mutate(|s| s.add_skill(&bot, SkillInput { name: "racing".into(), ..input() }, false)).await.unwrap_err();
    assert_eq!(racing.status, 409);
    let proposed = f.store.mutate(|s| s.add_skill(&bot, input(), true)).await.unwrap();
    assert_eq!(proposed.name, input().name);
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.read(|s| s.bot(&bot).unwrap().skills.clone()), Some(vec![proposed]));
}

#[test]
fn skill_input_validation_matches_zod_messages() {
    let error = |input: SkillInput| validate_skill_input(input).unwrap_err().message;
    assert_eq!(error(SkillInput { name: String::new(), ..input() }), "name: Too small: expected string to have >=1 characters");
    assert_eq!(error(SkillInput { name: "a\u{1}".into(), ..input() }), "name: Skill names cannot contain control characters.");
    assert_eq!(error(SkillInput { description: " ".into(), ..input() }), "description: Too small: expected string to have >=1 characters");
    assert_eq!(
        error(SkillInput { instructions: String::new(), ..input() }),
        "instructions: Too small: expected string to have >=1 characters; instructions: Provide nonempty skill instructions without NUL bytes."
    );
    assert_eq!(
        error(SkillInput { instructions: "가".repeat(10000), ..input() }),
        "Request: Skill text exceeds the 18 KiB serialized limit. Shorten it so the engine receives the complete instructions."
    );
    assert_eq!(
        error(SkillInput { name: String::new(), instructions: "가".repeat(10000), ..input() }),
        "name: Too small: expected string to have >=1 characters; Request: Skill text exceeds the 18 KiB serialized limit. Shorten it so the engine receives the complete instructions."
    );
    let trimmed = validate_skill_input(SkillInput { name: " a\n".into(), description: " d ".into(), instructions: " i ".into() }).unwrap();
    assert_eq!((trimmed.name.as_str(), trimmed.description.as_str(), trimmed.instructions.as_str()), ("a", "d", " i "));
}

#[tokio::test]
async fn skill_md_import_validates_metadata_rejects_tool_or_yaml_escape_fields_and_preserves_text() {
    let f = Fixture::new().await;
    let bot = f.bot_id();
    let markdown = format!("---\nname: imported-skill\ndescription: Use for the fixture.\n---\n{}\nCOMPLETE_TAIL_SENTINEL", "가".repeat(5000));
    let imported = import_skill_markdown(&markdown).unwrap();
    let skill = f.store.mutate(|s| s.add_skill(&bot, imported, false)).await.unwrap();
    assert_eq!(skill.name, "imported-skill");
    assert!(skill.instructions.ends_with("COMPLETE_TAIL_SENTINEL"));
    let unique = "SKILL.md frontmatter must contain valid YAML with unique fields.";
    let unsupported = "SKILL.md supports only name (lowercase letters, digits, hyphens; 64 characters) and description (1024 characters). Tool, script, model, and hook fields are unsupported.";
    let aliases = "SKILL.md YAML aliases and custom values are unsupported.";
    let frontmatter = "SKILL.md requires a YAML frontmatter block of at most 4096 bytes.";
    for (invalid, message) in [
        ("---\nname: imported-skill\nname: duplicate\ndescription: x\n---\nbody", unique),
        ("---\nname: a\n\"name\": b\ndescription: c\n---\nbody", unique),
        ("---\nname: unsafe\ndescription: x\nallowed-tools: Bash\n---\nbody", unsupported),
        ("---\nname: unsafe\ndescription: !!js/function x\n---\nbody", unique),
        ("---\nname: unsafe\ndescription: !custom x\n---\nbody", unique),
        ("---\nname: unsafe\ndescription: !!int abc\n---\nbody", unique),
        ("---\nname: unsafe\ndescription: &x x\nextra: *x\n---\nbody", aliases),
        ("---\nname: ../../path\ndescription: x\n---\nbody", unsupported),
        ("---\nname: unsafe\ndescription: 123\n---\nbody", unsupported),
        ("---\nname: unsafe\ndescription: ~\n---\nbody", unsupported),
        ("---\nname: unsafe\ndescription: [a]\n---\nbody", unsupported),
        ("---\nname: unsafe\ndescription: b\n--- \nc: d\n---\nbody", unique),
        ("---\n\tname: unsafe\n---\nbody", unique),
        ("---\n\n---\nbody", unsupported),
        ("---\n---\nbody", frontmatter),
        ("---\nname: a\ndescription: *missing\n---\nbody", aliases),
        ("---\nname: 123\ndescription: b\n---\nbody", unsupported),
        (
            "---\nname: a\ndescription: b\n---\n",
            "instructions: Too small: expected string to have >=1 characters; instructions: Provide nonempty skill instructions without NUL bytes.",
        ),
        ("# Missing frontmatter", frontmatter),
    ] {
        let error = import_skill_markdown(invalid).unwrap_err();
        assert_eq!((error.status, error.message.as_str()), (400, message), "{invalid:?}");
    }
    let accepted = [
        ("\u{feff}---\r\nname: crlf\r\ndescription: Windows lines.\r\n---\r\nBody", "crlf", "Windows lines.", "Body"),
        ("---\nname: quoted\ndescription: \"  spaced  \"\n---\nBody\n", "quoted", "spaced", "Body\n"),
        ("---\nname: block\ndescription: |\n  multi\n  line\n---\nBody", "block", "multi\nline", "Body"),
        ("---\nname: tagged\ndescription: !!str 2026-01-01\n---\nBody", "tagged", "2026-01-01", "Body"),
        ("---\nname: flow\ndescription: plain\n  continued # comment\n---\nBody", "flow", "plain continued", "Body"),
        ("---\n{name: braces, description: yes}\n---\nBody", "braces", "yes", "Body"),
        ("---\nname: a\ndescription: b\n...\n---\nbody", "a", "b", "body"),
    ];
    for (markdown, name, description, body) in accepted {
        let skill = import_skill_markdown(markdown).unwrap_or_else(|error| panic!("{markdown:?}: {error}"));
        assert_eq!((skill.name.as_str(), skill.description.as_str(), skill.instructions.as_str()), (name, description, body));
    }
    let large = import_skill_markdown(&format!("---\nname: a\ndescription: b\n---\n{}", "x".repeat(21 * 1024))).unwrap_err();
    assert_eq!(large.message, "markdown: Too big: expected string to have <=20480 characters");
    let nul = import_skill_markdown("---\nname: a\ndescription: b\n---\n\0").unwrap_err();
    assert_eq!((nul.status, nul.message.as_str()), (413, "SKILL.md must be UTF-8 text of at most 20 KiB without NUL bytes."));
    let wide = import_skill_markdown(&format!("---\nname: a\ndescription: b\n---\n{}", "가".repeat(7000))).unwrap_err();
    assert_eq!(wide.status, 413);
    let before = f.disk_text();
    let too_long =
        f.store.mutate(|s| s.add_skill(&bot, SkillInput { name: "too-long".into(), instructions: "가".repeat(10000), ..input() }, false)).await.unwrap_err();
    assert_eq!(too_long.status, 400);
    assert_eq!(f.disk_text(), before);
}

#[tokio::test]
async fn skill_metadata_redacts_json_special_credentials_and_preserves_structural_ids() {
    let f = Fixture::new().await;
    let bot = f.bot_id();
    for api_key in ["key\"with\\escapes", "id"] {
        configure(&f, api_key).await;
        let skill = f
            .store
            .mutate(|s| {
                s.add_skill(
                    &bot,
                    SkillInput {
                        name: format!("redaction-{}", api_key.len()),
                        description: format!("Use safely {api_key}"),
                        instructions: "Complete instructions.".into(),
                    },
                    false,
                )
            })
            .await
            .unwrap();
        let snapshot = f.store.snapshot();
        let public = snapshot.bots[0].skills.as_ref().unwrap().iter().find(|item| item.id == skill.id).unwrap();
        assert_eq!(public.description, "Use safely [redacted]");
        assert_eq!(snapshot.bots[0].id, bot);
        if api_key != "id" {
            let escaped = serde_json::to_string(api_key).unwrap();
            assert!(!serde_json::to_string(&snapshot).unwrap().contains(&escaped[1..escaped.len() - 1]));
        }
        assert_eq!(f.store.read(|s| s.redact(&format!("x{api_key}y"))), "x[redacted]y");
    }
}

#[test]
fn skill_response_text_escapes_data_uris_and_fingerprints_enabled_skills() {
    let value = serde_json::json!({ "text": "Literal PDF: data:application/pdf;base64,SGVsbG8= DATA:x Data:y" });
    let text = skill_response_text(&value);
    let escaped = format!("data{}u003a", '\\');
    assert_eq!(text, format!(r#"{{"text":"Literal PDF: {escaped}application/pdf;base64,SGVsbG8= DATA{}x Data{}y"}}"#, &escaped[4..], &escaped[4..]));
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap(), value);
    let skill = Skill {
        id: "6f1c2b9e-8a71-4d0c-9f43-2a1b3c4d5e6f".into(),
        name: "plan-review".into(),
        description: "d".into(),
        instructions: "i".into(),
        enabled: true,
        revision: 3,
        created_at: "2026-01-01T00:00:00.000Z".into(),
        updated_at: "2026-01-01T00:00:00.000Z".into(),
    };
    // sha256(JSON.stringify([{ id, revision, name, description, instructions }])) computed by Node.
    assert_eq!(skill_fingerprint(std::slice::from_ref(&skill)), "2831d98adeaf2ba94e872269e8f0d6a3d975365a0aa6871c2f10ddacfb5c4082");
    let disabled = Skill { enabled: false, ..skill };
    assert_eq!(skill_fingerprint(&[disabled]), skill_fingerprint(&[]));
}

#[tokio::test]
async fn skill_capacity_checks_size_prospective_state_exactly_and_preserve_queued_dispatch_reservations() {
    let f = Fixture::new().await;
    let bot = f.bot_id();
    let conversation = f.store.mutate(|s| s.add_conversation(&bot)).await.unwrap();
    let limit = 64 * 1024 * 1024;
    let now = f.iso_now();
    let id = conversation.id.clone();
    // Fill the conversation so the serialized state leaves `remaining` bytes below the limit.
    let fill = |s: &mut grok_bot_store::State, remaining: usize| {
        let c = s.conversation_mut(&id).unwrap();
        c.messages = vec![Message {
            id: new_id(),
            role: Role::User,
            text: String::new(),
            created_at: now.clone(),
            tools: vec![],
            bot_exchange_ids: None,
            source_bot_id: None,
            speaker_bot_id: None,
        }];
        let current = serde_json::to_vec(s.disk()).unwrap().len();
        s.disk_mut().conversations[0].messages[0].text = "p".repeat(limit - remaining - current);
    };
    let skill_bytes = serde_json::to_vec(&serde_json::json!({
        "name": input().name, "description": input().description, "instructions": input().instructions,
        "id": new_id(), "enabled": true, "revision": 1, "createdAt": f.iso_now(), "updatedAt": f.iso_now(),
    }))
    .unwrap()
    .len();
    let error = f
        .store
        .mutate(|s| {
            fill(s, skill_bytes + 2);
            s.add_skill(&bot, input(), false)
        })
        .await
        .unwrap_err();
    assert_eq!(error.status, 413);
    assert!(error.message.contains("reserved queued work"));
    assert_eq!(f.store.read(|s| s.bot(&bot).unwrap().skills.clone()), None);
    f.store.read(|s| s.assert_writable()).unwrap();
    f.store
        .mutate(|s| {
            s.conversation_mut(&id)?.messages.clear();
            Ok(())
        })
        .await
        .unwrap();
    let skill = f.store.mutate(|s| s.add_skill(&bot, SkillInput { instructions: "i".repeat(10000), ..input() }, false)).await.unwrap();
    f.store
        .mutate(|s| {
            fill(s, 5);
            s.update_skill(&bot, &skill.id, SkillPatch { instructions: Some("Shortened.".into()), ..patch(1) })
        })
        .await
        .unwrap();
    assert_eq!(f.store.read(|s| s.skill(&bot, &skill.id).unwrap().instructions.clone()), "Shortened.");
    f.store
        .mutate(|s| {
            s.conversation_mut(&id)?.messages.clear();
            Ok(())
        })
        .await
        .unwrap();
    let routine = f
        .store
        .mutate(|s| {
            s.add_routine(RoutineInput {
                bot_id: bot.clone(),
                name: "Reserved".into(),
                prompt: "q".repeat(10000),
                enabled: false,
                schedule: RoutineSchedule::Interval { minutes: 5 },
            })
        })
        .await
        .unwrap();
    f.store.mutate(|s| s.run_routine(&routine.id, f.now())).await.unwrap();
    let error = f
        .store
        .mutate(|s| {
            fill(s, 16000);
            s.add_skill(&bot, SkillInput { name: "reserve-intruder".into(), instructions: "i".repeat(8000), ..input() }, false)
        })
        .await
        .unwrap_err();
    assert!(error.message.contains("reserved queued work"));
    assert_eq!(f.store.read(|s| s.bot(&bot).unwrap().skills.as_ref().unwrap().len()), 1);
    f.store.read(|s| s.assert_writable()).unwrap();
}
