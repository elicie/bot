//! The cycle scenario needs real broker tool calls, not synthetic delivery/WaitingChildren rows.
//! Like scheduler/provider.rs, this serves the engine's real OpenAI-compatible HTTP protocol and
//! uses a one-shot gate plus notifications rather than time-based provider ordering.

use std::collections::VecDeque;
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
    Send { to: String, text: String },
    Hold(String),
}

impl Reply {
    pub fn text(text: &str) -> Self {
        Self::Text(text.into())
    }

    pub fn send(to: &str, text: &str) -> Self {
        Self::Send { to: to.into(), text: text.into() }
    }
}

#[derive(Clone)]
pub struct Request {
    pub persisted: DiskState,
}

#[derive(Default)]
struct Data {
    replies: VecDeque<Reply>,
    requests: Vec<Request>,
    release: Option<oneshot::Sender<()>>,
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
        assert!(data.replies.is_empty() && data.requests.is_empty());
        data.replies = replies.into();
    }

    pub fn requests(&self) -> Vec<Request> {
        self.data.lock().unwrap().requests.clone()
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

    pub fn release(&self) {
        self.data.lock().unwrap().release.take().expect("the cycle leaf reached its gate").send(()).unwrap();
    }

    pub fn assert_drained(&self) {
        let data = self.data.lock().unwrap();
        assert!(data.replies.is_empty(), "all real delivery/report turns consumed their scripted answers");
        assert!(data.release.is_none());
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
        data.requests.push(Request { persisted });
        let reply = data.replies.pop_front().expect("unexpected unplanned main-agent request");
        let gate = if matches!(&reply, Reply::Hold(_)) {
            let (release, released) = oneshot::channel();
            assert!(data.release.replace(release).is_none(), "only one explicit leaf gate");
            Some(released)
        } else {
            None
        };
        (reply, data.requests.len(), gate)
    };
    provider.changed.notify_one();
    match reply {
        Reply::Text(text) => sse(model, json!({"content": text}), "stop"),
        Reply::Send { to, text } => sse(
            model,
            json!({"tool_calls": [{
                "index": 0, "id": format!("call_cycle_{number}"), "type": "function",
                "function": {"name": "use_tool", "arguments": json!({
                    "tool_name": "workspace__send_message", "tool_input": {"toBotId": to, "text": text},
                }).to_string()},
            }]}),
            "tool_calls",
        ),
        Reply::Hold(text) => {
            gate.unwrap().await.expect("cycle leaf gate release");
            sse(model, json!({"content": text}), "stop")
        }
    }
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
