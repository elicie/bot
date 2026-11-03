//! Port of `tests/store-ordering.test.ts` plus the store contract: commit rules, settings and bot
//! input validation (the zod schemas from `server/app.ts`), snapshot redaction, and `state.json`
//! compatibility with the Node implementation.

mod common;

use std::sync::Arc;

use common::Fixture;
use grok_bot_model::*;
use grok_bot_store::Store;

fn bot_input(name: &str) -> BotInput {
    BotInput { name: name.into(), color: None, instructions: None, shape: None, pinned: None }
}

fn settings(base_url: &str, api_key: Option<&str>) -> SettingsInput {
    SettingsInput {
        base_url: base_url.into(),
        model: "model".into(),
        workspace: String::new(),
        api_key: api_key.map(Into::into),
        clear_api_key: None,
        max_concurrent_runs: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn state_revisions_survive_restart_and_subscribers_see_committed_disk_snapshots() {
    let f = Fixture::new().await;
    let mut events = f.store.subscribe();
    let path = f.state_file();
    let observer = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Ok(snapshot) = events.recv().await {
            let persisted: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert!(persisted["revision"].as_u64().unwrap() >= snapshot.revision);
            let name = &snapshot.bots.last().unwrap().name;
            assert!(persisted["bots"].as_array().unwrap().iter().any(|bot| bot["name"] == name.as_str()));
            seen.push(snapshot.revision);
            if seen.len() == 41 {
                break;
            }
        }
        seen
    });
    let before = f.store.snapshot().revision;
    f.store.mutate(|s| s.add_bot(BotInput { color: Some("#75a9c0".into()), ..bot_input("Persisted bot") })).await.unwrap();
    assert!(f.store.snapshot().revision > before);
    // Concurrent commits are written and broadcast in revision order.
    let tasks: Vec<_> = (0..40)
        .map(|index| {
            let store = f.store.clone();
            let bot = f.bot_id();
            tokio::spawn(async move {
                store
                    .mutate(|s| {
                        let conversation = s.add_conversation(&bot)?;
                        s.conversation_mut(&conversation.id)?.title = format!("Conversation {index}");
                        Ok(())
                    })
                    .await
                    .unwrap()
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    let seen = observer.await.unwrap();
    assert!(seen.windows(2).all(|pair| pair[0] < pair[1]), "{seen:?}");
    let previous = f.store.snapshot().revision;
    assert_eq!(*seen.last().unwrap(), previous);
    f.store.flush().await.unwrap();
    let restarted = f.reopen().await.unwrap();
    assert!(restarted.snapshot().revision > previous);
    assert_eq!(restarted.snapshot().bots.last().unwrap().name, "Persisted bot");
    assert_eq!(restarted.snapshot().conversations.len(), 40);
    restarted.flush().await.unwrap();
}

#[tokio::test]
async fn mutations_commit_only_when_ok_and_dirty() {
    let f = Fixture::new().await;
    let revision = f.store.snapshot().revision;
    let before = f.disk_text();
    let mut events = f.store.subscribe();
    assert_eq!(f.store.mutate(|s| Ok(s.disk().bots.len())).await.unwrap(), 1);
    assert_eq!(f.store.mutate(|s| s.assert_idle()).await, Ok(()));
    assert_eq!(f.store.mutate(|_| Err::<(), _>(HttpError::conflict("no"))).await.unwrap_err().status, 409);
    f.store
        .mutate(|s| {
            s.set_routine_scheduling_error(None);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(f.store.snapshot().revision, revision);
    assert_eq!(f.disk_text(), before);
    assert!(events.try_recv().is_err());
    f.store
        .mutate(|s| {
            s.set_routine_scheduling_error(Some("Routine history is full.".into()));
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(f.store.snapshot().revision, revision + 1);
    assert_eq!(events.recv().await.unwrap().routine_scheduling_error.as_deref(), Some("Routine history is full."));
    f.store
        .mutate(|s| {
            s.set_runtime(RuntimeInfo { available: true, bot_message_chains: None, version: Some("1.2.3".into()), error: None });
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(f.store.snapshot().runtime.version.as_deref(), Some("1.2.3"));
    assert_eq!(f.store.snapshot().revision, revision + 2);
    // Default state: one Grok bot, nothing else, private 0600 file in a 0700 directory.
    let disk = f.disk_json();
    assert_eq!(disk["bots"][0]["name"], "Grok");
    assert_eq!(disk["bots"][0]["color"], "#8b5cf6");
    assert_eq!(disk["settings"], serde_json::json!({ "baseUrl": "", "model": "", "workspace": "", "apiKey": "" }));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(f.state_file()).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(&f.data_dir).unwrap().permissions().mode() & 0o777, 0o700);
}

#[tokio::test]
async fn run_todos_survive_restart_including_interrupted_and_explicitly_empty_lists() {
    let f = Fixture::new().await;
    let conversation = f.store.mutate(|s| s.add_conversation(&f.bot)).await.unwrap();
    assert!(f.disk_json()["conversations"][0]["run"].get("todos").is_none());
    let todos = vec![
        TodoItem { content: "Queued".into(), status: TodoStatus::Pending },
        TodoItem { content: "Working".into(), status: TodoStatus::InProgress },
        TodoItem { content: "Done".into(), status: TodoStatus::Completed },
        TodoItem { content: "Skipped".into(), status: TodoStatus::Cancelled },
    ];
    f.set_run(&conversation.id, RunState { id: new_id(), status: RunStatus::Completed, todos: Some(todos.clone()), ..RunState::idle() }).await;
    let restarted = f.reopen().await.unwrap();
    assert_eq!(restarted.snapshot().conversations[0].run.todos.as_ref(), Some(&todos));
    restarted
        .mutate(|s| {
            s.conversation_mut(&conversation.id)?.run.status = RunStatus::Running;
            Ok(())
        })
        .await
        .unwrap();
    drop(restarted);
    let restarted = f.reopen().await.unwrap();
    let snapshot = restarted.snapshot();
    let run = &snapshot.conversations[0].run;
    assert_eq!(run.status, RunStatus::Interrupted);
    assert_eq!(run.todos.as_ref(), Some(&todos));
    restarted
        .mutate(|s| {
            s.conversation_mut(&conversation.id)?.run.todos = Some(Vec::new());
            Ok(())
        })
        .await
        .unwrap();
    drop(restarted);
    let restarted = f.reopen().await.unwrap();
    assert_eq!(restarted.snapshot().conversations[0].run.todos, Some(Vec::new()));
    assert_eq!(f.disk_json()["conversations"][0]["run"]["todos"], serde_json::json!([]));
}

#[tokio::test]
async fn opening_rejects_symlinks_hard_links_and_corrupt_state_without_replacing_it() {
    let f = Fixture::new().await;
    let good = f.disk_text();
    std::fs::write(f.state_file(), "{not json").unwrap();
    assert!(f.reopen().await.unwrap_err().to_string().contains("existing state.json was not replaced"));
    assert_eq!(f.disk_text(), "{not json");
    std::fs::write(f.root.path().join("elsewhere.json"), &good).unwrap();
    std::fs::remove_file(f.state_file()).unwrap();
    std::os::unix::fs::symlink(f.root.path().join("elsewhere.json"), f.state_file()).unwrap();
    assert!(f.reopen().await.is_err());
    std::fs::remove_file(f.state_file()).unwrap();
    std::fs::hard_link(f.root.path().join("elsewhere.json"), f.state_file()).unwrap();
    assert!(f.reopen().await.is_err());
    std::fs::remove_file(f.state_file()).unwrap();
    std::fs::write(f.state_file(), &good).unwrap();
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn settings_input_matches_the_node_route_and_store_rules() {
    let f = Fixture::new().await;
    let error = |input: SettingsInput| {
        let store = f.store.clone();
        async move { store.mutate(|s| s.update_settings(input)).await.unwrap_err() }
    };
    let multi = error(SettingsInput {
        base_url: "x".repeat(2049),
        model: String::new(),
        workspace: "w".repeat(4097),
        api_key: Some("\r".into()),
        clear_api_key: None,
        max_concurrent_runs: None,
    })
    .await;
    assert_eq!(
        multi.message,
        "baseUrl: Too big: expected string to have <=2048 characters; model: Too small: expected string to have >=1 characters; workspace: Too big: expected string to have <=4096 characters; apiKey: API key contains invalid characters."
    );
    assert_eq!(
        error(SettingsInput { model: "m".repeat(257), ..settings("http://localhost/", None) }).await.message,
        "model: Too big: expected string to have <=256 characters"
    );
    assert_eq!(error(settings("http://localhost/", Some(&"k".repeat(16385)))).await.message, "apiKey: Too big: expected string to have <=16384 characters");
    assert_eq!(error(settings("nope", None)).await.message, "Base URL must be an absolute HTTP or HTTPS URL.");
    assert_eq!(
        error(settings("http://user:pass@localhost/", None)).await.message,
        "Base URL must use HTTP or HTTPS without credentials, query parameters, or fragments."
    );
    assert_eq!(error(settings("https://api.example.com/v1", None)).await.message, "An API key is required for non-loopback providers.");
    let data_dir = f.data_dir.to_string_lossy().into_owned();
    let inside = error(SettingsInput { workspace: data_dir.clone(), ..settings("http://localhost/", None) }).await;
    assert_eq!(inside.message, "Workspace must not contain the application data directory or be the filesystem root.");
    let parent = error(SettingsInput { workspace: f.root.path().to_string_lossy().into_owned(), ..settings("http://localhost/", None) }).await;
    assert_eq!(parent.status, 400);
    let relative = error(SettingsInput { workspace: "relative".into(), ..settings("http://localhost/", None) }).await;
    assert_eq!(relative.message, "Workspace must be an absolute directory path.");

    let conversation = f.store.mutate(|s| s.add_conversation(&f.bot)).await.unwrap();
    f.store
        .mutate(|s| {
            let c = s.conversation_mut(&conversation.id)?;
            c.session_id = Some("s".into());
            c.session_fingerprint = Some("f".into());
            Ok(())
        })
        .await
        .unwrap();
    let workspace = f.workspace.to_string_lossy().into_owned();
    let input = SettingsInput { model: " model ".into(), workspace: format!(" {workspace} "), ..settings(" https://API.example.com/v1/ ", Some(" secret ")) };
    f.store.mutate(|s| s.update_settings(input)).await.unwrap();
    let saved = f.store.read(|s| s.settings().clone());
    assert_eq!(
        saved,
        PrivateSettings {
            base_url: "https://api.example.com/v1".into(),
            model: "model".into(),
            workspace: workspace.clone(),
            api_key: "secret".into(),
            max_concurrent_runs: None
        }
    );
    assert_eq!(f.conversation(&conversation.id).session_id, None);
    // Same origin keeps the stored key; another origin or clearApiKey drops it.
    f.store.mutate(|s| s.update_settings(settings("https://api.example.com/v2", None))).await.unwrap();
    assert_eq!(f.store.read(|s| s.settings().api_key.clone()), "secret");
    assert_eq!(error(settings("https://other.example.com/v1", Some("  "))).await.status, 400);
    f.store.mutate(|s| s.update_settings(SettingsInput { clear_api_key: Some(true), ..settings("http://[::1]:8080/v1", Some("ignored")) })).await.unwrap();
    assert_eq!(f.store.read(|s| s.settings().clone()).api_key, "");
    let snapshot = f.store.snapshot();
    assert_eq!(
        snapshot.settings,
        PublicSettings {
            base_url: "http://[::1]:8080/v1".into(),
            model: "model".into(),
            has_api_key: false,
            workspace: String::new(),
            max_concurrent_runs: Some(3)
        }
    );
    // Settings, bots, and files are locked while a run is active.
    f.start(&conversation.id).await;
    assert_eq!(error(settings("http://localhost/", None)).await.status, 409);
}

#[tokio::test]
async fn bot_input_matches_the_node_route_schema() {
    let f = Fixture::new().await;
    let error = |input: BotInput| {
        let store = f.store.clone();
        async move { store.mutate(|s| s.add_bot(input)).await.unwrap_err().message }
    };
    assert_eq!(
        error(BotInput { name: " ".into(), color: Some("#12345".into()), instructions: Some("i".repeat(30001)), shape: None, pinned: None }).await,
        r"name: Too small: expected string to have >=1 characters; color: Invalid string: must match pattern /^#[\da-fA-F]{6}$/; instructions: Too big: expected string to have <=30000 characters"
    );
    assert_eq!(error(bot_input(&"n".repeat(101))).await, "name: Too big: expected string to have <=100 characters");
    let bot = f.store.mutate(|s| s.add_bot(BotInput { shape: Some(AvatarShape::Hexagon), pinned: Some(true), ..bot_input("  Helper  ") })).await.unwrap();
    assert_eq!((bot.name.as_str(), bot.color.as_str(), bot.instructions.as_str()), ("Helper", "#8b5cf6", ""));
    assert_eq!((bot.shape, bot.pinned, bot.hidden), (Some(AvatarShape::Hexagon), Some(true), None));
    let empty = f.store.mutate(|s| s.update_bot(&bot.id, BotPatch::default())).await.unwrap_err();
    assert_eq!((empty.status, empty.message.as_str()), (400, "Provide a bot field to update."));
    let missing = f.store.mutate(|s| s.update_bot(&new_id(), BotPatch { hidden: Some(true), ..BotPatch::default() })).await.unwrap_err();
    assert_eq!((missing.status, missing.message.as_str()), (404, "Bot does not exist."));
    let conversation = f.store.mutate(|s| s.add_conversation(&bot.id)).await.unwrap();
    assert_eq!(conversation.title, "New conversation");
    f.store
        .mutate(|s| {
            s.conversation_mut(&conversation.id)?.session_id = Some("kept".into());
            Ok(())
        })
        .await
        .unwrap();
    f.store.mutate(|s| s.update_bot(&bot.id, BotPatch { instructions: Some(String::new()), hidden: Some(true), ..BotPatch::default() })).await.unwrap();
    assert_eq!(f.conversation(&conversation.id).session_id.as_deref(), Some("kept"));
    f.store.mutate(|s| s.update_bot(&bot.id, BotPatch { instructions: Some("New".into()), ..BotPatch::default() })).await.unwrap();
    assert_eq!(f.conversation(&conversation.id).session_id, None);
    assert_eq!(f.store.read(|s| s.bot(&bot.id).unwrap().hidden), Some(true));
    let invalid = f.store.mutate(|s| s.add_conversation("nope")).await.unwrap_err();
    assert_eq!((invalid.status, invalid.message.as_str()), (400, "botId: Invalid UUID"));
    assert_eq!(f.store.mutate(|s| s.add_conversation(&new_id())).await.unwrap_err().status, 404);
}

#[tokio::test]
async fn snapshot_redaction_keeps_identifiers_enums_and_link_arrays() {
    let f = Fixture::new().await;
    // A key that appears inside UUIDs, timestamps, statuses, and colors.
    let key = "a";
    f.store.mutate(|s| s.update_settings(settings("http://127.0.0.1:1/a", Some(key)))).await.unwrap();
    let bot = f.store.mutate(|s| s.add_bot(BotInput { color: Some("#aaaaaa".into()), ..bot_input("Banana") })).await.unwrap();
    let conversation = f.store.mutate(|s| s.add_conversation(&bot.id)).await.unwrap();
    f.store
        .mutate(|s| {
            let c = s.conversation_mut(&conversation.id)?;
            c.session_id = Some("session-a".into());
            c.session_fingerprint = Some("a".repeat(64));
            c.messages.push(Message {
                id: new_id(),
                role: Role::Assistant,
                text: "say a".into(),
                created_at: "2026-01-01T00:00:00.000Z".into(),
                tools: vec![ToolActivity {
                    id: "tool-a".into(),
                    title: "a".into(),
                    kind: "read".into(),
                    status: ToolStatus::Completed,
                    input: Some("a".into()),
                    output: None,
                    paths: Some(vec!["a.txt".into()]),
                    local_only: None,
                    approved_by_rule: None,
                    mcp_app: None,
                }],
                bot_exchange_ids: None,
                source_bot_id: None,
                attachments: None,
                speaker_bot_id: None,
                report_delivery_ids: None,
            });
            c.run = RunState {
                id: "run-a".into(),
                status: RunStatus::AwaitingPermission,
                todos: Some(vec![TodoItem { content: "a task".into(), status: TodoStatus::Cancelled }]),
                permission: Some(PendingPermission {
                    id: "perm-a".into(),
                    title: "a".into(),
                    kind: "edit".into(),
                    description: "a".into(),
                    options: vec![PermissionOption { id: "allow".into(), label: "Allow".into(), kind: "allow_once".into() }],
                    paths: vec!["a".into()],
                    local_only: Some(true),
                }),
                ..RunState::idle()
            };
            Ok(())
        })
        .await
        .unwrap();
    let state = f.store.snapshot();
    let public = &state.conversations[0];
    assert_eq!(public.id, conversation.id);
    assert_eq!(public.bot_id, bot.id);
    assert_eq!(public.session_id.as_deref(), Some("session-[redacted]"));
    assert_eq!(public.session_fingerprint, Some("a".repeat(64)));
    assert_eq!(public.messages[0].text, "s[redacted]y [redacted]");
    assert_eq!(public.messages[0].tools[0].id, "tool-[redacted]");
    assert_eq!(public.messages[0].tools[0].kind, "read");
    assert_eq!(public.messages[0].tools[0].paths, Some(vec!["[redacted].txt".into()]));
    assert_eq!(public.run.id, "run-[redacted]");
    assert_eq!(public.run.status, RunStatus::AwaitingPermission);
    assert_eq!(public.run.todos, Some(vec![TodoItem { content: "[redacted] t[redacted]sk".into(), status: TodoStatus::Cancelled }]));
    assert_eq!(f.disk_json()["conversations"][0]["run"]["todos"][0], serde_json::json!({"content": "a task", "status": "cancelled"}));
    let permission = public.run.permission.as_ref().unwrap();
    assert_eq!((permission.kind.as_str(), permission.options[0].kind.as_str(), permission.options[0].id.as_str()), ("edit", "allow_once", "[redacted]llow"));
    let public_bot = state.bots.iter().find(|item| item.id == bot.id).unwrap();
    assert_eq!((public_bot.name.as_str(), public_bot.color.as_str()), ("B[redacted]n[redacted]n[redacted]", "#aaaaaa"));
    assert_eq!(state.settings.base_url, "http://127.0.0.1:1/[redacted]");
    assert!(state.settings.has_api_key);
    // Disk keeps the originals.
    assert_eq!(f.disk_json()["conversations"][0]["messages"][0]["text"], "say a");
}

#[tokio::test]
async fn node_written_state_loads_round_trips_and_stays_loadable_by_node() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    let fixture = include_str!("fixtures/node-state.json");
    std::fs::write(data_dir.join("state.json"), fixture).unwrap();
    let clock: Clock = Arc::new(|| common::time("2026-02-01T00:00:00.000Z"));
    let store = Store::open(&data_dir, clock.clone()).await.unwrap();
    store.flush().await.unwrap();
    let original: serde_json::Value = serde_json::from_str(fixture).unwrap();
    let mut written: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(data_dir.join("state.json")).unwrap()).unwrap();
    assert_eq!(written["revision"].as_u64().unwrap(), original["revision"].as_u64().unwrap() + 1);
    written["revision"] = original["revision"].clone();
    assert_eq!(written, original);
    store.read(|s| {
        let disk = s.disk();
        assert_eq!(disk.bots.len(), 2);
        assert_eq!(disk.bots.iter().map(|bot| bot.skills.as_ref().map_or(0, Vec::len)).sum::<usize>(), 2);
        assert_eq!(disk.routines.len(), 3);
        assert!(matches!(disk.routines[2].schedule, RoutineSchedule::Daily { .. }));
        assert_eq!(disk.routine_runs[0].result.as_ref().unwrap().status, RoutineResultStatus::Completed);
        assert_eq!(disk.bot_deliveries[0].status, DeliveryStatus::Completed);
        assert_eq!(disk.bot_exchanges[0].messages.len(), 2);
    });
    let snapshot = serde_json::to_string(&store.snapshot()).unwrap();
    assert!(!snapshot.contains("fixture-secret-key"));
    assert!(!snapshot.contains("apiKey"));

    // Domain operations on the loaded state keep it valid for both implementations.
    let bot = store.read(|s| s.disk().bots[0].id.clone());
    let conversation = store.mutate(|s| s.add_conversation(&bot)).await.unwrap();
    store
        .mutate(|s| s.add_skill(&bot, SkillInput { name: "rust-skill".into(), description: "Added by Rust.".into(), instructions: "Body.".into() }, false))
        .await
        .unwrap();
    store.mutate(|s| s.cancel_queued_work(&conversation.id)).await.unwrap();
    store.flush().await.unwrap();
    drop(store);
    Store::open(&data_dir, clock).await.unwrap().flush().await.unwrap();
    // `RUST_STATE_OUT=/dir` keeps the Rust-written state for checking with the Node store.
    if let Ok(out) = std::env::var("RUST_STATE_OUT") {
        std::fs::create_dir_all(&out).unwrap();
        std::fs::copy(data_dir.join("state.json"), std::path::Path::new(&out).join("state.json")).unwrap();
    }
}
