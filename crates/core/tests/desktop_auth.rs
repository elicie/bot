//! Desktop authorization is opt-in and does not replace existing MCP/webhook capabilities.
mod common;
use grok_bot_core::{
    app::{Backend, BackendOptions},
    desktop_auth::DesktopToken,
};

#[test]
fn desktop_token_covers_reads_events_media_and_mutations_without_changing_web() {
    common::run("desktop-auth", async |root| {
        for desktop in [false, true] {
            let options = BackendOptions {
                data_dir: root.join(if desktop { "desktop" } else { "web" }),
                engine_root: root.join("engine"),
                web_root: None,
                embedded_web: Some(&[("index.html", b"<!doctype html><title>App</title>")]),
                allowed_hosts: vec![],
                now: None,
                scheduler: None,
                runtime: None,
            };
            let token = DesktopToken::generate().unwrap();
            let secret = token.as_str().to_owned();
            let backend = if desktop { Backend::for_desktop(options, token).await } else { Backend::new(options).await }.unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(backend.serve(listener, async move {
                let _ = stopped.await;
            }));
            let host = vec![("Host".into(), format!("127.0.0.1:{port}"))];
            assert_eq!(common::raw(port, "GET", "/api/state", &host, None).await.0, if desktop { 401 } else { 200 });
            let (status, html) = common::raw(port, "GET", "/", &host, None).await;
            assert_eq!(status, 200);
            assert!(!html.contains(&secret));
            if desktop {
                for route in ["/api/events", "/api/file?path=notes.txt", "/api/computer/preview", "/api/missing"] {
                    assert_eq!(common::raw(port, "GET", route, &host, None).await.0, 401, "{route}");
                }
                let mut wrong = host.clone();
                wrong.push(("X-Grok-Desktop-Token".into(), "wrong".into()));
                assert_eq!(common::raw(port, "GET", "/api/state", &wrong, None).await.0, 401);
                let mut authorized = host.clone();
                authorized.push(("X-Grok-Desktop-Token".into(), secret.clone()));
                assert_eq!(common::raw(port, "GET", "/api/state", &authorized, None).await.0, 200);
                let client = reqwest::Client::new();
                let events = client.get(format!("http://127.0.0.1:{port}/api/events")).header("X-Grok-Desktop-Token", &secret).send().await.unwrap();
                assert_eq!(events.status(), 200);
                assert_eq!(events.headers()["content-type"], "text/event-stream");
                drop(events);
                authorized.push(("Origin".into(), "https://evil.test".into()));
                assert_eq!(common::raw(port, "GET", "/api/state", &authorized, None).await.0, 403);
                let mut mutation = host.clone();
                mutation.extend([
                    ("Origin".into(), format!("http://127.0.0.1:{port}")),
                    ("X-Grok-Bot".into(), "1".into()),
                    ("Content-Type".into(), "application/json".into()),
                ]);
                assert_eq!(common::raw(port, "POST", "/api/conversations", &mutation, Some("{}")).await.0, 401);
                mutation.push(("X-Grok-Desktop-Token".into(), secret));
                // Authentication does not skip normal request/schema validation.
                assert_ne!(common::raw(port, "POST", "/api/conversations", &mutation, Some("{}")).await.0, 401);
                assert_eq!(common::raw(port, "POST", "/internal/mcp/not-a-run", &host, Some("{}")).await.0, 403);
                assert_ne!(common::raw(port, "GET", "/apiary", &host, None).await.0, 401);
            }
            let _ = stop.send(());
            task.await.unwrap().unwrap();
        }
    });
}
