//! Permission rules and routine webhooks at the store level: validation, persistence, deletion,
//! loopback-only loosening, policy lookup, key hashing, and webhook admission.

mod common;

use common::{Fixture, assert_load_rejected, write_json};
use grok_bot_model::*;
use grok_bot_store::permissions::{LOOSEN_REQUIRES_LOOPBACK, MAX_PERMISSION_RULES, PermissionPolicy};
use grok_bot_store::webhooks::{INVALID_WEBHOOK_KEY, MAX_WEBHOOK_PAYLOAD_BYTES, webhook_key_hash, webhook_key_hint};
use serde_json::json;

fn rule(category: PermissionCategory, action: PermissionAction, bot_id: Option<&str>) -> PermissionRuleInput {
    PermissionRuleInput { category, action, bot_id: bot_id.map(str::to_owned), server: None }
}

#[tokio::test]
async fn rules_validate_persist_and_delete() {
    use PermissionAction::*;
    use PermissionCategory::*;
    let f = Fixture::new().await;
    let bot = f.bot_id();
    assert_eq!(f.store.read(|s| s.permission_policy(FileWrite, &bot)), PermissionPolicy::Ask, "no rules: always ask");
    assert!(f.disk_json().get("permissionRules").is_none(), "empty rule lists are not written");

    let bad = f.store.mutate(|s| s.add_permission_rule(rule(FileWrite, Ask, Some("not-a-uuid")), true)).await.unwrap_err();
    assert_eq!((bad.status, bad.message.as_str()), (400, "botId: Invalid uuid"));
    let missing = f.store.mutate(|s| s.add_permission_rule(rule(FileWrite, Ask, Some(&new_id())), true)).await.unwrap_err();
    assert_eq!((missing.status, missing.message.as_str()), (404, "Bot does not exist."));

    let allow = f.store.mutate(|s| s.add_permission_rule(rule(FileWrite, Allow, None), true)).await.unwrap();
    assert_eq!(f.store.read(|s| s.permission_policy(FileWrite, &bot)), PermissionPolicy::Allow { rule_id: allow.id.clone() });
    assert_eq!(f.store.read(|s| s.permission_policy(BotMessage, &bot)), PermissionPolicy::Ask);
    let duplicate = f.store.mutate(|s| s.add_permission_rule(rule(FileWrite, Ask, None), true)).await.unwrap_err();
    assert_eq!(duplicate.status, 409);

    let narrow = f.store.mutate(|s| s.add_permission_rule(rule(FileWrite, Ask, Some(&bot)), true)).await.unwrap();
    assert_eq!(f.store.read(|s| s.permission_policy(FileWrite, &bot)), PermissionPolicy::Ask, "ask wins over allow");
    assert_eq!(f.store.read(|s| s.permission_policy(FileWrite, &new_id())), PermissionPolicy::Allow { rule_id: allow.id.clone() });

    // Persisted, public, and reloaded.
    assert_eq!(f.disk_json()["permissionRules"].as_array().unwrap().len(), 2);
    assert_eq!(f.store.snapshot().permission_rules.len(), 2);
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.read(|s| s.permission_rules().to_vec()), vec![allow.clone(), narrow.clone()]);
    drop(reopened);

    // Editing the action.
    f.set_time("2026-01-02T00:00:00.000Z");
    let edited = f.store.mutate(|s| s.update_permission_rule(&narrow.id, PermissionRulePatch { action: Allow }, true)).await.unwrap();
    assert_eq!((edited.action, edited.updated_at.as_str(), edited.created_at.as_str()), (Allow, "2026-01-02T00:00:00.000Z", narrow.created_at.as_str()));
    assert_eq!(f.store.read(|s| s.permission_policy(FileWrite, &bot)), PermissionPolicy::Allow { rule_id: narrow.id.clone() });
    let gone = f.store.mutate(|s| s.update_permission_rule(&new_id(), PermissionRulePatch { action: Ask }, true)).await.unwrap_err();
    assert_eq!(gone.status, 404);

    // Deleting.
    f.store.mutate(|s| s.delete_permission_rule(&narrow.id, true)).await.unwrap();
    f.store.mutate(|s| s.delete_permission_rule(&allow.id, true)).await.unwrap();
    assert!(f.store.read(|s| s.permission_rules().is_empty()));
    assert!(f.disk_json().get("permissionRules").is_none());
    assert_eq!(f.store.mutate(|s| s.delete_permission_rule(&allow.id, true)).await.unwrap_err().status, 404);
}

#[tokio::test]
async fn only_loopback_clients_can_loosen_rules() {
    use PermissionAction::*;
    use PermissionCategory::*;
    let f = Fixture::new().await;
    let bot = f.bot_id();
    let forbidden = |error: HttpError| assert_eq!((error.status, error.message.as_str()), (403, LOOSEN_REQUIRES_LOOPBACK));
    forbidden(f.store.mutate(|s| s.add_permission_rule(rule(SkillSave, Allow, None), false)).await.unwrap_err());
    assert!(f.store.read(|s| s.permission_rules().is_empty()));
    // Tightening is open to LAN clients.
    let ask = f.store.mutate(|s| s.add_permission_rule(rule(SkillSave, Ask, Some(&bot)), false)).await.unwrap();
    forbidden(f.store.mutate(|s| s.update_permission_rule(&ask.id, PermissionRulePatch { action: Allow }, false)).await.unwrap_err());
    forbidden(f.store.mutate(|s| s.delete_permission_rule(&ask.id, false)).await.unwrap_err());
    let allow = f.store.mutate(|s| s.add_permission_rule(rule(BotCreate, Allow, None), true)).await.unwrap();
    f.store.mutate(|s| s.update_permission_rule(&allow.id, PermissionRulePatch { action: Ask }, false)).await.unwrap();
    f.store.mutate(|s| s.update_permission_rule(&allow.id, PermissionRulePatch { action: Allow }, true)).await.unwrap();
    f.store.mutate(|s| s.delete_permission_rule(&allow.id, false)).await.unwrap();
    assert_eq!(f.store.read(|s| s.permission_rules().to_vec()), vec![ask]);
}

#[tokio::test]
async fn rule_count_is_bounded_and_bot_rules_can_be_removed() {
    let f = Fixture::new().await;
    let mut bots = Vec::new();
    for index in 0..20 {
        bots.push(
            f.store
                .mutate(move |s| s.add_bot(BotInput { name: format!("Bot {index}"), color: None, instructions: None, shape: None, pinned: None }))
                .await
                .unwrap()
                .id,
        );
    }
    for bot in &bots {
        // Five categories per bot fill the 100-rule limit exactly.
        for category in PermissionCategory::ALL.into_iter().take(5) {
            let input = rule(category, PermissionAction::Ask, Some(bot));
            f.store.mutate(move |s| s.add_permission_rule(input, false)).await.unwrap();
        }
    }
    assert_eq!(f.store.read(|s| s.permission_rules().len()), MAX_PERMISSION_RULES);
    let full = f.store.mutate(|s| s.add_permission_rule(rule(PermissionCategory::FileWrite, PermissionAction::Ask, None), true)).await.unwrap_err();
    assert_eq!(full.status, 409);
    let first = bots[0].clone();
    f.store
        .mutate(move |s| {
            s.remove_permission_rules_for_bot(&first);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(f.store.read(|s| s.permission_rules().len()), MAX_PERMISSION_RULES - 5);
}

#[tokio::test]
async fn invalid_persisted_rules_are_rejected() {
    let f = Fixture::new().await;
    f.store.mutate(|s| s.add_permission_rule(rule(PermissionCategory::FileWrite, PermissionAction::Allow, None), true)).await.unwrap();
    let valid = f.disk_json();
    let mut duplicate_scope = valid.clone();
    let copy =
        json!({ "id": new_id(), "category": "file_write", "action": "ask", "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z" });
    duplicate_scope["permissionRules"].as_array_mut().unwrap().push(copy);
    let bytes = write_json(&f.state_file(), &duplicate_scope);
    assert_load_rejected(&f, &bytes).await;
    let mut unknown = valid.clone();
    unknown["permissionRules"][0]["category"] = json!("shell");
    let bytes = write_json(&f.state_file(), &unknown);
    assert_load_rejected(&f, &bytes).await;
    let mut reused_id = valid.clone();
    reused_id["permissionRules"][0]["id"] = valid["bots"][0]["id"].clone();
    let bytes = write_json(&f.state_file(), &reused_id);
    assert_load_rejected(&f, &bytes).await;
    write_json(&f.state_file(), &valid);
    assert_eq!(f.reopen().await.unwrap().read(|s| s.permission_rules().len()), 1);
}

#[tokio::test]
async fn webhook_keys_are_hashed_rotated_revoked_and_claim_through_admission() {
    let f = Fixture::new().await;
    let routine = f.store.mutate(|s| s.add_routine(f.input(RoutineSchedule::Interval { minutes: 60 }))).await.unwrap();
    let id = routine.id.clone();
    let key = "gbw_first-secret-key-0001";
    let (hash, hint) = (webhook_key_hash(key), webhook_key_hint(key));
    let unauthorized = |error: HttpError| assert_eq!((error.status, error.message.as_str()), (401, INVALID_WEBHOOK_KEY));
    unauthorized(f.store.mutate(|s| s.claim_webhook_routine(&id, key, None, f.now())).await.unwrap_err());
    unauthorized(f.store.mutate(|s| s.claim_webhook_routine(&new_id(), key, None, f.now())).await.unwrap_err());

    f.store.mutate(|s| s.set_routine_webhook(&id, hash.clone(), hint.clone())).await.unwrap();
    let disk = f.disk_json();
    assert_eq!(disk["routines"][0]["webhook"]["keyHash"], hash.as_str());
    assert!(!f.disk_text().contains(key), "the key itself is never stored");
    let public = serde_json::to_value(f.store.snapshot()).unwrap();
    assert_eq!(public["routines"][0]["webhook"], json!({ "keyHint": "0001", "createdAt": "2026-01-01T00:00:00.000Z" }));
    assert!(!public.to_string().contains(&hash));
    // Editing the routine keeps its key.
    f.store.mutate(|s| s.update_routine(&id, RoutineInput { name: "Renamed".into(), ..f.input(RoutineSchedule::Interval { minutes: 30 }) })).await.unwrap();
    assert_eq!(f.routine(&id).webhook.unwrap().key_hash, hash);

    unauthorized(f.store.mutate(|s| s.claim_webhook_routine(&id, "gbw_wrong", None, f.now())).await.unwrap_err());
    let large = "x".repeat(MAX_WEBHOOK_PAYLOAD_BYTES + 1);
    assert_eq!(f.store.mutate(|s| s.claim_webhook_routine(&id, key, Some(&large), f.now())).await.unwrap_err().status, 413);
    assert!(f.runs().is_empty());

    let run = f.store.mutate(|s| s.claim_webhook_routine(&id, key, Some("{\"ticket\":42}"), f.now())).await.unwrap();
    assert_eq!(run.trigger, RoutineTrigger::Webhook);
    assert!(run.prompt.starts_with("Routine prompt\n\n") && run.prompt.contains("<webhook_payload>\n{\"ticket\":42}\n</webhook_payload>"), "{}", run.prompt);
    assert_eq!(f.routine(&id).prompt, "Routine prompt", "the routine prompt itself is unchanged");
    assert!(f.store.read(|s| s.is_webhook_conversation(&run.conversation_id)));
    assert_eq!(serde_json::to_value(&run).unwrap()["trigger"], "webhook");
    // One queued or active run per routine.
    let busy = f.store.mutate(|s| s.claim_webhook_routine(&id, key, None, f.now())).await.unwrap_err();
    assert_eq!(busy.status, 409);
    assert_eq!(f.store.mutate(|s| s.run_routine(&id, f.now())).await.unwrap_err().status, 409);
    f.start(&run.conversation_id).await;
    f.finish(&run.conversation_id).await;

    // Rotation invalidates the old key; revocation invalidates every key.
    let next = "gbw_second-secret-key-0002";
    f.store.mutate(|s| s.set_routine_webhook(&id, webhook_key_hash(next), webhook_key_hint(next))).await.unwrap();
    unauthorized(f.store.mutate(|s| s.claim_webhook_routine(&id, key, None, f.now())).await.unwrap_err());
    let second = f.store.mutate(|s| s.claim_webhook_routine(&id, next, None, f.now())).await.unwrap();
    assert_eq!(second.prompt, "Routine prompt", "no payload, no data block");
    f.start(&second.conversation_id).await;
    f.finish(&second.conversation_id).await;
    f.store.mutate(|s| s.clear_routine_webhook(&id)).await.unwrap();
    unauthorized(f.store.mutate(|s| s.claim_webhook_routine(&id, next, None, f.now())).await.unwrap_err());
    assert!(f.disk_json()["routines"][0].get("webhook").is_none());
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.read(|s| s.disk().routine_runs.iter().filter(|r| r.trigger == RoutineTrigger::Webhook).count()), 2);
}

#[tokio::test]
async fn webhook_runs_respect_schedules_and_prompt_limits() {
    let f = Fixture::new().await;
    let at = "2026-01-05T00:00:00.000Z";
    let mut input = f.input(RoutineSchedule::Once { at: at.into() });
    input.prompt = "p".repeat(99_000);
    let routine = f.store.mutate(|s| s.add_routine(input)).await.unwrap();
    let key = "gbw_limit-key-1234";
    f.store.mutate(|s| s.set_routine_webhook(&routine.id, webhook_key_hash(key), webhook_key_hint(key))).await.unwrap();
    let payload = "d".repeat(2000);
    let too_long = f.store.mutate(|s| s.claim_webhook_routine(&routine.id, key, Some(&payload), f.now())).await.unwrap_err();
    assert_eq!(too_long.status, 413, "{too_long:?}");
    let run = f.store.mutate(|s| s.claim_webhook_routine(&routine.id, key, Some("short"), f.now())).await.unwrap();
    // A webhook run does not consume the one-time schedule (a manual run would).
    let after = f.routine(&routine.id);
    assert!(after.enabled);
    assert_eq!(after.next_run_at.as_deref(), Some(at));
    // A paused routine still accepts webhooks: pausing only stops scheduled runs.
    f.start(&run.conversation_id).await;
    f.finish(&run.conversation_id).await;
    f.store.mutate(|s| s.set_routine_enabled(&routine.id, false)).await.unwrap();
    f.store.mutate(|s| s.claim_webhook_routine(&routine.id, key, None, f.now())).await.unwrap();
    let invalid = f.store.mutate(|s| s.set_routine_webhook(&routine.id, "short".into(), "x".into())).await.unwrap_err();
    assert_eq!(invalid.status, 400);
    let mut corrupt = f.disk_json();
    corrupt["routines"][0]["webhook"]["keyHash"] = json!("not-a-hash");
    let bytes = write_json(&f.state_file(), &corrupt);
    assert_load_rejected(&f, &bytes).await;
}
