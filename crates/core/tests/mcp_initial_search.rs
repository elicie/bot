//! First-search readiness against the real engine, not an application-side MCP probe.
//! A slow synthetic Notion server must finish discovery before any model inference begins.

mod common;

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use common::TestApp;
use futures::FutureExt;
use serde_json::{Value, json};
use tokio::time::timeout;

#[derive(Default)]
struct Fixture {
    delay_ms: AtomicU64,
    fail_listing: std::sync::atomic::AtomicBool,
    listings: AtomicUsize,
    calls: AtomicUsize,
    inferences: AtomicUsize,
    step: AtomicUsize,
    searches: Mutex<Vec<Value>>,
    listing_started: tokio::sync::Notify,
    live: Option<LiveProvider>,
}

struct LiveProvider {
    base_url: String,
    api_key: String,
}

async fn mcp(State(fixture): State<Arc<Fixture>>, Json(request): Json<Value>) -> Response {
    let Some(id) = request.get("id") else { return StatusCode::ACCEPTED.into_response() };
    let result = match request["method"].as_str().unwrap_or_default() {
        "initialize" => {
            json!({"protocolVersion":request["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"synthetic-notion","version":"1"}})
        }
        "ping" => json!({}),
        "tools/list" => {
            fixture.listings.fetch_add(1, Ordering::SeqCst);
            fixture.listing_started.notify_one();
            if fixture.fail_listing.load(Ordering::SeqCst) {
                return Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":"Synthetic discovery failure"}})).into_response();
            }
            tokio::time::sleep(Duration::from_millis(fixture.delay_ms.load(Ordering::SeqCst))).await;
            json!({"tools":[
                {"name":"notion-search","description":"Search synthetic Notion pages. Read only.","inputSchema":{"type":"object","properties":{}}},
                {"name":"notion-fetch","description":"Fetch a synthetic Notion page. Read only.","inputSchema":{"type":"object","properties":{}}}
            ]})
        }
        "tools/call" => {
            fixture.calls.fetch_add(1, Ordering::SeqCst);
            json!({"content":[{"type":"text","text":"synthetic-notion-ok"}]})
        }
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
}

async fn completions(State(fixture): State<Arc<Fixture>>, Json(request): Json<Value>) -> Response {
    let main = request["tools"].as_array().into_iter().flatten().any(|tool| tool["function"]["name"] == "search_tool");
    let mut call = None;
    if main {
        fixture.inferences.fetch_add(1, Ordering::SeqCst);
        if let Some(live) = &fixture.live {
            // Opt-in investigation only: forward synthetic fixture traffic, never real account data.
            let messages = request["messages"].as_array().unwrap();
            let latest = messages.iter().rposition(|m| m["role"] == "user" && m["content"].to_string().contains("Synthetic fixture only")).unwrap();
            if let Some(result) = messages[latest + 1..].iter().filter(|m| m["role"] == "tool").find_map(|m| {
                let result = serde_json::from_str::<Value>(m["content"].as_str()?).ok()?;
                result.get("total_hidden_tools").map(|_| result.clone())
            }) {
                let mut searches = fixture.searches.lock().unwrap();
                if searches.is_empty() {
                    searches.push(result);
                }
            }
            if fixture.step.fetch_add(1, Ordering::SeqCst) < 4 {
                let response = reqwest_mcp::Client::new()
                    .post(format!("{}/chat/completions", live.base_url.trim_end_matches('/')))
                    .bearer_auth(&live.api_key)
                    .json(&request)
                    .timeout(Duration::from_secs(60))
                    .send()
                    .await
                    .expect("live fixture provider responds");
                assert!(response.status().is_success(), "live provider status {}", response.status());
                return ([(header::CONTENT_TYPE, "text/event-stream")], axum::body::Body::from_stream(response.bytes_stream())).into_response();
            }
        } else {
            match fixture.step.fetch_add(1, Ordering::SeqCst) {
                0 => call = Some(("search_tool", json!({"query":"notion search","limit":5}))),
                1 => {
                    let result = request["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .rev()
                        .find(|message| message["role"] == "tool")
                        .and_then(|message| message["content"].as_str())
                        .and_then(|content| serde_json::from_str::<Value>(content).ok())
                        .expect("engine returns the search result to the model");
                    let found = result["results"].as_array().is_some_and(|groups| !groups.is_empty());
                    fixture.searches.lock().unwrap().push(result);
                    if found {
                        call = Some(("use_tool", json!({"tool_name":"notion__notion-search","tool_input":{}})));
                    }
                }
                _ => {}
            }
        }
    }
    let delta = match &call {
        Some((name, arguments)) => {
            json!({"tool_calls":[{"index":0,"id":"initial-search-call","type":"function","function":{"name":name,"arguments":arguments.to_string()}}]})
        }
        None => json!({"content":"Discovery finished."}),
    };
    let chunk = |delta: Value, finish: Value| json!({"id":"initial-search","object":"chat.completion.chunk","created":1,"model":request["model"],"choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
    let finish = if call.is_some() { "tool_calls" } else { "stop" };
    let body = format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n", chunk(delta, Value::Null), chunk(json!({}), json!(finish)));
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

async fn wait_for(app: &TestApp, id: &str, predicate: impl Fn(&Value) -> bool) -> Value {
    timeout(Duration::from_secs(20), async {
        loop {
            let current = app.store.read(|state| serde_json::to_value(state.conversation(id).unwrap()).unwrap());
            if predicate(&current) {
                return current;
            }
            assert_ne!(current["run"]["status"], "error", "{}", current["run"]);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("run reaches expected state")
}

async fn send(app: &TestApp, id: &str) {
    let result = app.api("POST", &format!("/api/conversations/{id}/messages"), Some(json!({"text":"Synthetic fixture only, no real account: first call search_tool with query \"notion search\" and limit 5. Then use the returned notion__notion-search read-only tool exactly once with {}. Do not use any workspace tools. Finish briefly after the call."}))).await;
    assert_eq!(result.0, 202, "{result:?}");
}

fn assert_ready_search(fixture: &Fixture, total: usize) {
    let searches = fixture.searches.lock().unwrap();
    assert_eq!(searches.len(), 1, "{searches:?}");
    let result = &searches[0];
    eprintln!("[mcp-initial-search] first search: {result}");
    assert_eq!(result["status"], "ready", "{result}");
    assert_eq!(result["total_hidden_tools"], total, "{result}");
    assert!(
        result["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|group| { group["server"] == "notion" && group["tools"].as_array().unwrap().iter().any(|tool| tool["tool_name"] == "notion__notion-search") }),
        "{result}"
    );
}

#[test]
fn first_search_waits_for_custom_mcp_on_new_and_loaded_sessions() {
    common::run("mcp-initial-search", async |root| {
        let model = std::env::var("MCP_SEARCH_LIVE_MODEL").unwrap_or_else(|_| "local-test-model".into());
        let live = std::env::var("MCP_SEARCH_LIVE_MODEL").ok().map(|_| LiveProvider {
            base_url: std::env::var("MCP_SEARCH_LIVE_BASE_URL").expect("explicit live provider base URL"),
            api_key: std::env::var("MCP_SEARCH_LIVE_API_KEY").expect("explicit live provider key"),
        });
        let fixture = Arc::new(Fixture { live, ..Fixture::default() });
        let router = Router::new()
            .route("/v1/chat/completions", post(completions))
            .route("/mcp", post(mcp).get(|| async { StatusCode::METHOD_NOT_ALLOWED }))
            .with_state(fixture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let app = TestApp::start(&root.join("engine"), root.join("data")).await;
        assert_eq!(app.api("PUT", "/api/settings", Some(json!({"baseUrl":format!("{origin}/v1"),"model":model,"workspace":""}))).await.0, 200);
        let bot = app.store.snapshot().bots[0].id.clone();
        let document = json!({"notion":{"url":format!("{origin}/mcp")}}).to_string();
        assert_eq!(app.api("PUT", &format!("/api/bots/{bot}/mcp-servers"), Some(json!({"json":document}))).await.0, 200);
        assert_eq!(app.api("PATCH", &format!("/api/bots/{bot}/mcp-servers/notion"), Some(json!({"enabled":true}))).await.0, 200);
        let (_, created) = app.api("POST", "/api/conversations", Some(json!({"botId":bot}))).await;
        let id = created["conversation"]["id"].as_str().unwrap();
        // 22 app-owned workspace tools, plus the two synthetic Notion tools.
        let builtin_count = 22;

        // Beyond upstream's two-second grace: the immediate model must still see a complete index.
        let delay_ms = std::env::var("MCP_SEARCH_DELAY_MS").ok().map(|value| value.parse::<u64>().expect("fixture delay in milliseconds")).unwrap_or(3500);
        fixture.delay_ms.store(delay_ms, Ordering::SeqCst);
        for turn in 0..2 {
            fixture.step.store(0, Ordering::SeqCst);
            fixture.searches.lock().unwrap().clear();
            send(&app, id).await;
            let current = wait_for(&app, id, |c| c["run"]["status"] == "awaiting_permission" || c["run"]["status"] == "completed").await;
            assert_ready_search(&fixture, builtin_count + 2);
            assert_eq!(current["run"]["status"], "awaiting_permission");
            assert_eq!(fixture.calls.load(Ordering::SeqCst), turn, "discovery must not bypass custom tool approval");
            let permission = &current["run"]["permission"];
            let allow = permission["options"].as_array().unwrap().iter().find(|option| option["kind"] == "allow_once").unwrap()["id"].clone();
            assert_eq!(
                app.api("POST", &format!("/api/conversations/{id}/permissions/{}", permission["id"].as_str().unwrap()), Some(json!({"optionId":allow})))
                    .await
                    .0,
                200
            );
            wait_for(&app, id, |c| c["run"]["status"] == "completed").await;
            assert_eq!(fixture.calls.load(Ordering::SeqCst), turn + 1);
        }
        assert_eq!(fixture.listings.load(Ordering::SeqCst), 2, "the second turn reloads and rediscovers the MCP server");

        // A custom server that fails still settles initialization; chat degrades without it.
        fixture.fail_listing.store(true, Ordering::SeqCst);
        fixture.step.store(0, Ordering::SeqCst);
        fixture.searches.lock().unwrap().clear();
        send(&app, id).await;
        wait_for(&app, id, |c| c["run"]["status"] == "completed").await;
        {
            let searches = fixture.searches.lock().unwrap();
            assert_eq!(searches.len(), 1);
            assert_eq!(searches[0]["status"], "ready");
            assert_eq!(searches[0]["total_hidden_tools"], builtin_count);
            assert!(searches[0]["results"].as_array().unwrap().is_empty());
        }
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);

        // Cancellation during discovery finishes promptly without ever contacting the model.
        fixture.fail_listing.store(false, Ordering::SeqCst);
        fixture.delay_ms.store(60_000, Ordering::SeqCst);
        while fixture.listing_started.notified().now_or_never().is_some() {}
        let before = fixture.inferences.load(Ordering::SeqCst);
        send(&app, id).await;
        timeout(Duration::from_secs(5), fixture.listing_started.notified()).await.expect("slow discovery starts");
        let cancelled = timeout(Duration::from_secs(2), app.api("POST", &format!("/api/conversations/{id}/cancel"), Some(json!({}))))
            .await
            .expect("cancel does not wait for MCP startup");
        assert_eq!(cancelled.0, 200);
        wait_for(&app, id, |c| c["run"]["status"] == "cancelled").await;
        assert_eq!(fixture.inferences.load(Ordering::SeqCst), before);
        app.close().await;

        // The same gate belongs to startup, not inference inactivity. No deferred prompt runs
        // after its deadline, and the session is closed through normal run cleanup.
        while fixture.listing_started.notified().now_or_never().is_some() {}
        let mut options = grok_bot_core::runtime::RuntimeOptions::new(root.join("engine"));
        options.startup_timeout = Duration::from_secs(1);
        let limited = TestApp::try_start_with_runtime(
            common::AppOptions { engine_root: &root.join("engine"), data_dir: root.join("data"), allowed_hosts: vec![] },
            Some(options),
        )
        .await
        .unwrap();
        send(&limited, id).await;
        timeout(Duration::from_secs(3), fixture.listing_started.notified()).await.expect("discovery starts within startup budget");
        let expired =
            timeout(Duration::from_secs(3), wait_for(&limited, id, |c| c["run"]["status"] == "error")).await.expect("startup deadline remains bounded");
        assert!(expired["run"]["error"].as_str().unwrap().contains("startup deadline"));
        assert_eq!(fixture.inferences.load(Ordering::SeqCst), before);
        limited.close().await;
        server.abort();
        let _ = server.await;
    });
}
