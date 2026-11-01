//! Group chats and threads: validation, member edits, deletion cascades, thread linkage, speaker
//! order, and persistence.

mod common;

use common::Fixture;
use grok_bot_model::*;
use grok_bot_store::groups::{group_speakers, mentioned_members};

fn bot_input(name: &str) -> BotInput {
    BotInput { name: name.into(), color: None, instructions: None, shape: None, pinned: None }
}

fn group_input(name: &str, members: &[&str]) -> GroupInput {
    GroupInput { name: name.into(), color: Some("#00c77b".into()), shape: Some(AvatarShape::Cloud), member_bot_ids: members.iter().map(|id| id.to_string()).collect() }
}

async fn add_bot(f: &Fixture, name: &str) -> String {
    let input = bot_input(name);
    f.store.mutate(|s| s.add_bot(input)).await.unwrap().id
}

fn message(role: Role, text: &str, speaker: Option<&str>, now: &str) -> Message {
    Message {
        id: new_id(),
        role,
        text: text.into(),
        created_at: now.into(),
        tools: vec![],
        bot_exchange_ids: None,
        source_bot_id: None,
        speaker_bot_id: speaker.map(str::to_owned),
        attachments: None,
    }
}

async fn push(f: &Fixture, conversation: &str, message: Message) -> String {
    let id = message.id.clone();
    f.store
        .mutate(|s| {
            s.conversation_mut(conversation)?.messages.push(message);
            Ok(())
        })
        .await
        .unwrap();
    id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn groups_validate_names_colors_and_members() {
    let f = Fixture::new().await;
    let grok = f.bot_id();
    let second = add_bot(&f, "Second").await;
    let error = |input: GroupInput| {
        let store = f.store.clone();
        async move { store.mutate(|s| s.add_group(input)).await.unwrap_err() }
    };
    let blank = error(group_input("  ", &[&grok, &second])).await;
    assert_eq!(blank.status, 400);
    assert!(blank.message.starts_with("name: Too small"), "{}", blank.message);
    let single = error(group_input("Solo", &[&grok])).await;
    assert_eq!((single.status, single.message.as_str()), (400, "memberBotIds: Choose between 2 and 12 member bots."));
    let duplicate = error(group_input("Twice", &[&grok, &grok])).await;
    assert_eq!(duplicate.message, "memberBotIds: Each bot can be a member only once.");
    let invalid = error(group_input("Invalid", &[&grok, "not-a-uuid"])).await;
    assert_eq!(invalid.message, "memberBotIds: Invalid UUID");
    let missing = error(group_input("Missing", &[&grok, &new_id()])).await;
    assert_eq!((missing.status, missing.message.as_str()), (404, "Bot does not exist."));
    let color = error(GroupInput { color: Some("red".into()), ..group_input("Color", &[&grok, &second]) }).await;
    assert!(color.message.starts_with("color: Invalid string"), "{}", color.message);
    let too_many: Vec<String> = (0..13).map(|_| new_id()).collect();
    let refs: Vec<&str> = too_many.iter().map(String::as_str).collect();
    assert_eq!(error(group_input("Crowd", &refs)).await.message, "memberBotIds: Choose between 2 and 12 member bots.");
    assert!(f.store.read(|s| s.disk().groups.is_empty()));

    let group = f.store.mutate(|s| s.add_group(group_input("  Team  ", &[&second, &grok]))).await.unwrap();
    assert_eq!(group.name, "Team");
    assert_eq!(group.member_bot_ids, [second.clone(), grok.clone()]);
    assert_eq!(group.shape, Some(AvatarShape::Cloud));
    let snapshot = f.store.snapshot();
    assert_eq!(snapshot.groups, std::slice::from_ref(&group));
    // Group IDs never resolve as bots, so 1:1 conversation creation rejects them.
    assert_eq!(f.store.mutate(|s| s.add_conversation(&group.id)).await.unwrap_err().status, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn members_and_fields_are_edited_with_idle_and_round_checks() {
    let f = Fixture::new().await;
    let grok = f.bot_id();
    let second = add_bot(&f, "Second").await;
    let third = add_bot(&f, "Third").await;
    let group = f.store.mutate(|s| s.add_group(group_input("Team", &[&grok, &second]))).await.unwrap();
    let id = group.id.clone();
    let empty = f.store.mutate(|s| s.update_group(&id, GroupPatch::default())).await.unwrap_err();
    assert_eq!(empty.message, "Provide a group field to update.");
    let patch = GroupPatch { name: Some("Renamed".into()), member_bot_ids: Some(vec![third.clone(), grok.clone(), second.clone()]), pinned: Some(true), ..GroupPatch::default() };
    f.store.mutate(|s| s.update_group(&id, patch)).await.unwrap();
    let edited = f.store.read(|s| s.group(&id).unwrap().clone());
    assert_eq!((edited.name.as_str(), edited.pinned), ("Renamed", Some(true)));
    assert_eq!(edited.member_bot_ids, [third.clone(), grok.clone(), second.clone()]);

    // Member edits wait for idle and for a finished round; renames do not.
    let conversation = f.store.mutate(|s| s.add_group_conversation(&id)).await.unwrap();
    assert_eq!((conversation.bot_id.as_str(), conversation.group_id.as_deref()), (id.as_str(), Some(id.as_str())));
    f.start(&conversation.id).await;
    let busy = f.store.mutate(|s| s.update_group(&id, GroupPatch { member_bot_ids: Some(vec![grok.clone(), second.clone()]), ..GroupPatch::default() })).await.unwrap_err();
    assert_eq!(busy.status, 409);
    f.store.mutate(|s| s.update_group(&id, GroupPatch { name: Some("While running".into()), ..GroupPatch::default() })).await.unwrap();
    f.finish(&conversation.id).await;
    let now = f.iso_now();
    let round_conversation = conversation.id.clone();
    let second_member = second.clone();
    f.store
        .mutate(move |s| {
            s.conversation_mut(&round_conversation)?.group_round =
                Some(GroupRound { id: new_id(), user_message_id: new_id(), pending_bot_ids: vec![second_member], created_at: now });
            Ok(())
        })
        .await
        .unwrap();
    assert!(f.store.read(|s| s.group_round_pending(&conversation.id)));
    let pending = f.store.mutate(|s| s.update_group(&id, GroupPatch { member_bot_ids: Some(vec![grok.clone(), second.clone()]), ..GroupPatch::default() })).await.unwrap_err();
    assert_eq!((pending.status, pending.message.as_str()), (409, "Wait for the group to finish answering or stop it first."));
    assert_eq!(f.store.mutate(|s| s.delete_group(&id)).await.unwrap_err().status, 409);

    // Cancelling between member turns clears the queue and settles the conversation.
    assert!(f.store.mutate(|s| s.cancel_queued_work(&conversation.id)).await.unwrap());
    let cancelled = f.conversation(&conversation.id);
    assert_eq!((cancelled.run.status, cancelled.group_round.is_none()), (RunStatus::Cancelled, true));
    assert!(!f.store.mutate(|s| s.cancel_queued_work(&conversation.id)).await.unwrap());

    // While a member runs, cancellation clears the queue but leaves the run to the runtime.
    f.start(&conversation.id).await;
    let now = f.iso_now();
    let round_conversation = conversation.id.clone();
    let grok_member = grok.clone();
    f.store
        .mutate(move |s| {
            s.conversation_mut(&round_conversation)?.group_round =
                Some(GroupRound { id: new_id(), user_message_id: new_id(), pending_bot_ids: vec![grok_member], created_at: now });
            Ok(())
        })
        .await
        .unwrap();
    assert!(!f.store.mutate(|s| s.cancel_queued_work(&conversation.id)).await.unwrap());
    let running = f.conversation(&conversation.id);
    assert_eq!((running.run.status, running.group_round.is_none()), (RunStatus::Running, true));

    f.store.mutate(|s| s.fail_group_round(&conversation.id, "Member failed")).await.unwrap();
    let failed = f.conversation(&conversation.id);
    assert_eq!((failed.run.status, failed.run.error.as_deref()), (RunStatus::Error, Some("Member failed")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_group_removes_its_conversations_and_threads_but_keeps_bots() {
    let f = Fixture::new().await;
    let grok = f.bot_id();
    let second = add_bot(&f, "Second").await;
    let group = f.store.mutate(|s| s.add_group(group_input("Team", &[&grok, &second]))).await.unwrap();
    let group_conversation = f.store.mutate(|s| s.add_group_conversation(&group.id)).await.unwrap();
    let now = f.iso_now();
    let parent = push(&f, &group_conversation.id, message(Role::Assistant, "Group answer", Some(&second), &now)).await;
    let group_thread = f.store.mutate(|s| s.create_thread(&group_conversation.id, &parent)).await.unwrap();
    let direct = f.store.mutate(|s| s.add_conversation(&grok)).await.unwrap();
    let direct_message = push(&f, &direct.id, message(Role::User, "Keep me", None, &now)).await;
    let direct_thread = f.store.mutate(|s| s.create_thread(&direct.id, &direct_message)).await.unwrap();

    f.start(&direct.id).await;
    assert_eq!(f.store.mutate(|s| s.delete_group(&group.id)).await.unwrap_err().status, 409);
    f.finish(&direct.id).await;
    f.store.mutate(|s| s.delete_group(&group.id)).await.unwrap();
    let ids: Vec<String> = f.store.read(|s| s.disk().conversations.iter().map(|c| c.id.clone()).collect());
    assert!(!ids.contains(&group_conversation.id) && !ids.contains(&group_thread.id));
    assert!(ids.contains(&direct.id) && ids.contains(&direct_thread.id));
    assert_eq!(f.store.read(|s| s.disk().bots.len()), 2);
    assert!(f.store.read(|s| s.disk().groups.is_empty()));
    assert_eq!(f.store.mutate(|s| s.delete_group(&group.id)).await.unwrap_err().status, 404);
    // The reloaded state is valid.
    let reopened = f.reopen().await.unwrap();
    assert!(reopened.snapshot().groups.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn threads_link_to_one_message_and_inherit_the_parent_speaker() {
    let f = Fixture::new().await;
    let grok = f.bot_id();
    let second = add_bot(&f, "Second").await;
    let direct = f.store.mutate(|s| s.add_conversation(&grok)).await.unwrap();
    let now = f.iso_now();
    let user = push(&f, &direct.id, message(Role::User, "Question with\n\nspacing", None, &now)).await;
    let empty = push(&f, &direct.id, message(Role::Assistant, "  ", None, &now)).await;
    let thread = f.store.mutate(|s| s.create_thread(&direct.id, &user)).await.unwrap();
    assert_eq!(thread.bot_id, grok);
    assert_eq!(thread.group_id, None);
    assert_eq!(thread.thread, Some(ThreadLink { parent_conversation_id: direct.id.clone(), parent_message_id: user.clone() }));
    assert_eq!(thread.title, "Thread: Question with spacing");
    let again = f.store.mutate(|s| s.create_thread(&direct.id, &user)).await.unwrap();
    assert_eq!(again.id, thread.id);
    assert_eq!(f.store.read(|s| s.disk().conversations.len()), 2);
    let nested = f.store.mutate(|s| s.create_thread(&thread.id, &user)).await.unwrap_err();
    assert_eq!((nested.status, nested.message.as_str()), (400, "Threads cannot be nested. Reply in this thread instead."));
    assert_eq!(f.store.mutate(|s| s.create_thread(&direct.id, &empty)).await.unwrap_err().status, 400);
    assert_eq!(f.store.mutate(|s| s.create_thread(&direct.id, &new_id())).await.unwrap_err().status, 404);
    assert_eq!(f.store.mutate(|s| s.create_thread(&new_id(), &user)).await.unwrap_err().status, 404);
    assert_eq!(f.store.mutate(|s| s.create_thread(&direct.id, "bad")).await.unwrap_err().message, "messageId: Invalid UUID");

    let group = f.store.mutate(|s| s.add_group(group_input("Team", &[&grok, &second]))).await.unwrap();
    let group_conversation = f.store.mutate(|s| s.add_group_conversation(&group.id)).await.unwrap();
    let answer = push(&f, &group_conversation.id, message(Role::Assistant, "Member answer", Some(&second), &now)).await;
    let group_thread = f.store.mutate(|s| s.create_thread(&group_conversation.id, &answer)).await.unwrap();
    assert_eq!((group_thread.bot_id.as_str(), group_thread.group_id.as_deref()), (group.id.as_str(), Some(group.id.as_str())));
    assert_eq!(f.store.read(|s| s.thread_for(&answer).map(|c| c.id.clone())), Some(group_thread.id.clone()));
}

#[test]
fn speakers_follow_member_order_or_whole_name_mentions() {
    let bot = |name: &str| Bot {
        id: new_id(),
        name: name.into(),
        color: "#8b5cf6".into(),
        instructions: String::new(),
        shape: None,
        skills: None,
        pinned: None,
        hidden: None,
        created_at: "2026-01-01T00:00:00.000Z".into(),
        memories: None,
        knowledge: None,
        unread: None,
        notify_on_updates: None,
    };
    let (al, alice, research) = (bot("Al"), bot("Alice"), bot("Research Bot"));
    let members = [&alice, &al, &research];
    let ids = |bots: &[&Bot]| bots.iter().map(|bot| bot.id.clone()).collect::<Vec<_>>();
    assert_eq!(group_speakers(&members, "Hello team"), ids(&members));
    assert_eq!(group_speakers(&members, "@alice what do you think?"), ids(&[&alice]));
    // Mentions answer in member order, not mention order; `@Al` does not match inside `@Alice`.
    assert_eq!(group_speakers(&members, "@Research Bot then @Alice"), ids(&[&alice, &research]));
    assert_eq!(group_speakers(&members, "@Al, please"), ids(&[&al]));
    assert_eq!(mentioned_members(&members, "@Alicia and email@al.example"), Vec::<String>::new());
    assert_eq!(group_speakers(&members, "@Nobody here"), ids(&members));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn groups_threads_and_rounds_persist_and_dangling_members_are_pruned_on_load() {
    let f = Fixture::new().await;
    // Empty groups are not written, so existing state files keep their shape.
    assert!(!f.disk_text().contains("\"groups\""));
    let grok = f.bot_id();
    let second = add_bot(&f, "Second").await;
    let temporary = add_bot(&f, "Temporary").await;
    let group = f.store.mutate(|s| s.add_group(group_input("Team", &[&grok, &temporary, &second]))).await.unwrap();
    let conversation = f.store.mutate(|s| s.add_group_conversation(&group.id)).await.unwrap();
    let now = f.iso_now();
    let user = push(&f, &conversation.id, message(Role::User, "Hello", None, &now)).await;
    push(&f, &conversation.id, message(Role::Assistant, "Hi from Grok", Some(&grok), &now)).await;
    let thread = f.store.mutate(|s| s.create_thread(&conversation.id, &user)).await.unwrap();
    let round_conversation = conversation.id.clone();
    let pending = vec![temporary.clone(), second.clone()];
    let round_now = now.clone();
    let round_user = user.clone();
    f.store
        .mutate(move |s| {
            let conversation = s.conversation_mut(&round_conversation)?;
            conversation.run.status = RunStatus::Completed;
            conversation.group_round = Some(GroupRound { id: new_id(), user_message_id: round_user, pending_bot_ids: pending, created_at: round_now });
            conversation.member_sessions = Some(vec![MemberSession { bot_id: grok_session(), session_id: "session-1".into(), session_fingerprint: "f".repeat(64) }]);
            Ok(())
        })
        .await
        .unwrap();
    fn grok_session() -> String {
        "00000000-0000-0000-0000-000000000000".into()
    }
    f.store.flush().await.unwrap();
    let disk = f.disk_json();
    assert_eq!(disk["groups"][0]["memberBotIds"].as_array().unwrap().len(), 3);
    assert_eq!(disk["conversations"][0]["groupId"], group.id.as_str());
    assert_eq!(disk["conversations"][0]["messages"][1]["speakerBotId"], grok.as_str());
    assert_eq!(disk["conversations"][1]["thread"]["parentMessageId"], user.as_str());

    // Remove the temporary bot from state.json directly, as a bot deletion elsewhere would.
    let mut edited = disk.clone();
    edited["bots"].as_array_mut().unwrap().retain(|bot| bot["id"] != temporary.as_str());
    std::fs::write(f.state_file(), serde_json::to_string(&edited).unwrap()).unwrap();
    let reopened = f.reopen().await.unwrap();
    let snapshot = reopened.snapshot();
    assert_eq!(snapshot.groups[0].member_bot_ids, [grok.clone(), second.clone()]);
    let restored = snapshot.conversations.iter().find(|c| c.id == conversation.id).unwrap();
    assert_eq!(restored.group_round.as_ref().unwrap().pending_bot_ids, std::slice::from_ref(&second));
    assert_eq!(restored.member_sessions.as_ref().unwrap()[0].session_id, "session-1");
    assert!(snapshot.conversations.iter().any(|c| c.id == thread.id && c.thread.is_some()));

    // A group conversation whose group is gone is an invalid reference.
    let mut orphan = edited.clone();
    orphan["groups"] = serde_json::json!([]);
    drop(reopened);
    std::fs::write(f.state_file(), serde_json::to_string(&orphan).unwrap()).unwrap();
    assert!(f.reopen().await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_a_bot_from_groups_keeps_groups_valid() {
    let f = Fixture::new().await;
    let grok = f.bot_id();
    let second = add_bot(&f, "Second").await;
    let group = f.store.mutate(|s| s.add_group(group_input("Team", &[&grok, &second]))).await.unwrap();
    let conversation = f.store.mutate(|s| s.add_group_conversation(&group.id)).await.unwrap();
    let id = conversation.id.clone();
    let (member, now) = (second.clone(), f.iso_now());
    f.store
        .mutate(move |s| {
            let conversation = s.conversation_mut(&id)?;
            conversation.group_round = Some(GroupRound { id: new_id(), user_message_id: new_id(), pending_bot_ids: vec![member.clone()], created_at: now });
            conversation.member_sessions = Some(vec![MemberSession { bot_id: member, session_id: "s".into(), session_fingerprint: "f".into() }]);
            Ok(())
        })
        .await
        .unwrap();
    let removed = second.clone();
    f.store
        .mutate(move |s| {
            s.remove_bot_from_groups(&removed);
            Ok(())
        })
        .await
        .unwrap();
    let snapshot = f.store.snapshot();
    assert_eq!(snapshot.groups[0].member_bot_ids, std::slice::from_ref(&grok));
    let conversation = snapshot.conversations.iter().find(|c| c.id == conversation.id).unwrap();
    assert!(conversation.group_round.is_none());
    assert_eq!(conversation.member_sessions.as_deref(), Some(&[][..]));
    // Hidden bots stay members.
    f.store.mutate(|s| s.update_bot(&grok, BotPatch { hidden: Some(true), ..BotPatch::default() })).await.unwrap();
    assert_eq!(f.store.read(|s| s.group_members(&group.id).unwrap().len()), 1);
}
