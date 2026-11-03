//! Checklist projection through the real engine, HTTP state API, and persisted server state.

use super::common::TestApp;
use super::{Duration, Instant, Path, Provider, Reply, Value, json, start_provider, text, tool_names, tool_outcomes};

fn conversation(state: &Value, id: &str) -> Value {
    state["conversations"].as_array().unwrap().iter().find(|c| c["id"] == id).unwrap().clone()
}

async fn state(app: &TestApp, id: &str) -> Value {
    let (status, state) = app.api("GET", "/api/state", None).await;
    assert_eq!(status, 200);
    conversation(&state, id)
}

async fn wait(app: &TestApp, id: &str, predicate: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let current = state(app, id).await;
        if predicate(&current) {
            return current;
        }
        assert!(Instant::now() < deadline, "checklist state did not settle: {current}");
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

pub(super) async fn checklist_updates(base: &Path, engine: &Path) {
    let provider = Provider::default();
    let base_url = start_provider(provider.clone()).await;
    let data = base.join("todos").join("data");
    let app = TestApp::start(engine, data.clone()).await;
    let key = "todo-provider-secret-canary";
    let (status, _) = app
        .api(
            "PUT",
            "/api/settings",
            Some(json!({
                "baseUrl": base_url, "model": "local-test-model", "workspace": "", "apiKey": key,
            })),
        )
        .await;
    assert_eq!(status, 200);
    let (_, initial_state) = app.api("GET", "/api/state", None).await;
    let bot = initial_state["bots"][0]["id"].as_str().unwrap();
    let (status, created) = app.api("POST", "/api/conversations", Some(json!({"botId": bot}))).await;
    assert_eq!(status, 201);
    let id = created["conversation"]["id"].as_str().unwrap().to_owned();
    assert!(created["conversation"]["run"].get("todos").is_none(), "legacy/idle state omits todos");

    let initial = json!([
        {"content": "Inspect [redacted]", "status": "in_progress"},
        {"content": "Implement", "status": "pending"},
    ]);
    let merged = json!([
        {"content": "Inspect [redacted]", "status": "completed"},
        {"content": "Implement", "status": "pending"},
        {"content": "Verify", "status": "pending"},
    ]);
    let latest = json!([
        {"content": "Verify", "status": "completed"},
        {"content": "Optional step", "status": "cancelled"},
    ]);
    let partial = json!({"todos": [
        {"id": "inspect", "status": "completed"},
        {"id": "verify", "content": "Verify"},
    ]});
    let mut updates = app.store.subscribe();
    provider.plan(vec![
        Reply::Tool(
            "todo_write",
            json!({"merge": false, "todos": [
                {"id": "inspect", "content": format!("Inspect {key}"), "status": "in_progress"},
                {"id": "implement", "content": "Implement", "status": "pending"},
            ]}),
        ),
        // Default merge=true accepts status-only updates; the ACP Plan contains the full list.
        Reply::Tool("todo_write", partial.clone()),
        Reply::Tool("todo_write", partial),
        // Rejected raw tool input must not become checklist state.
        Reply::Tool(
            "todo_write",
            json!({"merge": false, "todos": [
                {"id": "duplicate", "content": "Rejected first", "status": "pending"},
                {"id": "duplicate", "content": "Rejected second", "status": "pending"},
            ]}),
        ),
        Reply::Tool("todo_write", json!({"merge": false, "todos": []})),
        Reply::Tool(
            "todo_write",
            json!({"merge": false, "todos": [
                {"id": "verify", "content": "Verify", "status": "completed"},
                {"id": "optional", "content": "Optional step", "status": "cancelled"},
            ]}),
        ),
        Reply::Hold,
    ]);
    let route = format!("/api/conversations/{id}/messages");
    assert_eq!(app.api("POST", &route, Some(json!({"text": "Track this multi-step task."}))).await.0, 202);
    let live = wait(&app, &id, |c| c["run"]["todos"] == latest).await;
    assert_eq!(live["run"]["status"], "running");
    assert!(live["run"].get("permission").is_none(), "todo_write requires no browser approval");

    let mut snapshots = Vec::new();
    while let Ok(snapshot) = updates.try_recv() {
        snapshots.push(serde_json::to_value(&*snapshot).unwrap());
    }
    let checklists = snapshots.iter().map(|s| conversation(s, &id)["run"]["todos"].clone()).collect::<Vec<_>>();
    assert!(checklists.contains(&initial), "initial plan was broadcast: {checklists:?}");
    assert!(checklists.contains(&merged), "partial and repeated writes preserve the merged list: {checklists:?}");
    assert!(checklists.contains(&json!([])), "empty plans clear the list");
    for snapshot in &snapshots {
        let wire = snapshot.to_string();
        assert!(!wire.contains(key));
        let current = conversation(snapshot, &id);
        let checklist = current["run"]["todos"].to_string();
        assert!(!checklist.contains("Rejected first"));
        assert!(!checklist.contains("Rejected second"));
        assert!(current["run"]["todos"].as_array().is_none_or(|todos| todos.len() <= 3), "repeated plans never append lists");
        assert!(current["run"].get("permission").is_none());
    }
    for request in provider.inference() {
        assert_eq!(tool_names(&request), ["search_tool", "todo_write", "use_tool"]);
    }
    assert!(tool_outcomes(&provider.inference()).iter().any(|output| output.contains("Duplicate todo ID")));
    provider.release.notify_one();
    let completed = wait(&app, &id, |c| c["run"]["status"] == "completed").await;
    assert_eq!(completed["run"]["todos"], latest);
    // Fresh HTTP requests model browser reload: only the latest normalized list is returned.
    assert_eq!(state(&app, &id).await["run"]["todos"], latest);
    assert_eq!(state(&app, &id).await["run"]["todos"], latest);
    let persisted: Value = serde_json::from_str(&std::fs::read_to_string(data.join("state.json")).unwrap()).unwrap();
    assert_eq!(conversation(&persisted, &id)["run"]["todos"], latest);

    // New turns retain chat context but display only their own task IDs, even when the native
    // tool defaults to merge=true against persisted engine resources.
    let unfinished = json!([{"content": "New task still in progress", "status": "in_progress"}]);
    // HTTP can observe a terminal in-memory run before its final ordered write broadcasts.
    // Finish that previous turn before subscribing to snapshots attributed to the next turn.
    app.store.flush().await.unwrap();
    let mut next_updates = app.store.subscribe();
    provider.plan(vec![
        Reply::Tool(
            "todo_write",
            json!({"todos": [
                {"id": "new-turn-task", "content": "New task still in progress", "status": "in_progress"},
            ]}),
        ),
        text("This task is not finished yet."),
    ]);
    assert_eq!(app.api("POST", &route, Some(json!({"text": "Start a different task."}))).await.0, 202);
    let next = wait(&app, &id, |c| c["run"]["status"] == "completed").await;
    assert_eq!(next["sessionId"], completed["sessionId"], "new-turn checklist reset keeps conversation context");
    assert_eq!(next["run"]["todos"], unfinished, "transient turn-end cleanup must not falsely complete work");
    while let Ok(snapshot) = next_updates.try_recv() {
        let snapshot = serde_json::to_value(&*snapshot).unwrap();
        let current = conversation(&snapshot, &id);
        if let Some(todos) = current["run"].get("todos") {
            assert_eq!(todos, &unfinished, "neither old tasks nor cosmetic completion may be broadcast");
        }
    }
    let persisted: Value = serde_json::from_str(&std::fs::read_to_string(data.join("state.json")).unwrap()).unwrap();
    assert_eq!(conversation(&persisted, &id)["run"]["todos"], unfinished);
    app.close().await;

    let app = TestApp::start(engine, data).await;
    assert_eq!(state(&app, &id).await["run"]["todos"], unfinished, "server restart retains the model's last actual checklist status");
    // A cold engine must also isolate the new turn from persisted native todo resources.
    let cold = json!([{"content": "Only the cold-restart task", "status": "pending"}]);
    provider.plan(vec![
        Reply::Tool(
            "todo_write",
            json!({"todos": [
                {"id": "cold-restart-task", "content": "Only the cold-restart task", "status": "pending"},
            ]}),
        ),
        text("The new checklist is ready."),
    ]);
    assert_eq!(app.api("POST", &route, Some(json!({"text": "Track a task after server restart."}))).await.0, 202);
    let cold_turn = wait(&app, &id, |c| c["run"]["status"] == "completed").await;
    assert_eq!(cold_turn["run"]["todos"], cold, "persisted previous-turn IDs must not leak into a cold-engine checklist");

    let before = provider.inference_len();
    provider.plan(vec![Reply::Hold]);
    assert_eq!(app.api("POST", &route, Some(json!({"text": "A new turn without a checklist."}))).await.0, 202);
    wait(&app, &id, |c| c["run"]["status"] == "running" && provider.inference_len() > before).await;
    assert!(state(&app, &id).await["run"].get("todos").is_none(), "loaded session plans do not leak into a new turn");
    assert_eq!(app.api("POST", &format!("/api/conversations/{id}/cancel"), Some(json!({}))).await.0, 200);
    provider.release.notify_one();
    assert!(state(&app, &id).await["run"].get("todos").is_none());

    provider.plan(vec![Reply::Tool("todo_write", json!({"merge": false, "todos": []})), text("Cleared.")]);
    assert_eq!(app.api("POST", &route, Some(json!({"text": "Explicitly clear the checklist."}))).await.0, 202);
    assert_eq!(wait(&app, &id, |c| c["run"]["status"] == "completed").await["run"]["todos"], json!([]));
    app.close().await;
    eprintln!("[runtime] todos HTTP projection, reload, and restart ok");
}
