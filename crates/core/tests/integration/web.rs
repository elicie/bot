//! The embedded frontend stays behind the same Host/Origin/security middleware as disk files.
use crate::common;

use grok_bot_core::app::{Backend, BackendOptions};
use grok_bot_core::web::EmbeddedWeb;

const ASSETS: EmbeddedWeb = &[("assets/app.js", b"window.APP = true;"), ("index.html", b"<!doctype html><title>Embedded</title>")];

#[test]
fn embedded_frontend_keeps_http_guards_and_disk_override() {
    common::run("embedded-web", async |root| {
        // Exercising the real listener includes the socket-based API policy and shutdown path.
        let disk = root.join("web");
        std::fs::create_dir_all(&disk).unwrap();
        std::fs::write(disk.join("index.html"), "<!doctype html><title>Disk override</title>").unwrap();
        for web_root in [None, Some(disk)] {
            let backend = Backend::new(BackendOptions {
                data_dir: root.join(if web_root.is_some() { "override-data" } else { "embedded-data" }),
                engine_root: root.join("engine"),
                web_root: web_root.clone(),
                embedded_web: Some(ASSETS),
                allowed_hosts: vec![],
                now: None,
                scheduler: None,
                runtime: None,
            })
            .await
            .unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
            let task = tokio::spawn(backend.serve(listener, async move {
                let _ = stopped.await;
            }));
            let host = vec![("Host".to_owned(), format!("localhost:{port}"))];
            let (status, body) = common::raw(port, "GET", "/", &host, None).await;
            assert_eq!(status, 200);
            assert!(body.contains(if web_root.is_some() { "Disk override" } else { "Embedded" }));
            for route in ["/internals", "/internal-preview"] {
                let (status, body) = common::raw(port, "GET", route, &host, None).await;
                assert_eq!(status, 200, "{route}");
                assert!(body.contains(if web_root.is_some() { "Disk override" } else { "Embedded" }));
            }
            for route in ["/api/not-found", "/internal", "/internal/not-found"] {
                assert_eq!(common::raw(port, "GET", route, &host, None).await.0, 404);
            }
            assert_eq!(common::raw(port, "GET", "/", &[("Host".into(), format!("evil.test:{port}"))], None).await.0, 403);
            let mut cross_origin = host.clone();
            cross_origin.push(("Origin".into(), "https://evil.test".into()));
            assert_eq!(common::raw(port, "GET", "/assets/app.js", &cross_origin, None).await.0, 403);
            let _ = stop.send(());
            task.await.unwrap().unwrap();
        }
    });
}
