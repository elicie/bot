//! Gated OpenAI-compatible provider, using the same wire protocol as the runtime integration tests.

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

#[derive(Clone)]
pub struct Request {
    pub prompt: String,
    /// The actual durable state when the engine reached the provider, not a store-only snapshot.
    pub persisted: DiskState,
}

#[derive(Default)]
struct StateData {
    requests: Vec<Request>,
    releases: Vec<Option<oneshot::Sender<()>>>,
}

#[derive(Clone)]
pub struct Provider {
    state: Arc<Mutex<StateData>>,
    changed: Arc<Notify>,
    state_file: PathBuf,
    immediate: bool,
}

impl Provider {
    pub fn new(state_file: PathBuf, immediate: bool) -> Self {
        Self { state: Arc::default(), changed: Arc::default(), state_file, immediate }
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

    pub fn requests(&self) -> Vec<Request> {
        self.state.lock().unwrap().requests.clone()
    }

    pub fn prompts(&self) -> Vec<String> {
        self.requests().into_iter().map(|request| request.prompt).collect()
    }

    pub async fn wait_for(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if self.state.lock().unwrap().requests.len() >= count {
                    return;
                }
                self.changed.notified().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("waiting for {count} provider requests; saw {:?}", self.prompts()));
        assert_eq!(self.requests().len(), count, "unexpected duplicate provider request");
    }

    pub fn release(&self, index: usize) {
        let release = self.state.lock().unwrap().releases[index].take().expect("request has not been released");
        // A cancellation may have already dropped the HTTP request.
        let _ = release.send(());
    }

    pub fn release_all(&self) {
        for release in &mut self.state.lock().unwrap().releases {
            if let Some(release) = release.take() {
                let _ = release.send(());
            }
        }
    }
}

fn content_text(value: &Value) -> String {
    match value {
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
        .expect("engine wraps the turn in user_query");
    let start = query.find("<user_query>\n").unwrap() + "<user_query>\n".len();
    let end = query.rfind("\n</user_query>").unwrap();
    query[start..end].to_owned()
}

async fn completions(State(provider): State<Provider>, Json(request): Json<Value>) -> Response {
    // Title/context helper calls are not agent turns and must not consume a turn's gate.
    let main = request["tools"].as_array().is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == "search_tool"));
    let answer = if main {
        let prompt = turn_prompt(&request);
        let persisted = serde_json::from_slice(&std::fs::read(&provider.state_file).unwrap()).unwrap();
        let (release, released) = oneshot::channel();
        {
            let mut state = provider.state.lock().unwrap();
            state.requests.push(Request { prompt: prompt.clone(), persisted });
            state.releases.push(Some(release));
        }
        provider.changed.notify_one();
        if !provider.immediate {
            let _ = released.await;
        }
        format!("Completed: {prompt}")
    } else {
        "Scheduler fixture title".to_owned()
    };
    let model = request["model"].as_str().unwrap();
    let chunk = |delta: Value, finish: Value| {
        json!({
            "id": "scheduler-fixture", "object": "chat.completion.chunk", "created": 1, "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
        })
    };
    let mut end = chunk(json!({}), json!("stop"));
    end["usage"] = json!({"prompt_tokens": 100, "completion_tokens": 12, "total_tokens": 112});
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: {end}\n\ndata: [DONE]\n\n",
        chunk(json!({"role": "assistant"}), Value::Null),
        chunk(json!({"content": answer}), Value::Null),
    );
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}
