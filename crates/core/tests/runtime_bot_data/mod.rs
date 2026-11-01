//! Runtime scenario for bot memories and knowledge files, included by `runtime.rs` (the engine
//! environment is process-global, so it runs in that binary's sequential scenario list).

use grok_bot_model::{KnowledgeInput, MemorySource, RunStatus};
use serde_json::{Value, json};

use super::{Fixture, content_text, discover, last_text, messages, text, tool_outcomes, turn_prompt, use_tool};

fn system_rules(requests: &[Value]) -> String {
    requests.iter().flat_map(messages).filter(|m| m["role"] == "system").map(|m| content_text(&m["content"])).collect()
}

fn rule_data(rules: &str, label: &str) -> Vec<Value> {
    let line = rules.split(label).nth(1).unwrap_or_else(|| panic!("{label} in rules")).lines().next().unwrap();
    serde_json::from_str(line).unwrap()
}

pub(super) async fn bot_memory_and_knowledge(f: &Fixture) {
    let key = "BOT_DATA_KEY_CANARY";
    f.configure(key, false).await;
    let bot = f.bot_id();
    let seeded = {
        let bot = bot.clone();
        f.store
            .mutate(move |s| s.add_memories(&bot, vec![format!("The user's token is {key}"), "User prefers Korean replies".into()], MemorySource::User, false))
            .await
            .unwrap()
            .added
    };
    let mut body: String = (0..900).map(|line| format!("line {line:04} of the team handbook\n")).collect();
    body.push_str(&format!("secret {key}\nKNOWLEDGE_TAIL_CANARY"));
    let file = f.store.add_knowledge(&bot, KnowledgeInput { name: "handbook.txt".into(), content: body.clone() }).await.unwrap();
    let id = f.conversation(&bot).await;

    // Turn 1: memories and the knowledge index arrive as redacted rule data; the paginated read
    // delivers the complete file (tail sentinel included) through the real engine.
    f.provider.clear();
    f.provider.plan(vec![
        discover("workspace list_memories list_knowledge read_knowledge"),
        use_tool("list_memories", json!({})),
        use_tool("list_knowledge", json!({})),
        use_tool("read_knowledge", json!({"id": file.id})),
        use_tool("read_knowledge", json!({"id": file.id, "offset": 12288})),
        use_tool("read_knowledge", json!({"id": file.id, "offset": 24576})),
        use_tool("read_knowledge", json!({"id": file.id, "offset": 999_999})),
        text("Read the handbook."),
    ]);
    f.runtime.start(&id, "Check the handbook.").await.unwrap();
    let first = f.completed(&id).await;
    let requests = f.provider.inference();
    let rules = system_rules(&requests);
    let memories = rule_data(&rules, "Bot memories (data): ");
    assert_eq!(memories.iter().map(|m| m["text"].as_str().unwrap()).collect::<Vec<_>>(), ["The user's token is [redacted]", "User prefers Korean replies"]);
    assert_eq!(rule_data(&rules, "Knowledge file index (data): "), [json!({"id": file.id, "name": "handbook.txt", "bytes": file.bytes})]);
    // The final request carries every tool result of the turn exactly once.
    let outcomes = tool_outcomes(&requests[requests.len() - 1..]);
    let pages: Vec<Value> = outcomes.iter().filter(|o| o.contains("\"totalBytes\"")).map(|o| serde_json::from_str(o).unwrap()).collect();
    assert_eq!(pages.len(), 3, "{outcomes:?}");
    assert_eq!(pages.iter().map(|p| p["nextOffset"].as_u64()).collect::<Vec<_>>(), [Some(12288), Some(24576), None]);
    let delivered: String = pages.iter().map(|p| p["content"].as_str().unwrap()).collect();
    assert_eq!(delivered, body.replace(key, "[redacted]"));
    assert!(outcomes.iter().any(|o| o.contains("offset must be 0 or a nextOffset value")), "{outcomes:?}");
    assert!(outcomes.iter().any(|o| o.contains("User prefers Korean replies") && o.contains("\"memories\"")));
    let wire = serde_json::to_string(&requests).unwrap();
    assert!(wire.contains("KNOWLEDGE_TAIL_CANARY") && !wire.contains(key));
    let session = first.session_id.clone().expect("session");

    // Turn 2: a denied save writes nothing and keeps the engine session.
    f.provider.clear();
    f.provider.plan(vec![discover("workspace save_memory"), use_tool("save_memory", json!({"text": "Favorite color is teal"})), text("Not saved.")]);
    f.runtime.start(&id, "Remember that my favorite color is teal.").await.unwrap();
    let pending = f.pending(&id).await;
    let permission = pending.run.permission.clone().unwrap();
    assert!(permission.title.starts_with("Remember: Favorite color is teal") && permission.description.contains("Favorite color is teal"), "{permission:?}");
    assert_eq!(f.store.read(|s| s.memories(&bot).unwrap().len()), 2, "nothing is saved before approval");
    f.decide(&id, "reject_once").await;
    let denied = f.completed(&id).await;
    assert_eq!(f.store.read(|s| s.memories(&bot).unwrap().len()), 2);
    assert!(tool_outcomes(&f.provider.inference()).iter().any(|o| o.contains("Memory save was denied or cancelled.")));
    assert_eq!(denied.session_id.as_deref(), Some(session.as_str()), "unchanged memories keep the session");

    // Turn 3: an approved save, an idempotent retry without a second card, and an approved forget.
    f.provider.clear();
    f.provider.plan(vec![
        discover("workspace save_memory forget_memory"),
        use_tool("save_memory", json!({"text": "Favorite color is teal"})),
        use_tool("save_memory", json!({"text": "favorite color is  TEAL"})),
        use_tool("forget_memory", json!({"id": seeded[0].id})),
        use_tool("forget_memory", json!({"id": "00000000-0000-4000-8000-000000000000"})),
        text("Saved and forgot."),
    ]);
    f.runtime.start(&id, "Remember teal and forget my token.").await.unwrap();
    f.decide(&id, "allow_once").await;
    let forget = f.decide(&id, "allow_once").await;
    assert!(forget.title.starts_with("Forget memory: The user's token is [redacted]"), "{forget:?}");
    assert!(!forget.description.contains(key));
    let saved = f.completed(&id).await;
    assert_eq!(last_text(&saved), "Saved and forgot.");
    let current = f.store.read(|s| s.memories(&bot).unwrap().to_vec());
    assert_eq!(current.iter().map(|m| (m.text.as_str(), m.source)).collect::<Vec<_>>(), [("User prefers Korean replies", MemorySource::User), ("Favorite color is teal", MemorySource::Bot)]);
    let outcomes = tool_outcomes(&f.provider.inference());
    assert!(outcomes.iter().any(|o| o.contains("\"existing\":true")), "{outcomes:?}");
    assert!(outcomes.iter().any(|o| o.contains("\"forgotten\":true")), "{outcomes:?}");
    assert!(outcomes.iter().any(|o| o.contains("Memory does not exist for this bot.")), "{outcomes:?}");
    assert_eq!(saved.session_id.as_deref(), Some(session.as_str()));

    // Turn 4: changed memories invalidate the engine session; the replacement carries the new
    // memory in its rules and continues from the visible history. Turn 5 reuses it.
    f.provider.clear();
    f.provider.plan(vec![text("New session.")]);
    f.runtime.start(&id, "What do you remember?").await.unwrap();
    let replaced = f.completed(&id).await;
    assert_ne!(replaced.session_id.as_deref(), Some(session.as_str()));
    let requests = f.provider.inference();
    let memories = rule_data(&system_rules(&requests), "Bot memories (data): ");
    assert_eq!(memories.iter().map(|m| m["text"].as_str().unwrap()).collect::<Vec<_>>(), ["User prefers Korean replies", "Favorite color is teal"]);
    assert!(turn_prompt(&requests[0]).contains("Current user message:\nWhat do you remember?"));
    f.provider.clear();
    f.provider.plan(vec![text("Same session.")]);
    f.runtime.start(&id, "Again.").await.unwrap();
    assert_eq!(f.completed(&id).await.session_id, replaced.session_id);

    // Deleting a knowledge file also changes the fingerprint.
    f.store.delete_knowledge(&bot, &file.id).await.unwrap();
    f.provider.clear();
    f.provider.plan(vec![discover("workspace read_knowledge"), use_tool("read_knowledge", json!({"id": file.id})), text("Gone.")]);
    f.runtime.start(&id, "Read it again.").await.unwrap();
    let after = f.completed(&id).await;
    assert_ne!(after.session_id, replaced.session_id);
    assert!(tool_outcomes(&f.provider.inference()).iter().any(|o| o.contains("Knowledge file does not exist for this bot.")));
    assert_eq!(rule_data(&system_rules(&f.provider.inference()), "Knowledge file index (data): "), Vec::<Value>::new());
    assert_eq!(after.run.status, RunStatus::Completed);
}
