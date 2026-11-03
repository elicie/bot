//! Shared HTTP test harness: an in-process `Backend` on a random loopback port.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use grok_bot_core::app::{Backend, BackendOptions};
use grok_bot_store::Store;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Prepare the process-wide engine environment, then run `body` on a multi-thread runtime.
/// Each test binary must call this exactly once, from its single `#[test]`.
pub fn run(name: &str, body: impl AsyncFnOnce(PathBuf)) {
    let root = std::env::temp_dir().join(format!("grok-bot-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // SAFETY: called before the test's runtime and any engine thread start.
    unsafe { grok_bot_engine::host::prepare_process_environment(&root.join("engine")) };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    runtime.block_on(body(root.clone()));
    drop(runtime);
    let _ = std::fs::remove_dir_all(&root);
}

pub struct TestApp {
    pub origin: String,
    pub port: u16,
    pub data_dir: PathBuf,
    pub store: Arc<Store>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

pub struct AppOptions<'a> {
    pub engine_root: &'a Path,
    pub data_dir: PathBuf,
    pub allowed_hosts: Vec<String>,
}

impl TestApp {
    pub async fn try_start(options: AppOptions<'_>) -> anyhow::Result<Self> {
        Self::try_start_with_runtime(options, None).await
    }

    pub async fn try_start_with_runtime(options: AppOptions<'_>, runtime: Option<grok_bot_core::runtime::RuntimeOptions>) -> anyhow::Result<Self> {
        let backend = Backend::new(BackendOptions {
            data_dir: options.data_dir.clone(),
            engine_root: options.engine_root.to_path_buf(),
            web_root: None,
            embedded_web: None,
            allowed_hosts: options.allowed_hosts,
            now: None,
            scheduler: None,
            runtime,
        })
        .await?;
        let store = backend.store.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(backend.serve(listener, async move {
            let _ = stopped.await;
        }));
        Ok(Self { origin: format!("http://127.0.0.1:{port}"), port, data_dir: options.data_dir, store, stop: Some(stop), task: Some(task) })
    }

    pub async fn start(engine_root: &Path, data_dir: PathBuf) -> Self {
        Self::try_start(AppOptions { engine_root, data_dir, allowed_hosts: vec![] }).await.expect("backend starts")
    }

    pub async fn close(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.await.expect("server task").expect("server closes");
        }
    }

    /// Same-origin JSON request like the browser client. `body == None` sends no body.
    pub async fn api(&self, method: &str, route: &str, body: Option<Value>) -> (u16, Value) {
        let (status, text) = self.request(method, route, body.map(|b| b.to_string()), &[]).await;
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    pub async fn request(&self, method: &str, route: &str, body: Option<String>, headers: &[(&str, &str)]) -> (u16, String) {
        let mut all: Vec<(String, String)> = vec![
            ("Host".into(), format!("127.0.0.1:{}", self.port)),
            ("Origin".into(), self.origin.clone()),
            ("X-Grok-Bot".into(), "1".into()),
            ("Content-Type".into(), "application/json".into()),
        ];
        for (name, value) in headers {
            all.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
            if !value.is_empty() || name.eq_ignore_ascii_case("x-grok-bot") {
                all.push(((*name).into(), (*value).into()));
            }
        }
        raw(self.port, method, route, &all, body.as_deref()).await
    }
}

/// Minimal HTTP/1.1 client so tests control the exact Host and Origin headers.
pub async fn raw(port: u16, method: &str, route: &str, headers: &[(String, String)], body: Option<&str>) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut request = format!("{method} {route} HTTP/1.1\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(body) = body {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    if let Some(body) = body {
        stream.write_all(body.as_bytes()).await.unwrap();
    }
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response).await;
    let text = String::from_utf8_lossy(&response).into_owned();
    let status = text.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b.to_owned()).unwrap_or_default();
    let chunked = text.split_once("\r\n\r\n").is_some_and(|(h, _)| h.to_ascii_lowercase().contains("transfer-encoding: chunked"));
    (status, if chunked { dechunk(&body) } else { body })
}

fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let Ok(size) = usize::from_str_radix(size.trim(), 16) else { break };
        if size == 0 || tail.len() < size {
            break;
        }
        out.push_str(&tail[..size]);
        rest = tail[size..].trim_start_matches("\r\n");
    }
    out
}
