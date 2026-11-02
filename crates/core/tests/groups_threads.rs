//! Group chats and threads end to end: the HTTP routes, the dispatcher, the runtime, and the real
//! in-process engine against a mock OpenAI-compatible provider. Proves speaker order, attribution,
//! per-member sessions that see the other members' replies, mention routing, cancellation of the
//! remaining members, and thread context.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use common::{TestApp, run};
use grok_bot_model::{AppState, Conversation, RunStatus};
use serde_json::{Value, json};

#[derive(Default)]
struct ProviderState {
    /// (speaker, latest prompt) of each main agent request.
    requests: Vec<(String, String)>,
    /// Hold the request of this speaker until released.
    hold: Option<String>,
    held: bool,
}

#[derive(Clone, Default)]
struct Provider {
    state: Arc<Mutex<ProviderState>>,
    release: Arc<tokio::sync::Notify>,
}

impl Provider {
    fn requests(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().requests.clone()
    }
    fn clear(&self) {
        self.state.lock().unwrap().requests.clear();
    }
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// The member addressed by the latest group prompt (`You are <name>.`), or `direct` outside groups.
fn speaker(request: &Value) -> String {
    let prompt = turn_prompt(request);
    prompt
        .split("You are ")
        .nth(1)
        .and_then(|rest| rest.split_once(". Everything quoted").map(|(name, _)| name.to_owned()))
        .unwrap_or_else(|| "direct".into())
}

/// The latest prompt the engine sent (inside `<user_query>`).
fn turn_prompt(request: &Value) -> String {
    let query = request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .filter(|m| m["role"] == "user")
        .map(|m| content_text(&m["content"]))
        .find(|text| text.contains("<user_query>"))
        .unwrap_or_default();
    let start = query.find("<user_query>\n").map_or(0, |index| index + "<user_query>\n".len());
    let end = query.rfind("\n</user_query>").unwrap_or(query.len());
    query[start..end.max(start)].to_owned()
}

fn sse(model: &str, content: &str) -> Response {
    let chunk = |delta: Value, reason: Value| json!({"id": "chatcmpl-local", "object": "chat.completion.chunk", "created": 1, "model": model, "choices": [{"index": 0, "delta": delta, "finish_reason": reason}]});
    let mut last = chunk(json!({}), json!("stop"));
    last["usage"] = json!({"prompt_tokens": 100, "completion_tokens": 12, "total_tokens": 112});
    let mut body = String::new();
    for value in [chunk(json!({"role": "assistant"}), Value::Null), chunk(json!({"content": content}), Value::Null), last] {
        body.push_str(&format!("data: {value}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

async fn completions(State(provider): State<Provider>, Json(request): Json<Value>) -> Response {
    let model = request["model"].as_str().unwrap_or_default().to_owned();
    let main = request["tools"].as_array().into_iter().flatten().any(|t| t["function"]["name"] == "search_tool");
    if !main {
        return sse(&model, "Fixture title");
    }
    let name = speaker(&request);
    let hold = {
        let mut state = provider.state.lock().unwrap();
        state.requests.push((name.clone(), turn_prompt(&request)));
        let hold = state.hold.as_deref() == Some(name.as_str());
        if hold {
            state.held = true;
        }
        hold
    };
    if hold {
        let _ = tokio::time::timeout(Duration::from_secs(60), provider.release.notified()).await;
    }
    sse(&model, &format!("Reply from {name}"))
}

async fn start_provider(provider: Provider) -> String {
    let router = Router::new()
        .route("/v1/chat/completions", post(completions))
        .route("/v1/models", get(|| async { Json(json!({"object": "list", "data": [{"id": "local-test-model", "object": "model"}]})) }))
        .with_state(provider);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("{origin}/v1")
}

struct Harness {
    app: TestApp,
    provider: Provider,
}

impl Harness {
    fn state(&self) -> AppState {
        self.app.store.snapshot()
    }

    fn conversation(&self, id: &str) -> Conversation {
        self.state().conversations.into_iter().find(|c| c.id == id).unwrap()
    }

    async fn wait(&self, what: &str, predicate: impl Fn(&AppState) -> bool) -> AppState {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let state = self.state();
            if predicate(&state) {
                return state;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}: {}", serde_json::to_string(&state.conversations).unwrap());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Wait until the conversation has `count` messages, no active run, and no queued member.
    async fn settled(&self, id: &str, count: usize) -> Conversation {
        self.wait("a settled conversation", |state| {
            state.conversations.iter().any(|c| c.id == id && c.messages.len() == count && c.run.status.is_terminal() && c.group_round.is_none())
        })
        .await;
        self.conversation(id)
    }

    async fn post(&self, route: &str, body: Value) -> (u16, Value) {
        self.app.api("POST", route, Some(body)).await
    }

    async fn send(&self, conversation: &str, text: &str) -> (u16, Value) {
        self.post(&format!("/api/conversations/{conversation}/messages"), json!({ "text": text })).await
    }

    async fn bot(&self, name: &str) -> String {
        let (status, state) = self.post("/api/bots", json!({ "name": name, "instructions": format!("{name} private instructions") })).await;
        assert_eq!(status, 201, "{state}");
        state["bots"].as_array().unwrap().iter().find(|bot| bot["name"] == name).unwrap()["id"].as_str().unwrap().to_owned()
    }
}

fn speakers(conversation: &Conversation) -> Vec<Option<String>> {
    conversation.messages.iter().map(|m| m.speaker_bot_id.clone()).collect()
}

#[test]
fn group_chats_and_threads() {
    run("groups-threads", async |root| {
        let provider = Provider::default();
        let base_url = start_provider(provider.clone()).await;
        let app = TestApp::start(&root.join("engine"), root.join("data")).await;
        let h = Harness { app, provider };
        let (status, body) = h.app.api("PUT", "/api/settings", Some(json!({ "baseUrl": base_url, "model": "local-test-model", "workspace": "" }))).await;
        assert_eq!(status, 200, "{body}");
        let started = Instant::now();
        group_order_attribution_and_sessions(&h).await;
        eprintln!("[groups] order and sessions ok in {:?}", started.elapsed());
        threads_quote_their_parent(&h).await;
        eprintln!("[groups] threads ok in {:?}", started.elapsed());
        h.app.close().await;
    });
}

async fn group_order_attribution_and_sessions(h: &Harness) {
    let alpha = h.bot("Alpha").await;
    let beta = h.bot("Beta").await;
    let gamma = h.bot("Gamma").await;
    let (status, created) = h.post("/api/groups", json!({ "name": "Team", "color": "#00c77b", "shape": "cloud", "memberBotIds": [alpha, beta] })).await;
    assert_eq!(status, 201, "{created}");
    let group = created["group"]["id"].as_str().unwrap().to_owned();
    assert_eq!(created["state"]["groups"][0]["name"], "Team");
    let (status, invalid) = h.post("/api/groups", json!({ "name": "Solo", "memberBotIds": [alpha] })).await;
    assert_eq!((status, invalid["error"].as_str()), (400, Some("memberBotIds: Choose between 2 and 12 member bots.")));
    let (status, patched) = h.app.api("PATCH", &format!("/api/groups/{group}"), Some(json!({ "memberBotIds": [alpha, beta, gamma] }))).await;
    assert_eq!(status, 200, "{patched}");
    let (status, created) = h.post(&format!("/api/groups/{group}/conversations"), json!({})).await;
    assert_eq!(status, 201, "{created}");
    let conversation = created["conversation"]["id"].as_str().unwrap().to_owned();
    assert_eq!(created["conversation"]["botId"], group.as_str());

    // Every member answers in member order, one turn at a time, attributed to its speaker.
    let (status, body) = h.send(&conversation, "First question").await;
    assert_eq!(status, 202, "{body}");
    let first = h.settled(&conversation, 4).await;
    assert_eq!(first.run.status, RunStatus::Completed, "{:?}", first.run);
    assert_eq!(speakers(&first), [None, Some(alpha.clone()), Some(beta.clone()), Some(gamma.clone())]);
    assert_eq!(first.messages.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), ["First question", "Reply from Alpha", "Reply from Beta", "Reply from Gamma"]);
    assert_eq!(first.title, "First question");
    let requests = h.provider.requests();
    assert_eq!(requests.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(), ["Alpha", "Beta", "Gamma"]);
    // Later members see the earlier replies to the same message as quoted data.
    assert!(requests[0].1.contains("Latest user message:\nFirst question"), "{}", requests[0].1);
    assert!(!requests[0].1.contains("Other members already replied"), "{}", requests[0].1);
    assert!(requests[2].1.contains(r#"[{"from":"Alpha","text":"Reply from Alpha"},{"from":"Beta","text":"Reply from Beta"}]"#), "{}", requests[2].1);
    // One engine session per member, each in its own directory.
    let sessions = first.member_sessions.clone().unwrap();
    assert_eq!(sessions.len(), 3);
    assert_eq!(sessions.iter().map(|s| s.session_id.clone()).collect::<std::collections::HashSet<_>>().len(), 3);
    for member in [&alpha, &beta, &gamma] {
        assert!(h.app.data_dir.join("grok").join(&conversation).join(member).is_dir());
    }
    assert_eq!(first.session_id, None);

    // The second round loads each member's session and quotes what happened since its last reply.
    h.provider.clear();
    assert_eq!(h.send(&conversation, "Second question").await.0, 202);
    let second = h.settled(&conversation, 8).await;
    assert_eq!(speakers(&second)[4..], [None, Some(alpha.clone()), Some(beta.clone()), Some(gamma.clone())]);
    let requests = h.provider.requests();
    let alpha_prompt = &requests[0].1;
    assert!(alpha_prompt.contains("since your last reply"), "{alpha_prompt}");
    assert!(alpha_prompt.contains(r#"[{"from":"Beta","text":"Reply from Beta"},{"from":"Gamma","text":"Reply from Gamma"}]"#), "{alpha_prompt}");
    assert!(!alpha_prompt.contains("First question"), "{alpha_prompt}");
    let gamma_prompt = &requests[2].1;
    assert!(!gamma_prompt.contains("Reply from Gamma"), "{gamma_prompt}");
    assert!(gamma_prompt.contains(r#""from":"Beta","text":"Reply from Beta""#), "{gamma_prompt}");
    assert_eq!(second.member_sessions.unwrap(), sessions, "loaded sessions keep their IDs");

    // Mentions select who answers; member order still applies.
    h.provider.clear();
    assert_eq!(h.send(&conversation, "@gamma and @Alpha, only you two").await.0, 202);
    let mentioned = h.settled(&conversation, 11).await;
    assert_eq!(speakers(&mentioned)[8..], [None, Some(alpha.clone()), Some(gamma.clone())]);

    // Stopping the current speaker also drops the members still queued.
    h.provider.clear();
    h.provider.state.lock().unwrap().hold = Some("Alpha".into());
    assert_eq!(h.send(&conversation, "Will be stopped").await.0, 202);
    h.wait("Alpha to be held", |_| h.provider.state.lock().unwrap().held).await;
    let busy = h.state().conversations.into_iter().find(|c| c.id == conversation).unwrap();
    assert_eq!(busy.group_round.as_ref().unwrap().pending_bot_ids, [beta.clone(), gamma.clone()]);
    let (status, body) = h.send(&conversation, "Interleaved").await;
    assert_eq!((status, body["error"].as_str()), (409, Some("Another reply is still running. Wait for it to finish or stop it, then send again.")));
    let (status, _) = h.post(&format!("/api/conversations/{conversation}/cancel"), json!({})).await;
    assert_eq!(status, 200);
    h.provider.release.notify_waiters();
    let cancelled = h.settled(&conversation, 13).await;
    assert_eq!(cancelled.run.status, RunStatus::Cancelled);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let after = h.conversation(&conversation);
    assert_eq!(after.messages.len(), 13, "no queued member answered after cancellation");
    assert_eq!(h.provider.requests().iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(), ["Alpha"]);
    h.provider.state.lock().unwrap().hold = None;

    // Members cannot be changed under a running group; deleting keeps the bots.
    assert_eq!(h.app.api("DELETE", &format!("/api/groups/{group}"), Some(json!({}))).await.0, 200);
    let state = h.state();
    assert!(state.groups.is_empty());
    assert!(!state.conversations.iter().any(|c| c.id == conversation));
    assert!(state.bots.iter().any(|bot| bot.id == alpha));
}

async fn threads_quote_their_parent(h: &Harness) {
    let grok = h.state().bots[0].id.clone();
    let (_, created) = h.post("/api/conversations", json!({ "botId": grok })).await;
    let parent = created["conversation"]["id"].as_str().unwrap().to_owned();
    assert_eq!(h.send(&parent, "Parent question").await.0, 202);
    let answered = h.settled(&parent, 2).await;
    let reply = answered.messages[1].id.clone();
    h.provider.clear();
    let (status, created) = h.post(&format!("/api/conversations/{parent}/threads"), json!({ "messageId": reply })).await;
    assert_eq!(status, 201, "{created}");
    let thread = created["conversation"]["id"].as_str().unwrap().to_owned();
    let (status, again) = h.post(&format!("/api/conversations/{parent}/threads"), json!({ "messageId": reply })).await;
    assert_eq!((status, again["conversation"]["id"].as_str()), (200, Some(thread.as_str())));
    assert_eq!(h.send(&thread, "Thread follow-up").await.0, 202);
    let first = h.settled(&thread, 2).await;
    assert_eq!(first.messages[1].text, "Reply from direct");
    let prompt = h.provider.requests()[0].1.clone();
    assert!(prompt.contains(r#"{"precedingMessages":[{"from":"user","text":"Parent question"}],"threadParent":{"from":"Grok","text":"Reply from direct"}}"#), "{prompt}");
    assert!(prompt.ends_with("Thread follow-up"), "{prompt}");
    // The loaded thread session gets only the new message; the parent conversation is untouched.
    h.provider.clear();
    assert_eq!(h.send(&thread, "Second thread message").await.0, 202);
    h.settled(&thread, 4).await;
    assert_eq!(h.provider.requests()[0].1, "Second thread message");
    assert_eq!(h.conversation(&parent).messages.len(), 2);

    // Threads of group conversations are answered by the group with the parent quoted.
    let alpha = h.bot("Delta").await;
    let beta = h.bot("Echo").await;
    let (_, created) = h.post("/api/groups", json!({ "name": "Pair", "memberBotIds": [alpha, beta] })).await;
    let group = created["group"]["id"].as_str().unwrap().to_owned();
    let (_, created) = h.post(&format!("/api/groups/{group}/conversations"), json!({})).await;
    let conversation = created["conversation"]["id"].as_str().unwrap().to_owned();
    assert_eq!(h.send(&conversation, "Group parent").await.0, 202);
    let answered = h.settled(&conversation, 3).await;
    let echo_reply = answered.messages[2].id.clone();
    let (status, created) = h.post(&format!("/api/conversations/{conversation}/threads"), json!({ "messageId": echo_reply })).await;
    assert_eq!(status, 201);
    let group_thread = created["conversation"]["id"].as_str().unwrap().to_owned();
    assert_eq!(created["conversation"]["groupId"], group.as_str());
    let (status, nested) = h.post(&format!("/api/conversations/{group_thread}/threads"), json!({ "messageId": echo_reply })).await;
    assert_eq!(status, 400, "{nested}");
    h.provider.clear();
    assert_eq!(h.send(&group_thread, "Thread question").await.0, 202);
    let replies = h.settled(&group_thread, 3).await;
    assert_eq!(speakers(&replies), [None, Some(alpha.clone()), Some(beta.clone())]);
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 2);
    for (_, prompt) in &requests {
        assert!(prompt.contains(r#""threadParent":{"from":"Echo","text":"Reply from Echo"}"#), "{prompt}");
        assert!(prompt.contains(r#"{"from":"user","text":"Group parent"}"#), "{prompt}");
    }
    assert_eq!(h.conversation(&conversation).messages.len(), 3);
}
