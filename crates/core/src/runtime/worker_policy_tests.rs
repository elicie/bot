//! Worker discovery and guidance depend on this turn's authority, not just the bot or session.
//! These tests exercise the real store and authenticated broker without starting an engine. They
//! assert the conditional delegation policy, not whether a model will follow it on a given prompt.

use super::*;
use grok_bot_model::{BotInput, BotMessageInput, GroupInput, RoutineInput, RoutineSchedule, system_clock};
use grok_bot_store::workers::MAX_WORKERS_PER_BATCH;

struct Fixture {
    _directory: tempfile::TempDir,
    store: Arc<Store>,
    runtime: Arc<Runtime>,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path(), system_clock()).await.unwrap();
        let runtime = Runtime::new(Arc::clone(&store), CommandQueue::new(), RuntimeOptions::new(directory.path().join("engine")));
        Self { _directory: directory, store, runtime }
    }

    async fn parent(&self) -> Arc<Run> {
        let bot = self.store.read(|state| state.disk().bots[0].clone());
        let conversation = self.store.mutate(|state| state.add_conversation(&bot.id)).await.unwrap();
        self.run(&conversation.id, bot).await
    }

    async fn run(&self, conversation_id: &str, bot: Bot) -> Arc<Run> {
        let (run_id, message_id) = (new_id(), new_id());
        let speaker = self
            .store
            .mutate(|state| {
                let now = iso(state.now());
                let delivery = state.disk().bot_deliveries.iter().find(|delivery| delivery.recipient_conversation_id == conversation_id);
                let mut user = message(Role::User, "What is 2 + 2?", &now);
                if let Some(delivery) = delivery {
                    user.text = delivery.text.clone();
                    user.source_bot_id = Some(delivery.from_bot_id.clone());
                }
                let conversation = state.conversation_mut(conversation_id)?;
                let speaker = conversation.group_id.as_ref().map(|_| bot.id.clone());
                let mut assistant = message(Role::Assistant, "", &now);
                assistant.id = message_id.clone();
                assistant.speaker_bot_id = speaker.clone();
                conversation.messages.extend([user, assistant]);
                conversation.run = RunState { id: run_id.clone(), status: RunStatus::Running, started_at: Some(now), ..RunState::idle() };
                Ok(speaker)
            })
            .await
            .unwrap();
        Arc::new(Run {
            conversation_id: conversation_id.to_owned(),
            run_id,
            message_id,
            settings: self.store.read(|state| state.settings().clone()),
            bot,
            token: "worker-policy-fixture-token".into(),
            fingerprint: "worker-policy-fixture-fingerprint".into(),
            history: Vec::new(),
            attachments: Vec::new(),
            speaker,
            fresh_prompt: None,
            loaded_prompt: None,
            extra_rules: None,
            report_deliveries: Vec::new(),
            worker_id: None,
            worker_deadline: None,
            worker_report: None,
            keys: Vec::new(),
            mcp: custom_mcp::RunMcp::new(Vec::new(), Vec::new()),
            flags: Mutex::new(RunFlags { session_id: Some("worker-policy-fixture-session".into()), ..RunFlags::default() }),
            activity: tokio::sync::Notify::new(),
            stop: watch::Sender::new(false),
            done: watch::Sender::new(false),
            broker: tokio::sync::Mutex::new(()),
        })
    }

    async fn rpc(&self, run: &Arc<Run>, method: &str, params: Value) -> Value {
        assert!(self.runtime.runs().is_empty());
        self.runtime.runs().push(Arc::clone(run));
        let authorization = format!("Bearer {}", run.token);
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            self.runtime.mcp(&run.run_id, Some(&authorization), json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})),
        )
        .await;
        self.runtime.runs().clear();
        response.expect("discovery and rejected spawns must not wait for an engine or approval").unwrap().unwrap()
    }

    async fn advertisement(&self, run: &Arc<Run>, allowed: bool) -> Vec<Value> {
        assert_eq!(self.runtime.worker_spawn_allowed(run), allowed);
        let rules = self.runtime.rules(run).unwrap();
        assert_eq!(rules.matches(workers::SPAWN_GUIDANCE).count(), usize::from(allowed), "{rules}");
        assert_eq!(rules.matches("workspace__spawn_worker").count(), usize::from(allowed), "{rules}");
        // The profile is rendered for both new and loaded sessions; no engine is needed to check it.
        let meta = self.runtime.session_meta(run).unwrap();
        assert_eq!(meta["agentProfile"]["promptBody"].as_str(), Some(rules.as_str()));
        let response = self.rpc(run, "tools/list", json!({})).await;
        let tools = response["result"]["tools"].as_array().unwrap().clone();
        let names: Vec<&str> = tools.iter().map(|tool| tool["name"].as_str().unwrap()).collect();
        assert_eq!(names.iter().filter(|&&name| name == "spawn_worker").count(), usize::from(allowed), "{response}");
        for read in ["list_files", "read_file", "list_skills", "read_skill", "list_memories", "list_knowledge", "read_knowledge"] {
            assert_eq!(names.iter().filter(|&&name| name == read).count(), 1, "missing or duplicate {read}: {response}");
        }
        if run.worker_id.is_some() {
            assert_eq!(names.len(), 7, "a worker keeps exactly its seven read-only tools: {response}");
        }
        tools
    }

    async fn rejected_spawn(&self, run: &Arc<Run>, task: &str) {
        let before = self.store.read(|state| state.disk().clone());
        assert!(run.flags().approvals.is_empty());
        let response = self.rpc(run, "tools/call", json!({"name": "spawn_worker", "arguments": {"task": task}})).await;
        assert_eq!(response["result"]["isError"], true, "{response}");
        let error = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(!error.is_empty());
        assert_ne!(error, "Invalid workspace tool arguments.", "use valid arguments so the policy rejects the invocation");
        assert!(run.flags().approvals.is_empty(), "an unavailable worker must never request approval");
        self.store.read(|state| {
            assert!(state.conversation(&run.conversation_id).unwrap().run.permission.is_none());
            assert_eq!(state.disk(), &before, "rejection must not create a tool activity, permission, batch, child conversation, or revision");
        });
    }
}

fn message(role: Role, text: &str, now: &str) -> Message {
    Message {
        id: new_id(),
        role,
        text: text.into(),
        created_at: now.into(),
        tools: Vec::new(),
        bot_exchange_ids: None,
        source_bot_id: None,
        speaker_bot_id: None,
        attachments: None,
        report_delivery_ids: None,
    }
}

#[tokio::test]
async fn ordinary_parent_advertises_worker_once_with_conditional_helpful_only_guidance() {
    let f = Fixture::new().await;
    let run = f.parent().await;
    let tools = f.advertisement(&run, true).await;
    let rules = f.runtime.rules(&run).unwrap();
    for policy in [
        "Only when splitting the user's task into independent read-only subtasks would materially help",
        "you may discover workspace__spawn_worker",
        "Answer ordinary questions and small tasks directly",
        "do not delegate merely because the tool exists",
        "Each new worker needs explicit one-use approval",
    ] {
        assert!(rules.contains(policy), "missing policy {policy:?}: {rules}");
    }
    let description = tools.iter().find(|tool| tool["name"] == "spawn_worker").unwrap()["description"].as_str().unwrap();
    for policy in ["Optionally delegate", "only when splitting the user's task would materially help", "Answer ordinary questions directly"] {
        assert!(description.contains(policy), "missing policy {policy:?}: {description}");
    }
    // This is guidance, not a task-content classifier that prevents useful complex delegation.
    f.store
        .mutate(|state| {
            state.conversation_mut(&run.conversation_id)?.messages[0].text =
                "Compare the architecture of two independent components and reconcile their trade-offs.".into();
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(f.advertisement(&run, true).await, tools);
    assert_eq!(f.runtime.rules(&run).unwrap(), rules);
    f.store.read(|state| {
        state.validate_worker_source(&run.conversation_id, &run.message_id, &run.run_id, "Compare two independent parts of the user's complex task").unwrap();
        assert!(state.disk().worker_batches.is_empty());
    });
    assert!(run.flags().approvals.is_empty());
}

#[tokio::test]
async fn worker_and_automatic_report_flags_hide_spawn_before_any_durable_receipt_exists() {
    for kind in ["worker", "worker-report", "bot-report"] {
        let f = Fixture::new().await;
        let mut run = f.parent().await;
        // The ordinary store origin is eligible: each private flag must independently deny it.
        assert!(f.runtime.worker_spawn_allowed(&run));
        let context = Arc::get_mut(&mut run).unwrap();
        match kind {
            "worker" => context.worker_id = Some(new_id()),
            "worker-report" => context.worker_report = Some(new_id()),
            "bot-report" => context.report_deliveries = vec![new_id()],
            _ => unreachable!(),
        }
        f.store.read(|state| {
            assert!(state.worker_source_eligible(&run.conversation_id, &run.message_id, &run.run_id));
            assert!(state.disk().worker_batches.is_empty());
            assert!(state.disk().bot_deliveries.is_empty());
        });
        f.advertisement(&run, false).await;
        f.rejected_spawn(&run, "A valid task in an unsupported run type").await;
    }
}

#[tokio::test]
async fn group_thread_routine_and_delivery_origins_do_not_advertise_or_admit_workers() {
    for kind in ["group", "thread", "routine", "delivery"] {
        let f = Fixture::new().await;
        let bot = f.store.read(|state| state.disk().bots[0].clone());
        // Create actual source contexts through Store APIs, not a forged WorkerBatch fixture.
        let (conversation_id, bot) = match kind {
            "group" => {
                let conversation = f
                    .store
                    .mutate(|state| {
                        let other = state.add_bot(BotInput { name: "Second member".into(), instructions: None, color: None, shape: None, pinned: None })?;
                        let group = state.add_group(GroupInput {
                            name: "Policy group".into(),
                            color: None,
                            shape: None,
                            member_bot_ids: vec![bot.id.clone(), other.id],
                        })?;
                        state.add_group_conversation(&group.id)
                    })
                    .await
                    .unwrap();
                (conversation.id, bot)
            }
            "thread" => {
                let parent = f.parent().await;
                let conversation = f
                    .store
                    .mutate(|state| {
                        let now = iso(state.now());
                        let parent_conversation = state.conversation_mut(&parent.conversation_id)?;
                        parent_conversation.run.status = RunStatus::Completed;
                        parent_conversation.run.finished_at = Some(now);
                        let input_id = parent_conversation.messages[0].id.clone();
                        state.create_thread(&parent.conversation_id, &input_id)
                    })
                    .await
                    .unwrap();
                (conversation.id, bot)
            }
            "routine" => {
                let routine_run = f
                    .store
                    .mutate(|state| {
                        let routine = state.add_routine(RoutineInput {
                            bot_id: bot.id.clone(),
                            name: "Policy routine".into(),
                            prompt: "Summarize the current situation".into(),
                            schedule: RoutineSchedule::Interval { minutes: 60 },
                            enabled: false,
                        })?;
                        state.run_routine(&routine.id, state.now())
                    })
                    .await
                    .unwrap();
                (routine_run.conversation_id, bot)
            }
            "delivery" => {
                let recipient = f
                    .store
                    .mutate(|state| state.add_bot(BotInput { name: "Recipient".into(), instructions: None, color: None, shape: None, pinned: None }))
                    .await
                    .unwrap();
                let sender = f.parent().await;
                let delivery = f
                    .store
                    .mutate(|state| {
                        let delivery = state.queue_bot_message(
                            &sender.conversation_id,
                            &sender.run_id,
                            &sender.message_id,
                            BotMessageInput { to_bot_id: recipient.id.clone(), text: "A chat-only question".into(), request_id: None },
                        )?;
                        let now = iso(state.now());
                        let conversation = state.conversation_mut(&sender.conversation_id)?;
                        conversation.run.status = RunStatus::Completed;
                        conversation.run.finished_at = Some(now);
                        Ok(delivery)
                    })
                    .await
                    .unwrap();
                (delivery.recipient_conversation_id, recipient)
            }
            _ => unreachable!(),
        };
        let run = f.run(&conversation_id, bot).await;
        assert!(run.worker_id.is_none() && run.worker_report.is_none() && run.report_deliveries.is_empty());
        f.store.read(|state| {
            assert!(!state.worker_source_eligible(&run.conversation_id, &run.message_id, &run.run_id), "{kind}");
            assert!(state.disk().worker_batches.is_empty());
        });
        f.advertisement(&run, false).await;
        f.rejected_spawn(&run, "A valid task from a non-ordinary source").await;
    }
}

#[tokio::test]
async fn same_run_recomputes_guidance_and_catalog_for_ordinary_report_ordinary_context_changes() {
    let f = Fixture::new().await;
    let mut run = f.parent().await;
    let identity = (Arc::as_ptr(&run), run.run_id.clone(), run.fingerprint.clone(), run.flags().session_id.clone());
    let ordinary_catalog = f.advertisement(&run, true).await;
    let ordinary_rules = f.runtime.rules(&run).unwrap();
    for kind in ["worker-report", "bot-report", "durable-bot-report"] {
        match kind {
            "worker-report" => Arc::get_mut(&mut run).unwrap().worker_report = Some(new_id()),
            "bot-report" => Arc::get_mut(&mut run).unwrap().report_deliveries = vec![new_id()],
            "durable-bot-report" => {
                f.store
                    .mutate(|state| {
                        // An empty report marker remains meaningful after its deliveries are deleted.
                        state.conversation_mut(&run.conversation_id)?.messages[0].report_delivery_ids = Some(Vec::new());
                        Ok(())
                    })
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        f.advertisement(&run, false).await;
        f.rejected_spawn(&run, "An automatic follow-up cannot recurse").await;
        Arc::get_mut(&mut run).unwrap().worker_report = None;
        Arc::get_mut(&mut run).unwrap().report_deliveries.clear();
        f.store
            .mutate(|state| {
                state.conversation_mut(&run.conversation_id)?.messages[0].report_delivery_ids = None;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(f.advertisement(&run, true).await, ordinary_catalog, "{kind}");
        assert_eq!(f.runtime.rules(&run).unwrap(), ordinary_rules, "{kind}");
        assert_eq!((Arc::as_ptr(&run), run.run_id.clone(), run.fingerprint.clone(), run.flags().session_id.clone()), identity);
    }
}

#[tokio::test]
async fn authority_advertisement_does_not_depend_on_worker_quota_or_store_writability() {
    let f = Fixture::new().await;
    let run = f.parent().await;
    let ordinary_catalog = f.advertisement(&run, true).await;
    let ordinary_rules = f.runtime.rules(&run).unwrap();
    for index in 0..MAX_WORKERS_PER_BATCH {
        f.store.mutate(|state| state.spawn_worker(&run.conversation_id, &run.message_id, &run.run_id, &format!("Independent check {index}"))).await.unwrap();
    }
    let task = "A fourth independent check";
    assert_eq!(f.store.read(|state| state.validate_worker_source(&run.conversation_id, &run.message_id, &run.run_id, task)).unwrap_err().status, 409);
    assert_eq!(f.advertisement(&run, true).await, ordinary_catalog);
    f.rejected_spawn(&run, task).await;

    // Fail atomic replacement deterministically even when tests run as root. Both paths belong to
    // this fixture's private temporary directory; no later reopen or restoration is needed.
    let path = f.store.data_dir().join("state.json");
    let saved = f.store.data_dir().join("state.worker-policy-saved.json");
    assert!(path.is_absolute() && path.is_file() && saved.is_absolute() && !saved.exists());
    std::fs::rename(&path, &saved).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert_eq!(
        f.store
            .mutate(|state| {
                state.disk_mut();
                Ok(())
            })
            .await
            .unwrap_err()
            .status,
        500
    );
    f.store.read(|state| {
        assert_eq!(state.assert_writable().unwrap_err().status, 503);
        assert!(state.worker_source_eligible(&run.conversation_id, &run.message_id, &run.run_id));
        assert_eq!(state.validate_worker_source(&run.conversation_id, &run.message_id, &run.run_id, task).unwrap_err().status, 503);
    });
    assert_eq!(f.advertisement(&run, true).await, ordinary_catalog);
    assert_eq!(f.runtime.rules(&run).unwrap(), ordinary_rules);
    f.rejected_spawn(&run, task).await;
}
