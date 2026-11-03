//! Real TCP probes stay outside the app command queue, have bounded admission, use the engine's
//! actual subprocess launch context, and are cleaned on browser disconnect and backend shutdown.
#![cfg(unix)]

use crate::common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use grok_bot_core::app::{Backend, BackendOptions};
use grok_bot_core::scheduler::SchedulerOptions;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout};

async fn api(port: u16, method: &str, route: &str, body: Value) -> (u16, Value) {
    let headers = vec![
        ("Host".into(), format!("127.0.0.1:{port}")),
        ("Origin".into(), format!("http://127.0.0.1:{port}")),
        ("X-Grok-Bot".into(), "1".into()),
        ("Content-Type".into(), "application/json".into()),
    ];
    let (status, text) = common::raw(port, method, route, &headers, Some(&body.to_string())).await;
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

async fn probe_request(port: u16, bot: &str, marker: &Path, active: bool) -> TcpStream {
    let script = if active {
        r#"const fs=require('fs'), cp=require('child_process');
const child=cp.spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{stdio:'ignore'});
fs.writeFileSync(process.argv[1],JSON.stringify({pid:process.pid,child:child.pid}));
setInterval(()=>{},1000);"#
    } else {
        "require('fs').writeFileSync(process.argv[1], 'must-not-spawn'); setInterval(()=>{},1000);"
    };
    let body = json!({"json":json!({"shutdown-test":{"command":"node", "args":["-e",script,marker]}}).to_string()}).to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let connection = if active { "keep-alive" } else { "close" };
    let head = format!(
        "POST /api/bots/{bot}/mcp-servers/test HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nX-Grok-Bot: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {connection}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();
    stream
}

async fn record(marker: &Path) -> Value {
    timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(bytes) = tokio::fs::read(marker).await
                && let Ok(record) = serde_json::from_slice::<Value>(&bytes)
            {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("stdio fixture started")
}

async fn stopped(record: &Value) {
    let pid = record["pid"].as_i64().unwrap() as i32;
    let child = record["child"].as_i64().unwrap();
    timeout(Duration::from_secs(3), async {
        loop {
            // SAFETY: signal 0 probes only the PID recorded by this test's own child.
            let reaped = unsafe { libc::kill(pid, 0) } == -1;
            let status = tokio::fs::read_to_string(format!("/proc/{child}/stat")).await.unwrap_or_default();
            if reaped && (status.is_empty() || status.split_whitespace().nth(2) == Some("Z")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("direct child reaped and process-group descendant stopped");
}

async fn response(mut stream: TcpStream, status: u16) -> String {
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with(&format!("HTTP/1.1 {status}")), "{response}");
    response
}

// A real provider flow requests a workspace tool so the HTTP message and approval routes are
// exercised, not merely an unrelated read or an invalid endpoint that never takes the queue.
async fn completions(State(stage): State<Arc<AtomicUsize>>, Json(request): Json<Value>) -> Response {
    let main = request["tools"].as_array().into_iter().flatten().any(|tool| tool["function"]["name"] == "search_tool");
    let step = if main { stage.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_add(1)).unwrap_or(usize::MAX) } else { usize::MAX };
    let call = match step {
        0 => Some(("search_tool", json!({"query":"workspace create_bot", "limit":5}))),
        1 => Some(("use_tool", json!({"tool_name":"workspace__create_bot", "tool_input":{"name":"Probe approval bot"}}))),
        _ => None,
    };
    let delta = match &call {
        Some((name, args)) => {
            json!({"role":"assistant", "tool_calls":[{"index":0,"id":format!("probe-call-{step}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]})
        }
        None => json!({"role":"assistant", "content":"The app remains responsive."}),
    };
    let chunk = |delta: Value, finish: Value| json!({"id":"probe-provider","object":"chat.completion.chunk","created":1,"model":"local-test-model","choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
    let mut end = chunk(json!({}), json!(if call.is_some() { "tool_calls" } else { "stop" }));
    end["usage"] = json!({"prompt_tokens":10,"completion_tokens":5,"total_tokens":15});
    let body = format!("data: {}\n\ndata: {end}\n\ndata: [DONE]\n\n", chunk(delta, Value::Null));
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

async fn conversation_state(store: &grok_bot_store::Store, id: &str, predicate: impl Fn(&Value) -> bool) -> Value {
    timeout(Duration::from_secs(8), async {
        loop {
            let current = store.read(|s| serde_json::to_value(s.conversation(id).unwrap()).unwrap());
            if predicate(&current) {
                return current;
            }
            assert_ne!(current["run"]["status"], "error", "{current}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("engine conversation reaches expected state")
}

const CONTEXT_SERVER: &str = r#"
const fs=require('fs');
const keys=['HOME','PATH','TMPDIR','XDG_CONFIG_HOME','XDG_CACHE_HOME','XDG_DATA_HOME','FIXTURE_OVERRIDE','GROK_SESSION_ID'];
const fingerprint=require('crypto').createHash('sha256').update(JSON.stringify(Object.entries(process.env).filter(([k])=>!['CONTEXT_OUT','GROK_SESSION_ID'].includes(k)).sort((a,b)=>a[0].localeCompare(b[0])))).digest('hex');
fs.writeFileSync(process.env.CONTEXT_OUT,JSON.stringify({cwd:process.cwd(),fingerprint,env:Object.fromEntries(keys.map(k=>[k,process.env[k]]))}));
require('readline').createInterface({input:process.stdin}).on('line',line=>{
 const req=JSON.parse(line); if(req.id===undefined)return;
 let result;
 if(req.method==='initialize')result={protocolVersion:'2025-11-25',capabilities:{tools:{}},serverInfo:{name:'context',version:'1'}};
 else if(req.method==='tools/list')result={tools:[]};
 else if(req.method==='ping')result={}; else process.exit(3);
 console.log(JSON.stringify({jsonrpc:'2.0',id:req.id,result}));
});
"#;

fn context_document(out: &Path) -> String {
    json!({"context":{"command":"node","args":["-e",CONTEXT_SERVER],"env":{
        "CONTEXT_OUT":out, "FIXTURE_OVERRIDE":"explicit-engine-env", "GROK_SESSION_ID":"must-be-overwritten"
    }}})
    .to_string()
}

#[test]
fn probes_are_responsive_bounded_and_cleaned_on_tcp_disconnect_and_shutdown() {
    common::run("mcp-probe-shutdown", async |root| {
        let stage = Arc::new(AtomicUsize::new(usize::MAX));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let router = Router::new().route("/v1/chat/completions", axum::routing::post(completions)).with_state(stage.clone());
        let provider = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let data_dir = root.join("data");
        let backend = Backend::new(BackendOptions {
            data_dir,
            engine_root: root.join("engine"),
            web_root: None,
            embedded_web: None,
            allowed_hosts: vec![],
            now: None,
            scheduler: Some(SchedulerOptions { auto_start: false, ..SchedulerOptions::default() }),
            runtime: None,
        })
        .await
        .unwrap();
        let store = backend.store.clone();
        let bot = store.snapshot().bots[0].id.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop, shutdown) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(backend.serve(listener, async {
            let _ = shutdown.await;
        }));
        let settings = json!({"baseUrl":base_url,"model":"local-test-model","workspace":""});
        assert_eq!(api(port, "PUT", "/api/settings", settings.clone()).await.0, 200);

        // Capture the pinned engine's ACTUAL stdio subprocess, not an assumed shell env/cwd.
        let engine_out = root.join("engine-context.json");
        let route = format!("/api/bots/{bot}/mcp-servers");
        assert_eq!(api(port, "PUT", &route, json!({"json":context_document(&engine_out)})).await.0, 200);
        assert_eq!(api(port, "PATCH", &format!("{route}/context"), json!({"enabled":true})).await.0, 200);
        let (_, created) = api(port, "POST", "/api/conversations", json!({"botId":bot})).await;
        let id = created["conversation"]["id"].as_str().unwrap();
        let messages = format!("/api/conversations/{id}/messages");
        assert_eq!(api(port, "POST", &messages, json!({"text":"Warm the actual engine."})).await.0, 202);
        conversation_state(&store, id, |c| c["run"]["status"] == "completed").await;
        let actual = record(&engine_out).await;
        let probe_out = root.join("probe-context.json");
        assert_eq!(api(port, "POST", &format!("{route}/test"), json!({"json":context_document(&probe_out)})).await, (200, json!({"toolCount":0})));
        let probe = record(&probe_out).await;
        assert_eq!(probe["cwd"], actual["cwd"], "probe cwd must match actual engine subprocess, not the ACP private workspace");
        assert_eq!(probe["fingerprint"], actual["fingerprint"], "all inherited+configured env must match apart from output path and unique session ID");
        for key in ["HOME", "PATH", "TMPDIR", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "FIXTURE_OVERRIDE"] {
            assert_eq!(probe["env"][key], actual["env"][key], "{key}");
        }
        for capture in [&actual, &probe] {
            assert_ne!(capture["env"]["GROK_SESSION_ID"], "must-be-overwritten");
            assert!(!capture["env"]["GROK_SESSION_ID"].as_str().unwrap().is_empty());
        }

        let active_marker = root.join("active.json");
        let active = probe_request(port, &bot, &active_marker, true).await;
        let process = record(&active_marker).await;
        let rejected_marker = root.join("must-not-be-queued");
        let rejected = probe_request(port, &bot, &rejected_marker, false).await;
        timeout(Duration::from_secs(1), response(rejected, 429)).await.expect("admission rejects immediately, never silently queues");
        assert!(!rejected_marker.exists());

        // Representative writes AND real message/approval processing finish while startup hangs.
        timeout(Duration::from_secs(2), async {
            assert_eq!(api(port, "PUT", "/api/settings", settings).await.0, 200);
            assert_eq!(
                api(
                    port,
                    "POST",
                    "/api/routines",
                    json!({"botId":bot,"name":"During probe","prompt":"Later","schedule":{"kind":"interval","minutes":60},"enabled":false})
                )
                .await
                .0,
                201
            );
            assert_eq!(api(port, "POST", "/api/permission-rules", json!({"category":"file_write","action":"ask","botId":bot})).await.0, 201);
            stage.store(0, Ordering::SeqCst);
            assert_eq!(api(port, "POST", &messages, json!({"text":"Create a bot while MCP test hangs."})).await.0, 202);
        })
        .await
        .expect("app writes and message start must not wait for a probe");
        let awaiting = conversation_state(&store, id, |c| c["run"]["status"] == "awaiting_permission").await;
        let permission = &awaiting["run"]["permission"];
        let option = permission["options"].as_array().unwrap().iter().find(|o| o["kind"] == "allow_once").unwrap()["id"].as_str().unwrap();
        timeout(Duration::from_secs(1), async {
            assert_eq!(
                api(port, "POST", &format!("/api/conversations/{id}/permissions/{}", permission["id"].as_str().unwrap()), json!({"optionId":option})).await.0,
                200
            );
        })
        .await
        .expect("approval is not held behind probe startup");
        conversation_state(&store, id, |c| c["run"]["status"] == "completed").await;
        assert!(store.snapshot().bots.iter().any(|b| b.name == "Probe approval bot"));
        // Browser TCP socket closes, not an artificial abort of the Rust handler future.
        drop(active);
        stopped(&process).await;

        // Admission remains usable after disconnect cleanup; a subsequent active probe is tracked
        // independently of the app queue and is reaped before graceful shutdown completes.
        let shutdown_marker = root.join("shutdown.json");
        let active = probe_request(port, &bot, &shutdown_marker, true).await;
        let process = record(&shutdown_marker).await;
        let before = serde_json::to_value(store.snapshot()).unwrap();
        let started = Instant::now();
        stop.send(()).unwrap();
        timeout(Duration::from_secs(3), async {
            let (reply, server) = tokio::join!(response(active, 503), task);
            assert!(reply.contains("MCP 테스트 중단"));
            server.unwrap().unwrap();
        })
        .await
        .expect("shutdown cancels tracked probe without the app queue");
        assert!(started.elapsed() < Duration::from_secs(3));
        let pid = process["pid"].as_i64().unwrap() as i32;
        // SAFETY: signal 0 probes this fixture's child, which must already have been waited.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "shutdown waits for worker child reaping");
        stopped(&process).await;
        assert_eq!(serde_json::to_value(store.snapshot()).unwrap(), before, "probe shutdown does not mutate store");
        provider.abort();
        let _ = provider.await;
    });
}
