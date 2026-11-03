//! The cycle scenario needs real broker tool calls, not synthetic delivery/WaitingChildren rows.
//! Like scheduler/provider.rs, this serves the engine's real OpenAI-compatible HTTP protocol and
//! uses a one-shot gate plus notifications rather than time-based provider ordering.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use grok_bot_model::DiskState;
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

pub enum Reply {
    Text(String),
    Send {
        to: String,
        text: String,
    },
    Tool {
        name: &'static str,
        input: Value,
    },
    Hold(String),
    /// An independently released gate for deterministic overlapping Bot/worker scenarios.
    NamedHold {
        name: String,
        text: String,
    },
}

impl Reply {
    pub fn text(text: &str) -> Self {
        Self::Text(text.into())
    }

    pub fn send(to: &str, text: &str) -> Self {
        Self::Send { to: to.into(), text: text.into() }
    }

    pub fn tool(name: &'static str, input: Value) -> Self {
        Self::Tool { name, input }
    }

    pub fn hold(name: &str, text: &str) -> Self {
        Self::NamedHold { name: name.into(), text: text.into() }
    }
}

#[derive(Clone)]
pub struct Request {
    pub persisted: DiskState,
    pub payload: Value,
}

#[derive(Default)]
struct Data {
    replies: VecDeque<Reply>,
    /// Plans keyed by a distinct current-turn prompt marker, independent of bot request ordering.
    keyed: BTreeMap<String, VecDeque<Reply>>,
    /// Explicit substring routes, disjoint from exact keyed plans. Ambiguous matches fail rather
    /// than routing a worker as a report (or choosing by incidental map ordering).
    contains: BTreeMap<String, VecDeque<Reply>>,
    prefixes: BTreeMap<String, VecDeque<Reply>>,
    requests: Vec<Request>,
    release: Option<oneshot::Sender<()>>,
    named_releases: BTreeMap<String, oneshot::Sender<()>>,
}

#[derive(Clone)]
pub struct ScriptedProvider {
    data: Arc<Mutex<Data>>,
    changed: Arc<Notify>,
    state_file: PathBuf,
}

impl ScriptedProvider {
    pub fn new(state_file: PathBuf) -> Self {
        Self { data: Arc::default(), changed: Arc::default(), state_file }
    }

    pub async fn serve(&self) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let router = Router::new()
            .route("/v1/chat/completions", axum::routing::post(completions))
            .route("/v1/models", axum::routing::get(|| async { Json(json!({"data": [{"id": "scheduler-test-model"}]})) }))
            .with_state(self.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (base, server)
    }

    pub fn plan(&self, replies: Vec<Reply>) {
        let mut data = self.data.lock().unwrap();
        assert!(data.replies.is_empty() && data.keyed.is_empty() && data.requests.is_empty());
        data.replies = replies.into();
    }

    pub fn plan_for(&self, marker: &str, replies: Vec<Reply>) {
        let mut data = self.data.lock().unwrap();
        assert!(data.replies.is_empty() && data.requests.is_empty());
        assert!(data.keyed.insert(marker.into(), replies.into()).is_none(), "one plan per distinct turn marker");
    }

    pub fn plan_contains(&self, marker: &str, replies: Vec<Reply>) {
        let mut data = self.data.lock().unwrap();
        assert!(!marker.is_empty() && data.replies.is_empty() && data.keyed.is_empty() && data.requests.is_empty());
        assert!(data.contains.insert(marker.into(), replies.into()).is_none(), "one plan per explicit substring marker");
    }

    pub fn plan_prefix(&self, prefix: &str, replies: Vec<Reply>) {
        let mut data = self.data.lock().unwrap();
        assert!(!prefix.is_empty() && data.replies.is_empty() && data.keyed.is_empty() && data.requests.is_empty());
        assert!(data.prefixes.insert(prefix.into(), replies.into()).is_none(), "one plan per explicit role prefix");
    }

    pub fn release_for(&self, name: &str) {
        self.data.lock().unwrap().named_releases.remove(name).expect("the named request reached its gate").send(()).unwrap();
    }

    /// Dispose only of an intentionally cancelled HTTP request. Unlike normal release, this
    /// requires proof that its receiver closed without receiving a fabricated provider answer.
    pub async fn discard_cancelled_gate(&self, name: &str) {
        let mut release = self.data.lock().unwrap().named_releases.remove(name).expect("the cancelled request previously reached its gate");
        tokio::time::timeout(Duration::from_secs(45), release.closed())
            .await
            .unwrap_or_else(|_| panic!("cancelled provider request {name} must disconnect before its gate is discarded"));
    }

    pub fn requests(&self) -> Vec<Request> {
        self.data.lock().unwrap().requests.clone()
    }

    pub fn tool_outcomes(&self) -> Vec<String> {
        self.requests()
            .iter()
            .flat_map(|request| request.payload["messages"].as_array().unwrap())
            .filter(|message| message["role"] == "tool")
            .map(|message| content_text(&message["content"]))
            .collect()
    }

    pub async fn wait_for(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                if self.data.lock().unwrap().requests.len() >= count {
                    return;
                }
                self.changed.notified().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("waiting for {count} cycle requests; saw {}", self.requests().len()));
        assert_eq!(self.requests().len(), count, "unexpected inference before the leaf gate is released");
    }

    pub async fn wait_for_gate(&self, name: &str) {
        tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.data.lock().unwrap().named_releases.contains_key(name) {
                    return;
                }
                changed.await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("waiting for named gate {name}; saw {} requests", self.requests().len()));
    }

    pub fn release(&self) {
        self.data.lock().unwrap().release.take().expect("the cycle leaf reached its gate").send(()).unwrap();
    }

    pub fn assert_drained(&self) {
        let data = self.data.lock().unwrap();
        assert!(data.replies.is_empty() && data.keyed.values().all(VecDeque::is_empty), "all real tool/report turns consumed their scripted answers");
        assert!(data.release.is_none());
        assert!(
            data.contains.values().chain(data.prefixes.values()).all(VecDeque::is_empty),
            "all explicitly routed worker/report turns consumed their scripted answers"
        );
        assert!(data.named_releases.is_empty(), "every named gate was released");
    }
}

async fn completions(State(provider): State<ScriptedProvider>, Json(request): Json<Value>) -> Response {
    let model = request["model"].as_str().unwrap();
    let main = request["tools"].as_array().is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == "search_tool"));
    if !main {
        return sse(model, json!({"content": "Cycle fixture title"}), "stop");
    }
    let persisted = serde_json::from_slice(&std::fs::read(&provider.state_file).unwrap()).unwrap();
    let (reply, number, gate) = {
        let mut data = provider.data.lock().unwrap();
        data.requests.push(Request { persisted, payload: request.clone() });
        let reply = if !data.contains.is_empty() || !data.prefixes.is_empty() {
            assert!(data.replies.is_empty() && data.keyed.is_empty(), "explicit marker plans cannot mix with legacy global/exact plans");
            let prompt = turn_prompt(&request);
            // Continuation history can quote old markers; route only the actual current turn.
            let current = prompt.rsplit("\n\nCurrent user message:\n").next().unwrap();
            let prefixes: Vec<_> = data.prefixes.keys().filter(|prefix| current.starts_with(prefix.as_str())).cloned().collect();
            assert!(prefixes.len() <= 1, "ambiguous role prefixes for {current}: {prefixes:?}");
            if let Some(prefix) = prefixes.first() {
                // The explicit app-written report role outranks task markers quoted in its JSON.
                data.prefixes.get_mut(prefix).unwrap().pop_front().expect("exhausted role-prefix plan")
            } else {
                let markers: Vec<_> = data.contains.keys().filter(|marker| current.contains(marker.as_str())).cloned().collect();
                assert_eq!(markers.len(), 1, "ambiguous/unplanned current turn: {current}; matches {markers:?}");
                data.contains.get_mut(&markers[0]).unwrap().pop_front().expect("exhausted substring plan")
            }
        } else if data.keyed.is_empty() {
            data.replies.pop_front().expect("unexpected unplanned main-agent request")
        } else {
            data.keyed.get_mut(&turn_prompt(&request)).expect("unexpected current-turn prompt").pop_front().expect("exhausted current-turn plan")
        };
        // Register the one-shot receiver before advertising the request to observers. An immediate
        // release is buffered even if the response handler has not started awaiting the receiver.
        let gate = match &reply {
            Reply::Hold(_) => {
                let (release, released) = oneshot::channel();
                assert!(data.release.replace(release).is_none(), "only one explicit leaf gate");
                Some(released)
            }
            Reply::NamedHold { name, .. } => {
                let (release, released) = oneshot::channel();
                assert!(data.named_releases.insert(name.clone(), release).is_none(), "each live named gate has a distinct owner");
                Some(released)
            }
            _ => None,
        };
        (reply, data.requests.len(), gate)
    };
    provider.changed.notify_one();
    match reply {
        Reply::Text(text) => sse(model, json!({"content": text}), "stop"),
        Reply::Send { to, text } => tool_sse(model, number, "send_message", json!({"toBotId": to, "text": text})),
        Reply::Tool { name, input } => tool_sse(model, number, name, input),
        Reply::Hold(text) => {
            gate.unwrap().await.expect("cycle leaf gate release");
            sse(model, json!({"content": text}), "stop")
        }
        Reply::NamedHold { text, .. } => {
            gate.unwrap().await.expect("named request gate release");
            sse(model, json!({"content": text}), "stop")
        }
    }
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().filter_map(|part| part["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn turn_prompt(request: &Value) -> String {
    let query = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .filter(|message| message["role"] == "user")
        .map(|message| content_text(&message["content"]))
        .find(|text| text.contains("<user_query>"))
        .expect("engine wraps the current turn in user_query");
    let start = query.find("<user_query>\n").unwrap() + "<user_query>\n".len();
    let end = query.rfind("\n</user_query>").unwrap();
    query[start..end].to_owned()
}

fn tool_sse(model: &str, number: usize, name: &str, input: Value) -> Response {
    sse(
        model,
        json!({"tool_calls": [{
            "index": 0, "id": format!("call_cycle_{number}"), "type": "function",
            "function": {"name": "use_tool", "arguments": json!({
                "tool_name": format!("workspace__{name}"), "tool_input": input,
            }).to_string()},
        }]}),
        "tool_calls",
    )
}

fn sse(model: &str, delta: Value, finish: &str) -> Response {
    let chunk = |delta: Value, finish: Value| {
        json!({
            "id": "cycle-fixture", "object": "chat.completion.chunk", "created": 1, "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        })
    };
    let mut end = chunk(json!({}), json!(finish));
    end["usage"] = json!({"prompt_tokens": 100, "completion_tokens": 12, "total_tokens": 112});
    let body = format!("data: {}\n\ndata: {}\n\ndata: {end}\n\ndata: [DONE]\n\n", chunk(json!({"role": "assistant"}), Value::Null), chunk(delta, Value::Null),);
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}
