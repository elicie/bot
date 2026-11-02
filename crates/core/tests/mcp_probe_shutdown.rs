//! Shutdown cancels a live draft probe before draining the serialized command queue. Queued
//! probes observe the current stop signal and never execute their commands.
#![cfg(unix)]

mod common;

use std::path::Path;
use std::time::Duration;

use grok_bot_core::app::{Backend, BackendOptions};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout};

async fn request(port: u16, bot: &str, marker: &Path, active: bool) -> TcpStream {
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
    let head = format!(
        "POST /api/bots/{bot}/mcp-servers/test HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nX-Grok-Bot: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();
    stream
}

async fn process_ids(marker: &Path) -> (i32, i32) {
    timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(bytes) = tokio::fs::read(marker).await
                && let Ok(record) = serde_json::from_slice::<Value>(&bytes)
            {
                return (record["pid"].as_i64().unwrap() as i32, record["child"].as_i64().unwrap() as i32);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("active draft started")
}

async fn response(mut stream: TcpStream) -> String {
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 503"), "{response}");
    assert!(response.contains("MCP 테스트 중단"), "queued handler already passed the HTTP guard before shutdown: {response}");
    response
}

#[test]
fn shutdown_reaps_active_probe_and_does_not_spawn_queued_probe() {
    common::run("mcp-probe-shutdown", async |root| {
        let data_dir = root.join("data");
        let backend = Backend::new(BackendOptions {
            data_dir: data_dir.clone(),
            engine_root: root.join("engine"),
            web_root: None,
            allowed_hosts: vec![],
            now: None,
            scheduler: None,
            runtime: None,
        })
        .await
        .unwrap();
        let store = backend.store.clone();
        let bot = store.snapshot().bots[0].id.clone();
        let before = serde_json::to_value(store.snapshot()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(backend.serve(listener, async {
            let _ = stopped.await;
        }));

        let active_marker = root.join("active.json");
        let queued_marker = root.join("queued-must-not-start");
        let active = request(port, &bot, &active_marker, true).await;
        let (pid, child) = process_ids(&active_marker).await;
        let queued = request(port, &bot, &queued_marker, false).await;
        // There can be no response while the active probe holds the command queue. The response
        // check below also proves this handler passed the HTTP guard BEFORE closing became true.
        assert!(timeout(Duration::from_millis(100), queued.peek(&mut [0u8; 1])).await.is_err());
        assert!(!queued_marker.exists());

        let started = Instant::now();
        stop.send(()).unwrap();
        timeout(Duration::from_secs(3), async {
            let (_active, _queued, server) = tokio::join!(response(active), response(queued), task);
            server.unwrap().unwrap();
        })
        .await
        .expect("shutdown must interrupt active and queued probes rather than waiting 10 seconds each");
        assert!(started.elapsed() < Duration::from_secs(3));
        // The Backend has completed graceful shutdown; the worker must already have waited for
        // the direct child, not merely detached a cancellation task that may lose the shutdown race.
        // SAFETY: signal 0 probes the PID recorded by this test's own child.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "active probe reaped before Backend exits");
        assert!(!queued_marker.exists(), "queued stdio must never be spawned during shutdown");
        timeout(Duration::from_secs(3), async {
            loop {
                let status = tokio::fs::read_to_string(format!("/proc/{child}/stat")).await.unwrap_or_default();
                if status.is_empty() || status.split_whitespace().nth(2) == Some("Z") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("process-group descendant stopped");
        assert_eq!(serde_json::to_value(store.snapshot()).unwrap(), before, "probes never changed the store");
    });
}
