//! Issue #20: Bot-owned mutations wait only for their owner; shared resources still wait for all.

mod common;

use common::Fixture;
use grok_bot_model::*;

async fn configured() -> Fixture {
    let f = Fixture::new().await;
    f.store
        .mutate(|s| {
            s.update_settings(SettingsInput {
                base_url: "http://localhost:8000/v1".into(),
                model: "test-model".into(),
                workspace: String::new(),
                api_key: None,
                clear_api_key: None,
                max_concurrent_runs: None,
            })
        })
        .await
        .unwrap();
    f
}

async fn add_bot(f: &Fixture, name: &str) -> String {
    f.store.mutate(|s| s.add_bot(BotInput { name: name.into(), color: None, instructions: None, shape: None, pinned: None })).await.unwrap().id
}

async fn conversation(f: &Fixture, bot: &str) -> String {
    f.store.mutate(|s| s.add_conversation(bot)).await.unwrap().id
}

fn secret(name: &str, bot: Option<&str>) -> SecretInput {
    SecretInput { name: name.into(), value: "guard-test-secret-value".into(), bot_id: bot.map(str::to_owned) }
}

fn knowledge(name: &str) -> KnowledgeInput {
    KnowledgeInput { name: name.into(), content: "Bot-private knowledge".into() }
}

#[tokio::test]
async fn memories_and_knowledge_wait_only_for_their_owner_and_preserve_approved_run_edits() {
    let f = Fixture::new().await;
    let target = f.bot_id();
    let busy = add_bot(&f, "Busy").await;
    let active = conversation(&f, &busy).await;
    let busy_memory = f.store.mutate(|s| s.add_memories(&busy, vec!["Existing memory".into()], MemorySource::User, false)).await.unwrap().added[0].id.clone();
    let busy_file = f.store.add_knowledge(&busy, knowledge("busy.txt")).await.unwrap();
    f.start(&active).await;

    let added = f.store.mutate(|s| s.add_memories(&target, vec!["New memory".into()], MemorySource::User, false)).await.unwrap();
    f.store.mutate(|s| s.delete_memory(&target, &added.added[0].id, false)).await.unwrap();
    let file = f.store.add_knowledge(&target, knowledge("idle.txt")).await.unwrap();
    f.store.delete_knowledge(&target, &file.id).await.unwrap();
    assert_eq!(f.conversation(&active).run.status, RunStatus::Running);

    let before = f.disk_text();
    assert_eq!(f.store.mutate(|s| s.add_memories(&busy, vec!["Blocked".into()], MemorySource::User, false)).await.unwrap_err().status, 409);
    assert_eq!(f.store.mutate(|s| s.delete_memory(&busy, &busy_memory, false)).await.unwrap_err().status, 409);
    assert_eq!(f.store.add_knowledge(&busy, knowledge("blocked.txt")).await.unwrap_err().status, 409);
    assert_eq!(f.store.delete_knowledge(&busy, &busy_file.id).await.unwrap_err().status, 409);
    assert_eq!(f.disk_text(), before, "rejected changes never commit partial Bot data");

    let approved = f.store.mutate(|s| s.add_memories(&busy, vec!["Approved in-run memory".into()], MemorySource::Bot, true)).await.unwrap();
    f.store.mutate(|s| s.delete_memory(&busy, &approved.added[0].id, true)).await.unwrap();
    f.finish(&active).await;
    let reopened = f.reopen().await.unwrap();
    reopened.read(|s| {
        assert!(s.memories(&target).unwrap().is_empty());
        assert!(s.knowledge(&target).unwrap().is_empty());
        assert_eq!(s.memories(&busy).unwrap().len(), 1);
        assert_eq!(s.knowledge(&busy).unwrap().len(), 1);
    });
}

#[tokio::test]
async fn mcp_save_form_toggle_and_probe_are_bot_local_in_both_directions() {
    let f = Fixture::new().await;
    let target = f.bot_id();
    let busy = add_bot(&f, "Busy").await;
    let document = r#"{"web":{"url":"https://example.com/mcp"},"off":{"url":"https://off.example/mcp"}}"#;
    for bot in [&target, &busy] {
        f.store.mutate(|s| s.set_mcp_servers(bot, document, true)).await.unwrap();
        f.store.mutate(|s| s.set_mcp_server_enabled(bot, "web", true, true)).await.unwrap();
    }
    let active = conversation(&f, &busy).await;
    f.start(&active).await;

    let edited = document.replace("example.com", "changed.example");
    f.store.mutate(|s| s.set_mcp_servers(&target, &edited, true)).await.unwrap();
    f.store.mutate(|s| s.set_mcp_server_enabled(&target, "web", true, true)).await.unwrap();
    f.store.mutate(|s| s.set_mcp_server_enabled(&target, "web", false, false)).await.unwrap();
    let form = r#"{"web":{"url":"https://example.com/mcp","headers":{"X":"${secret:FORM_TOKEN}"}}}"#;
    // Omitted input scope is converted to this Bot's private scope, not treated as a global write.
    f.store.mutate(|s| s.set_mcp_servers_with_secrets(&target, form, &[secret("FORM_TOKEN", None)], true)).await.unwrap();
    assert_eq!(f.store.snapshot().secrets[0].bot_id.as_deref(), Some(target.as_str()));
    let probe = r#"{"probe":{"url":"https://example.com/mcp","headers":{"X":"${secret:PROBE_TOKEN}"}}}"#;
    let before_probe = f.disk_text();
    f.store.read(|s| s.resolve_mcp_draft(&target, probe, &[secret("PROBE_TOKEN", None)], true)).unwrap();
    assert_eq!(f.disk_text(), before_probe, "a Bot-local probe remains read-only");

    let before = f.disk_text();
    assert_eq!(f.store.mutate(|s| s.set_mcp_servers(&busy, &edited, true)).await.unwrap_err().status, 409);
    assert_eq!(f.store.mutate(|s| s.set_mcp_servers_with_secrets(&busy, form, &[secret("FORM_TOKEN", None)], true)).await.unwrap_err().status, 409);
    assert_eq!(f.store.mutate(|s| s.set_mcp_server_enabled(&busy, "off", true, true)).await.unwrap_err().status, 409);
    assert_eq!(f.store.mutate(|s| s.set_mcp_server_enabled(&busy, "web", false, false)).await.unwrap_err().status, 409);
    assert_eq!(f.store.read(|s| s.resolve_mcp_draft(&busy, probe, &[secret("PROBE_TOKEN", None)], true)).unwrap_err().status, 409);
    assert_eq!(f.disk_text(), before, "failed MCP mutations do not save credentials or toggle capabilities");
    assert_eq!(f.conversation(&active).run.status, RunStatus::Running);
    f.finish(&active).await;
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn private_secret_changes_are_bot_local_but_global_secrets_wait_for_every_active_state() {
    let f = Fixture::new().await;
    let target = f.bot_id();
    let busy = add_bot(&f, "Busy").await;
    let global = f.store.mutate(|s| s.add_secret(secret("SHARED_TOKEN", None), true)).await.unwrap();
    let owned = f.store.mutate(|s| s.add_secret(secret("OWNED_TOKEN", Some(&busy)), true)).await.unwrap();
    let active = conversation(&f, &busy).await;
    f.start(&active).await;

    // Adding/removing a private shadow of a global secret changes only its owner's resolution.
    let private = f.store.mutate(|s| s.add_secret(secret("SHARED_TOKEN", Some(&target)), true)).await.unwrap();
    f.store.mutate(|s| s.replace_secret(&private.id, "private-replacement-value", true)).await.unwrap();
    f.store.mutate(|s| s.delete_secret(&private.id, true)).await.unwrap();

    for status in [RunStatus::Starting, RunStatus::Running, RunStatus::AwaitingPermission] {
        f.store
            .mutate(|s| {
                s.conversation_mut(&active)?.run.status = status;
                Ok(())
            })
            .await
            .unwrap();
        let before = f.disk_text();
        assert_eq!(f.store.mutate(|s| s.add_secret(secret("NEW_PRIVATE", Some(&busy)), true)).await.unwrap_err().status, 409);
        assert_eq!(f.store.mutate(|s| s.replace_secret(&owned.id, "blocked-replacement-value", true)).await.unwrap_err().status, 409);
        assert_eq!(f.store.mutate(|s| s.delete_secret(&owned.id, true)).await.unwrap_err().status, 409);
        assert_eq!(f.store.mutate(|s| s.add_secret(secret("NEW_GLOBAL", None), true)).await.unwrap_err().status, 409);
        assert_eq!(f.store.mutate(|s| s.replace_secret(&global.id, "blocked-replacement-value", true)).await.unwrap_err().status, 409);
        assert_eq!(f.store.mutate(|s| s.delete_secret(&global.id, true)).await.unwrap_err().status, 409);
        assert_eq!(f.disk_text(), before, "{status:?} protects existing credentials without writes");
    }
    f.finish(&active).await;
    f.store.mutate(|s| s.replace_secret(&global.id, "allowed-global-replacement", true)).await.unwrap();
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn active_group_turn_locks_only_its_real_speaker_with_legacy_fallback() {
    let f = Fixture::new().await;
    let speaker = f.bot_id();
    let silent = add_bot(&f, "Silent").await;
    let group = f
        .store
        .mutate(|s| s.add_group(GroupInput { name: "Team".into(), color: None, shape: None, member_bot_ids: vec![speaker.clone(), silent.clone()] }))
        .await
        .unwrap();
    let group_conversation = f.store.mutate(|s| s.add_group_conversation(&group.id)).await.unwrap().id;
    for explicit_owner in [true, false] {
        let now = f.iso_now();
        f.store
            .mutate(|s| {
                let conversation = s.conversation_mut(&group_conversation)?;
                conversation.messages.push(Message {
                    id: new_id(),
                    role: Role::Assistant,
                    text: "Speaking".into(),
                    created_at: now.clone(),
                    tools: vec![],
                    speaker_bot_id: Some(speaker.clone()),
                    source_bot_id: None,
                    attachments: None,
                    bot_exchange_ids: None,
                    report_delivery_ids: None,
                });
                conversation.run = RunState {
                    id: new_id(),
                    status: RunStatus::Running,
                    bot_id: explicit_owner.then(|| speaker.clone()),
                    started_at: Some(now),
                    ..RunState::idle()
                };
                // Stopping a round can leave its current speaker active after pending members clear.
                conversation.group_round = None;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(f.store.mutate(|s| s.add_memories(&speaker, vec!["Blocked".into()], MemorySource::User, false)).await.unwrap_err().status, 409);
        f.store.mutate(|s| s.add_memories(&silent, vec![format!("Allowed: explicit owner {explicit_owner}")], MemorySource::User, false)).await.unwrap();
        assert_eq!(
            f.store.mutate(|s| s.delete_group(&group.id)).await.unwrap_err().status,
            409,
            "an active speaker still protects its conversation when round is cleared"
        );
        if !explicit_owner {
            f.store.mutate(|s| s.delete_bot(&silent)).await.unwrap();
            assert_eq!(f.conversation(&group_conversation).run.status, RunStatus::Running);
        }
        f.finish(&group_conversation).await;
    }
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn global_main_selection_and_template_import_still_require_every_bot_idle() {
    let f = Fixture::new().await;
    let target = add_bot(&f, "Target").await;
    let template = f.store.read(|s| s.export_template(&target, false)).unwrap();
    let active = conversation(&f, &f.bot_id()).await;
    f.start(&active).await;
    let before = f.disk_text();
    assert_eq!(f.store.mutate(|s| s.set_main_bot(&target)).await.unwrap_err().status, 409);
    assert_eq!(f.store.mutate(|s| s.add_bot_from_template(template)).await.unwrap_err().status, 409);
    assert_eq!(f.disk_text(), before);
    f.finish(&active).await;
}

#[tokio::test]
async fn accepted_user_work_blocks_only_its_bot_deletion_until_cancelled() {
    let f = configured().await;
    let gone = add_bot(&f, "Gone").await;
    let unrelated = add_bot(&f, "Unrelated").await;
    let queued = conversation(&f, &gone).await;
    let accepted = f.store.mutate(|s| s.enqueue_user_turn(&queued, "Accepted user work", vec![])).await.unwrap();
    let before = f.disk_text();
    let error = f.store.mutate(|s| s.delete_bot(&gone)).await.unwrap_err();
    assert_eq!(error.status, 409);
    assert!(error.message.contains("queued work"));
    assert_eq!(f.disk_text(), before, "deletion cannot silently drop an accepted turn");
    assert!(f.store.read(|s| s.disk().work_queue.iter().any(|work| work.id == accepted.id)));

    f.store.mutate(|s| s.delete_bot(&unrelated)).await.unwrap();
    assert!(f.store.mutate(|s| s.cancel_conversation_work(&queued)).await.unwrap());
    assert_eq!(f.store.mutate(|s| s.delete_bot(&gone)).await.unwrap(), vec![queued]);
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn bot_deletion_preserves_later_group_speakers_without_locking_unmentioned_members() {
    let f = configured().await;
    let a = f.bot_id();
    let b = add_bot(&f, "B").await;
    let excluded = add_bot(&f, "Excluded").await;
    let group = f
        .store
        .mutate(|s| s.add_group(GroupInput { name: "Team".into(), color: None, shape: None, member_bot_ids: vec![a.clone(), b.clone(), excluded.clone()] }))
        .await
        .unwrap();
    let group_conversation = f.store.mutate(|s| s.add_group_conversation(&group.id)).await.unwrap().id;
    let request = format!("@{} and @B please answer", f.store.read(|s| s.bot(&a).unwrap().name.clone()));
    let accepted = f.store.mutate(|s| s.enqueue_user_turn(&group_conversation, &request, vec![])).await.unwrap();
    assert_eq!(accepted.bot_id, a, "the canonical queue points to the first speaker only");
    assert_eq!(accepted.speaker_bot_ids, Some(vec![a.clone(), b.clone()]));
    f.store.mutate(|s| s.update_bot(&b, BotPatch { name: Some("Renamed".into()), ..BotPatch::default() })).await.unwrap();
    f.store.mutate(|s| s.add_memories(&b, vec!["Editable before my turn".into()], MemorySource::User, false)).await.unwrap();
    let before = f.disk_text();
    assert_eq!(f.store.mutate(|s| s.delete_bot(&b)).await.unwrap_err().status, 409, "a later accepted speaker is queued too");
    assert_eq!(f.disk_text(), before);
    f.store.mutate(|s| s.delete_bot(&excluded)).await.unwrap();
    assert!(f.store.mutate(|s| s.cancel_conversation_work(&group_conversation)).await.unwrap());

    let now = f.iso_now();
    f.store
        .mutate(|s| {
            let conversation = s.conversation_mut(&group_conversation)?;
            conversation.run =
                RunState { id: new_id(), status: RunStatus::Running, bot_id: Some(a.clone()), started_at: Some(now.clone()), ..RunState::idle() };
            conversation.group_round = Some(GroupRound { id: new_id(), user_message_id: new_id(), pending_bot_ids: vec![b.clone()], created_at: now });
            Ok(())
        })
        .await
        .unwrap();
    f.store.mutate(|s| s.add_memories(&b, vec!["Still editable until my turn".into()], MemorySource::User, false)).await.unwrap();
    assert_eq!(f.store.mutate(|s| s.delete_bot(&b)).await.unwrap_err().status, 409, "pending round members cannot be silently removed");
    f.store.mutate(|s| s.cancel_conversation_work(&group_conversation)).await.unwrap();
    f.store.mutate(|s| s.delete_bot(&b)).await.unwrap();
    assert_eq!(f.conversation(&group_conversation).run.status, RunStatus::Running);
    f.finish(&group_conversation).await;
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn group_structure_checks_only_its_own_active_pending_and_accepted_user_work() {
    let f = configured().await;
    let a = f.bot_id();
    let b = add_bot(&f, "B").await;
    let group = f
        .store
        .mutate(|s| s.add_group(GroupInput { name: "Team".into(), color: None, shape: None, member_bot_ids: vec![a.clone(), b.clone()] }))
        .await
        .unwrap();
    let group_conversation = f.store.mutate(|s| s.add_group_conversation(&group.id)).await.unwrap().id;
    let direct = conversation(&f, &a).await;
    f.start(&direct).await;
    let patch = || GroupPatch { member_bot_ids: Some(vec![b.clone(), a.clone()]), ..GroupPatch::default() };
    f.store.mutate(|s| s.update_group(&group.id, patch())).await.unwrap();
    f.store.mutate(|s| s.enqueue_user_turn(&group_conversation, "Accepted group work", vec![])).await.unwrap();

    let before = f.disk_text();
    assert_eq!(f.store.mutate(|s| s.update_group(&group.id, patch())).await.unwrap_err().status, 409);
    assert_eq!(f.store.mutate(|s| s.delete_group(&group.id)).await.unwrap_err().status, 409);
    assert_eq!(f.disk_text(), before, "membership and deletion cannot invalidate accepted group work");
    assert!(f.store.mutate(|s| s.cancel_conversation_work(&group_conversation)).await.unwrap());
    f.store.mutate(|s| s.update_group(&group.id, patch())).await.unwrap();
    assert_eq!(f.store.mutate(|s| s.delete_group(&group.id)).await.unwrap(), vec![group_conversation]);
    assert_eq!(f.conversation(&direct).run.status, RunStatus::Running);
    f.finish(&direct).await;
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn idle_bot_deletion_does_not_stop_an_unrelated_bot() {
    let f = Fixture::new().await;
    let busy = f.bot_id();
    let gone = add_bot(&f, "Gone").await;
    let active = conversation(&f, &busy).await;
    let removed = conversation(&f, &gone).await;
    f.store.mutate(|s| s.add_secret(secret("PRIVATE_TOKEN", Some(&gone)), true)).await.unwrap();
    f.start(&active).await;

    assert_eq!(f.store.mutate(|s| s.delete_bot(&gone)).await.unwrap(), vec![removed]);
    assert_eq!(f.conversation(&active).run.status, RunStatus::Running);
    assert!(f.store.snapshot().secrets.is_empty());
    assert_eq!(f.store.mutate(|s| s.delete_bot(&busy)).await.unwrap_err().status, 409);
    f.finish(&active).await;
    f.reopen().await.unwrap();
}
