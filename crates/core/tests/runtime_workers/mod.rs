//! App-managed worker scenarios in the shared real-engine/mock-provider test process.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use std::time::Duration;

use grok_bot_core::queue::CommandQueue;
use grok_bot_core::scheduler::{RoutineScheduler, SchedulerOptions};
use grok_bot_model::{DeliveryStatus, PermissionAction, PermissionCategory, PermissionRuleInput, ReportStatus, RunStatus, WorkerBatch, system_clock};
use serde_json::{Value, json};

use super::{Fixture, Reply, content_text, discover, messages, serve, text, tool_outcomes, use_tool, user_texts};

async fn custom_mcp(State(requests): State<Arc<AtomicUsize>>, Json(request): Json<Value>) -> Response {
    requests.fetch_add(1, Ordering::SeqCst);
    let Some(id) = request.get("id") else { return StatusCode::ACCEPTED.into_response() };
    let result = match request["method"].as_str() {
        Some("initialize") => {
            json!({"protocolVersion": request["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "worker-isolation-fixture", "version": "1"}})
        }
        Some("tools/list") => json!({"tools": [{"name": "echo", "description": "Fixture echo", "inputSchema": {"type": "object", "properties": {}}}]}),
        _ => json!({}),
    };
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

fn scheduler(f: &Fixture) -> Arc<RoutineScheduler> {
    RoutineScheduler::new(
        Arc::clone(&f.store),
        Arc::clone(&f.runtime),
        CommandQueue::new(),
        SchedulerOptions { now: system_clock(), poll_interval: Duration::from_secs(3600), auto_start: false },
    )
}

fn batch(f: &Fixture, parent: &str) -> WorkerBatch {
    f.store.read(|s| s.disk().worker_batches.iter().find(|batch| batch.parent_conversation_id == parent).unwrap().clone())
}

async fn queue(f: &Fixture, parent: &str, tasks: &[&str]) -> WorkerBatch {
    let mut plan: Vec<Reply> = tasks.iter().map(|task| use_tool("spawn_worker", json!({"task": task}))).collect();
    plan.push(text("Delegated the tasks."));
    f.provider.plan(plan);
    f.runtime.start(parent, "Split this read-only task.").await.unwrap();
    for _ in tasks {
        let card = f.decide(parent, "allow_once").await;
        assert_eq!(card.options.iter().find(|o| o.kind == "allow_once").unwrap().label, "Run worker once");
    }
    f.completed(parent).await;
    batch(f, parent)
}

/// Three bounded serial workers, exact retries without another approval, read-only enforcement,
/// separate sessions, hidden execution conversations, and one grouped follow-up only.
pub(super) async fn worker_batches(f: &Fixture) {
    let bot = f.bot_id();
    let requests = Arc::new(AtomicUsize::new(0));
    let origin = serve(Router::new().route("/mcp", post(custom_mcp)).with_state(Arc::clone(&requests))).await;
    f.store.mutate(|s| s.set_mcp_servers(&bot, &json!({"mcpServers": {"custom": {"url": format!("{origin}/mcp")}}}).to_string(), true)).await.unwrap();
    f.store.mutate(|s| s.set_mcp_server_enabled(&bot, "custom", true, true)).await.unwrap();
    f.store
        .mutate(|s| s.update_bot(&bot, grok_bot_model::BotPatch { instructions: Some("SAME_BOT_WORKER_INSTRUCTIONS".into()), ..Default::default() }))
        .await
        .unwrap();
    for category in PermissionCategory::ALL {
        f.store
            .mutate(|s| s.add_permission_rule(PermissionRuleInput { category, action: PermissionAction::Allow, bot_id: None, server: None }, true))
            .await
            .unwrap();
    }
    let parent = f.conversation(&bot).await;
    let dispatcher = scheduler(f);
    f.provider.plan(vec![
        discover("workspace spawn_worker"),
        use_tool("spawn_worker", json!({"task": "Read task A"})),
        use_tool("spawn_worker", json!({"task": "  Read task A  "})),
        use_tool("spawn_worker", json!({"task": "Read task B"})),
        use_tool("spawn_worker", json!({"task": "Read task C"})),
        use_tool("spawn_worker", json!({"task": "Fourth task is rejected"})),
        use_tool("spawn_worker", json!({"task": "x".repeat(4001)})),
        Reply::Hold,
    ]);
    f.runtime.start(&parent, "Split into three distinct read-only workers.").await.unwrap();
    for _ in 0..3 {
        f.decide(&parent, "allow_once").await;
    }
    f.wait(&parent, "all queued task receipts", |c| {
        c.run.status == RunStatus::Running
            && c.messages.last().unwrap().tools.iter().filter(|tool| tool.output.as_deref().is_some_and(|output| output.starts_with("Queued worker "))).count()
                == 3
            && f.provider.state.lock().unwrap().plan.is_empty()
    })
    .await;
    let queued = batch(f, &parent);
    assert_eq!(queued.workers.len(), 3, "duplicate and rejected tasks create nothing");
    assert!(queued.released_at.is_none());
    assert!(queued.workers.iter().all(|worker| worker.status == DeliveryStatus::Queued));
    dispatcher.tick().await.unwrap();
    assert!(queued.workers.iter().all(|worker| f.get(&worker.conversation_id).messages.is_empty()), "children wait for the parent");
    assert_eq!(f.runtime.start_worker(&queued.workers[0].conversation_id).await.unwrap_err().status, 409);
    f.provider.release.notify_waiters();
    let source = f.completed(&parent).await;
    let inference = f.provider.inference();
    // Earlier requests repeat tool history; inspect the final request's results exactly once.
    let outcomes = tool_outcomes(&inference[inference.len() - 1..]);
    let receipts: Vec<_> = outcomes.iter().filter(|value| value.contains("workerId")).collect();
    assert_eq!(receipts.len(), 4, "the duplicate returns its prior receipt");
    assert_eq!(receipts[0], receipts[1]);
    assert!(outcomes.iter().any(|value| value.contains("at most three")));
    assert!(outcomes.iter().any(|value| value.contains("task") && value.contains("4000")));
    assert!(source.messages[1].tools.iter().all(|tool| tool.approved_by_rule.is_none()), "spawn always asks despite all allow rules");
    assert!(batch(f, &parent).released_at.is_some());
    assert_eq!(f.public().conversations.len(), 1, "worker execution conversations are hidden");
    let custom_before_workers = requests.load(Ordering::SeqCst);
    assert!(custom_before_workers > 0, "the ordinary parent initializes its enabled custom MCP server");

    f.provider.clear();
    f.provider.plan(vec![
        use_tool("write_file", json!({"path": "notes.txt", "content": "FORBIDDEN"})),
        use_tool("create_bot", json!({"name": "Forbidden worker bot"})),
        use_tool("save_memory", json!({"text": "Forbidden worker memory"})),
        use_tool("create_skill", json!({"name": "forbidden", "description": "x", "instructions": "x"})),
        use_tool("send_message", json!({"toBotId": bot, "text": "Forbidden"})),
        use_tool("computer_observe", json!({})),
        use_tool("spawn_worker", json!({"task": "Forbidden nested worker"})),
        use_tool("list_routines", json!({})),
        use_tool("list_skills", json!({})),
        use_tool("list_memories", json!({})),
        use_tool("list_knowledge", json!({})),
        use_tool("read_file", json!({"path": "notes.txt"})),
        text("WORKER_A_ANSWER"),
    ]);
    dispatcher.tick().await.unwrap();
    let first = f.completed(&queued.workers[0].conversation_id).await;
    assert_eq!(first.messages[1].text, "WORKER_A_ANSWER");
    assert!(first.run.permission.is_none());
    let worker_rules: String = f
        .provider
        .inference()
        .iter()
        .flat_map(messages)
        .filter(|message| message["role"] == "system")
        .map(|message| content_text(&message["content"]))
        .collect();
    assert!(worker_rules.contains("SAME_BOT_WORKER_INSTRUCTIONS"));
    assert!(!worker_rules.contains("Enabled custom MCP servers"));
    assert_eq!(f.notes(), "original\n");
    assert_eq!(f.store.read(|s| s.disk().bots.len()), 1);
    assert_eq!(batch(f, &parent).workers.len(), 3);
    assert!(batch(f, &parent).report.is_none(), "first sibling never triggers an early report");
    assert_eq!(f.get(&parent).messages.len(), 2);
    let outcomes = tool_outcomes(&f.provider.inference());
    assert!(outcomes.iter().any(|value| value.contains("original")), "safe file reads succeed: {outcomes:?}");
    assert!(
        outcomes
            .iter()
            .filter(|value| value.contains("read-only") || value.contains("not found") || value.contains("not available") || value.contains("Unknown tool"))
            .count()
            >= 1,
        "mutating tools are unavailable: {outcomes:?}"
    );
    assert_eq!(f.runtime.start(&queued.workers[0].conversation_id, "Hijack worker").await.unwrap_err().status, 409);

    f.provider.plan(vec![Reply::Hold]);
    dispatcher.tick().await.unwrap();
    f.wait(&queued.workers[1].conversation_id, "the second worker", |c| c.run.status == RunStatus::Running && f.provider.state.lock().unwrap().plan.is_empty())
        .await;
    assert_eq!(f.runtime.start_worker(&queued.workers[2].conversation_id).await.unwrap_err().status, 409);
    assert_eq!(f.store.read(|s| s.disk().conversations.iter().filter(|c| c.run.status.is_active()).count()), 1);
    assert!(batch(f, &parent).report.is_none());
    f.provider.release.notify_waiters();
    f.completed(&queued.workers[1].conversation_id).await;
    f.provider.plan(vec![text("WORKER_C_ANSWER")]);
    dispatcher.tick().await.unwrap();
    let third = f.completed(&queued.workers[2].conversation_id).await;
    assert_ne!(first.session_id, third.session_id);
    assert_ne!(source.session_id, third.session_id);
    assert_eq!(requests.load(Ordering::SeqCst), custom_before_workers, "workers never initialize or call custom MCP servers");
    assert_eq!(batch(f, &parent).report.as_ref().unwrap().status, ReportStatus::Queued);

    f.provider.clear();
    f.provider.plan(vec![
        use_tool("write_file", json!({"path": "notes.txt", "content": "Report must ask"})),
        use_tool("spawn_worker", json!({"task": "A report cannot create another batch"})),
        text("SYNTHESIZED_ONCE"),
    ]);
    dispatcher.tick().await.unwrap();
    f.wait(&parent, "the grouped worker follow-up", |c| c.messages.len() == 4).await;
    let card = f.decide(&parent, "reject_once").await;
    assert!(card.description.contains("Report must ask"), "worker report ignores allow rules");
    let reported = f.completed(&parent).await;
    assert_eq!(reported.messages[3].text, "SYNTHESIZED_ONCE");
    assert_eq!(f.notes(), "original\n");
    let done = batch(f, &parent);
    assert_eq!(done.report.as_ref().unwrap().status, ReportStatus::Completed);
    assert_eq!(done.report_input_id.as_deref(), Some(reported.messages[2].id.as_str()));
    assert!(
        user_texts(&f.provider.inference())
            .iter()
            .any(|input| input.contains("WORKER_A_ANSWER") && input.contains("WORKER_C_ANSWER") && input.contains("untrusted conversation data"))
    );
    assert!(f.public().conversations.iter().all(|c| c.messages.iter().all(|m| Some(&m.id) != done.report_input_id.as_ref())));
    let before = (reported.run.id.clone(), reported.messages.len());
    dispatcher.tick().await.unwrap();
    assert_eq!((f.get(&parent).run.id, f.get(&parent).messages.len()), before);
    assert_eq!(f.runtime.start_worker_report(&done.id).await.unwrap_err().status, 409);
    f.store.flush().await.unwrap();
    let reopened = grok_bot_store::Store::open(&f.root.join("data"), system_clock()).await.unwrap();
    assert_eq!(reopened.read(|s| s.worker_batch(&done.id).unwrap().report.as_ref().unwrap().status), ReportStatus::Completed);
    dispatcher.close().await;
}

/// Stop before admission, between turns, during a child, and while a report is pending all cascade
/// through the durable batch; no cancelled task/report is scheduled again.
pub(super) async fn worker_cancellation(f: &Fixture) {
    let dispatcher = scheduler(f);
    let parent = f.conversation(&f.bot_id()).await;
    f.provider.plan(vec![use_tool("spawn_worker", json!({"task": "Cancelled before approval"})), text("Must not execute")]);
    f.runtime.start(&parent, "Delegate, then stop.").await.unwrap();
    f.pending(&parent).await;
    f.runtime.cancel(&parent).await.unwrap();
    assert!(f.store.read(|s| s.disk().worker_batches.is_empty()));

    let failed_parent = f.conversation(&f.bot_id()).await;
    f.provider.plan(vec![use_tool("spawn_worker", json!({"task": "Never run after parent failure"})), Reply::Status(500, "parent failed".into())]);
    f.runtime.start(&failed_parent, "Delegate, then fail.").await.unwrap();
    f.decide(&failed_parent, "allow_once").await;
    assert_eq!(f.settled(&failed_parent).await.run.status, RunStatus::Error);
    let failed = batch(f, &failed_parent);
    assert!(failed.cancelled && failed.workers[0].status == DeliveryStatus::Cancelled && failed.released_at.is_none());

    let between = f.conversation(&f.bot_id()).await;
    queue(f, &between, &["Queued A", "Queued B"]).await;
    f.runtime.cancel(&between).await.unwrap();
    let cancelled = batch(f, &between);
    assert!(cancelled.cancelled && cancelled.workers.iter().all(|worker| worker.status == DeliveryStatus::Cancelled));
    let requests = f.provider.inference_len();
    dispatcher.tick().await.unwrap();
    assert_eq!(f.provider.inference_len(), requests);

    let active = f.conversation(&f.bot_id()).await;
    let queued = queue(f, &active, &["Running A", "Never start B"]).await;
    f.provider.plan(vec![Reply::Hold]);
    dispatcher.tick().await.unwrap();
    f.wait(&queued.workers[0].conversation_id, "an active child", |c| c.run.status == RunStatus::Running).await;
    f.runtime.cancel(&active).await.unwrap();
    f.provider.release.notify_waiters();
    assert_eq!(f.get(&queued.workers[0].conversation_id).run.status, RunStatus::Cancelled);
    assert!(batch(f, &active).workers.iter().all(|worker| worker.status == DeliveryStatus::Cancelled));
    assert!(batch(f, &active).report.is_none());
    dispatcher.tick().await.unwrap();
    assert!(f.get(&queued.workers[1].conversation_id).messages.is_empty());

    let waiting = f.conversation(&f.bot_id()).await;
    let queued = queue(f, &waiting, &["Answer without synthesis"]).await;
    f.provider.plan(vec![text("A result")]);
    dispatcher.tick().await.unwrap();
    f.completed(&queued.workers[0].conversation_id).await;
    assert_eq!(batch(f, &waiting).report.as_ref().unwrap().status, ReportStatus::Queued);
    f.runtime.cancel(&waiting).await.unwrap();
    assert_eq!(batch(f, &waiting).report.as_ref().unwrap().status, ReportStatus::Skipped);
    dispatcher.tick().await.unwrap();
    assert_eq!(f.get(&waiting).messages.len(), 2);

    let reporting = f.conversation(&f.bot_id()).await;
    let queued = queue(f, &reporting, &["Answer before stopping synthesis"]).await;
    f.provider.plan(vec![text("A synthesis input")]);
    dispatcher.tick().await.unwrap();
    f.completed(&queued.workers[0].conversation_id).await;
    f.provider.plan(vec![Reply::Hold]);
    dispatcher.tick().await.unwrap();
    f.wait(&reporting, "an active worker report", |c| {
        c.messages.len() == 4 && c.run.status == RunStatus::Running && f.provider.state.lock().unwrap().plan.is_empty()
    })
    .await;
    f.runtime.cancel(&reporting).await.unwrap();
    f.provider.release.notify_waiters();
    assert_eq!(batch(f, &reporting).report.as_ref().unwrap().status, ReportStatus::Cancelled);
    dispatcher.tick().await.unwrap();
    assert_eq!(f.get(&reporting).messages.len(), 4);
    f.store.flush().await.unwrap();
    let reopened = grok_bot_store::Store::open(&f.root.join("data"), system_clock()).await.unwrap();
    assert_eq!(reopened.read(|s| s.worker_batch(&queued.id).unwrap().report.as_ref().unwrap().status), ReportStatus::Cancelled);
    dispatcher.close().await;
}

/// File authority is fixed by the originating approval; a later folder selection grants no new
/// workspace to queued or legacy workers.
pub(super) async fn worker_workspace_authorization(f: &Fixture) {
    let dispatcher = scheduler(f);
    let parent = f.conversation(&f.bot_id()).await;
    let approved = queue(f, &parent, &["Read the approved notes only"]).await;
    assert_eq!(approved.authorized_workspace.as_deref(), Some(f.workspace.to_str().unwrap()));
    let later = f.root.join("later-workspace");
    std::fs::create_dir_all(&later).unwrap();
    std::fs::write(later.join("notes.txt"), "UNAUTHORIZED_LATER_WORKSPACE_CANARY").unwrap();
    f.store
        .mutate(|state| {
            state.update_settings(grok_bot_model::SettingsInput {
                base_url: f.base_url.clone(),
                model: "local-test-model".into(),
                workspace: later.to_string_lossy().into_owned(),
                api_key: None,
                clear_api_key: None,
            })
        })
        .await
        .unwrap();
    f.provider.clear();
    f.provider.plan(vec![use_tool("read_file", json!({"path": "notes.txt"})), text("Used the approved folder.")]);
    dispatcher.tick().await.unwrap();
    f.completed(&approved.workers[0].conversation_id).await;
    let outcomes = tool_outcomes(&f.provider.inference());
    assert!(outcomes.iter().any(|output| output.contains("original")), "{outcomes:?}");
    assert!(outcomes.iter().all(|output| !output.contains("UNAUTHORIZED_LATER_WORKSPACE_CANARY")));
    f.provider.plan(vec![text("One grouped authorized result.")]);
    dispatcher.tick().await.unwrap();
    f.completed(&parent).await;

    let legacy_parent = f.conversation(&f.bot_id()).await;
    let legacy = queue(f, &legacy_parent, &["An old queued task has no saved folder authorization"]).await;
    f.store
        .mutate(|state| {
            state.disk_mut().worker_batches.iter_mut().find(|batch| batch.id == legacy.id).unwrap().authorized_workspace = None;
            Ok(())
        })
        .await
        .unwrap();
    f.provider.clear();
    f.provider.plan(vec![use_tool("read_file", json!({"path": "notes.txt"})), text("Legacy worker did not access files.")]);
    dispatcher.tick().await.unwrap();
    f.completed(&legacy.workers[0].conversation_id).await;
    let outcomes = tool_outcomes(&f.provider.inference());
    assert!(outcomes.iter().any(|output| output.contains("Choose a workspace")), "{outcomes:?}");
    assert!(outcomes.iter().all(|output| !output.contains("UNAUTHORIZED_LATER_WORKSPACE_CANARY")));
    f.runtime.cancel(&legacy_parent).await.unwrap();
    dispatcher.close().await;
}

/// The worker deadline is absolute and shorter here than the idle timeout. A failed worker is
/// included in the grouped result, and a report that cannot start is final rather than retried.
pub(super) async fn worker_deadline(f: &Fixture) {
    let dispatcher = scheduler(f);
    let parent = f.conversation(&f.bot_id()).await;
    let queued = queue(f, &parent, &["Slow read-only task"]).await;
    f.provider.plan(vec![Reply::Hold]);
    dispatcher.tick().await.unwrap();
    let failed = f.settled(&queued.workers[0].conversation_id).await;
    f.provider.release.notify_waiters();
    assert_eq!(failed.run.status, RunStatus::Error);
    assert!(failed.run.error.as_deref().unwrap().contains("absolute execution deadline"));
    assert_eq!(batch(f, &parent).workers[0].status, DeliveryStatus::Error);
    f.runtime.set_origin(String::new());
    dispatcher.tick().await.unwrap();
    f.runtime.set_origin(f.origin.clone());
    let failed_report = batch(f, &parent);
    assert_eq!(failed_report.report.as_ref().unwrap().status, ReportStatus::Error);
    assert!(failed_report.report_input_id.is_none());
    dispatcher.tick().await.unwrap();
    assert_eq!(f.get(&parent).messages.len(), 2);
    dispatcher.close().await;
}
