//! Canvas HTTP admission and revision contract, including edits while a model run is active.

use crate::common;

use common::{TestApp, run};
use grok_bot_model::{RunStatus, iso};
use serde_json::{Value, json};

fn input(revision: u64, content: &str) -> Value {
    json!({"revision": revision, "title": "Draft", "language": "markdown", "content": content})
}

#[test]
fn conversation_canvas_http() {
    run("canvas-http", async |root| {
        let engine = root.join("engine");
        let data = root.join("data");
        let app = TestApp::start(&engine, data.clone()).await;
        let bot = app.store.snapshot().bots[0].id.clone();
        let (_, created) = app.api("POST", "/api/conversations", Some(json!({"botId": bot}))).await;
        let id = created["conversation"]["id"].as_str().unwrap().to_owned();
        let route = format!("/api/conversations/{id}/canvas");
        let restore = format!("{route}/restore");

        // Canvas access needs neither provider configuration nor a workspace. Every new mutation
        // still passes the same Host/Origin/JSON guard as all other app commands.
        for (header, value) in [("Origin", "http://evil.invalid"), ("Origin", ""), ("X-Grok-Bot", ""), ("Content-Type", "text/plain")] {
            assert_eq!(app.request("PUT", &route, Some(input(0, "first").to_string()), &[(header, value)]).await.0, 403);
        }
        assert_eq!(
            app.api("PUT", &route, Some(json!({"revision": 0, "title": "Draft", "language": "markdown", "content": "first", "extra": true}))).await.0,
            400
        );
        let (status, first) = app.api("PUT", &route, Some(input(0, "앞 문단 😀\r\n\r\n길게 쓴 문단입니다.\r\n"))).await;
        assert_eq!(status, 200, "{first}");
        assert_eq!(first["conversations"][0]["canvas"]["revision"], 1);
        assert_eq!(first["conversations"][0]["canvas"]["source"], "user");
        assert_eq!(first["conversations"][0]["canvas"]["history"], json!([]));
        assert!(first["settings"]["workspace"].as_str().unwrap().is_empty());
        assert_eq!(app.api("PUT", &route, Some(input(0, "stale create"))).await.0, 409);

        // Narrow canvas mutation remains admitted during a model run; other idle-guarded commands
        // keep their existing policy. No engine prompt is started by this HTTP admission test.
        app.store
            .mutate(|state| {
                let conversation = state.conversation_mut(&id)?;
                conversation.run.id = grok_bot_model::new_id();
                conversation.run.status = RunStatus::Running;
                conversation.run.started_at = Some(iso(chrono::Utc::now()));
                Ok(())
            })
            .await
            .unwrap();
        let (status, second) = app.api("PUT", &route, Some(input(1, "앞 문단 😀\r\n\r\n짧은 문단.\r\n"))).await;
        assert_eq!(status, 200, "{second}");
        assert_eq!(second["conversations"][0]["run"]["status"], "running");
        assert_eq!(second["conversations"][0]["canvas"]["revision"], 2);
        assert_eq!(second["conversations"][0]["canvas"]["history"][0]["revision"], 1);
        assert_eq!(app.api("PUT", &route, Some(input(1, "stale replacement"))).await.0, 409);
        assert_eq!(app.api("POST", &restore, Some(json!({"revision": 1, "targetRevision": 1}))).await.0, 409);
        assert_eq!(app.api("POST", &restore, Some(json!({"revision": 2, "targetRevision": 999}))).await.0, 404);
        assert_eq!(
            app.request("POST", &restore, Some(json!({"revision": 2, "targetRevision": 1}).to_string()), &[("Origin", "http://evil.invalid")]).await.0,
            403
        );
        let (status, restored) = app.api("POST", &restore, Some(json!({"revision": 2, "targetRevision": 1}))).await;
        assert_eq!(status, 200, "{restored}");
        let canvas = &restored["conversations"][0]["canvas"];
        assert_eq!(canvas["revision"], 3);
        assert_eq!(canvas["source"], "restore");
        assert_eq!(canvas["content"], first["conversations"][0]["canvas"]["content"]);
        assert_eq!(canvas["history"].as_array().unwrap().len(), 2);
        assert_eq!(app.api("POST", &restore, Some(json!({"revision": 2, "targetRevision": 1}))).await.0, 409);

        // Invalid payloads never replace saved contents or append a version.
        for bad in [
            json!({"revision": 3, "title": " ", "language": "markdown", "content": "x"}),
            json!({"revision": 3, "title": "Draft", "language": "../code", "content": "x"}),
            input(3, "nul\0text"),
            input(3, &"x".repeat(128 * 1024 + 1)),
        ] {
            let (status, error) = app.api("PUT", &route, Some(bad)).await;
            assert!((400..500).contains(&status), "{status}: {error}");
            assert_eq!(app.store.snapshot().conversations[0].canvas.as_ref().unwrap().revision, 3);
        }
        app.close().await;
        let reopened = TestApp::start(&engine, data).await;
        assert_eq!(reopened.store.snapshot().conversations[0].canvas.as_ref().unwrap().revision, 3);
        assert_eq!(reopened.store.snapshot().conversations[0].run.status, RunStatus::Interrupted);

        // Once an old document string becomes a protected secret, response history must use the
        // redacted public snapshot rather than the raw Canvas returned from store.save_canvas.
        const KEY: &str = "synthetic-http-canvas-secret";
        assert_eq!(reopened.api("PUT", &route, Some(input(3, KEY))).await.0, 200);
        assert_eq!(reopened.api("PUT", &route, Some(input(4, "Safe current text"))).await.0, 200);
        let settings = json!({"baseUrl": "http://127.0.0.1:12345/v1", "model": "fixture", "workspace": "", "apiKey": KEY});
        assert_eq!(reopened.api("PUT", "/api/settings", Some(settings)).await.0, 200);
        let (status, public) = reopened.api("PUT", &route, Some(input(5, "Safe next text"))).await;
        assert_eq!(status, 200, "{public}");
        assert!(!public.to_string().contains(KEY));
        assert!(public.to_string().contains("[redacted]"));
        assert_eq!(public["conversations"][0]["canvas"]["history"][3]["redacted"], true);
        reopened.close().await;
    });
}
