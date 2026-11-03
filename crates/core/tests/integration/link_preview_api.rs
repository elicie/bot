//! The public HTTP setting/capability and restart contract. Download safety is exercised with
//! isolated injected fixtures in the service's unit tests; this integration test makes no outbound
//! requests and requires no test-origin environment override.

use crate::common;

use common::{TestApp, run};
use serde_json::{Value, json};

#[test]
fn link_preview_setting_http_schema_guards_and_restart() {
    run("link-preview-api", async |root| {
        let engine = root.join("engine");
        let data = root.join("data");
        let app = TestApp::start(&engine, data.clone()).await;
        let (_, state) = app.api("GET", "/api/state", None).await;
        assert_eq!(state["linkPreviewsEnabled"], false);
        let (status, error) = app.api("GET", "/api/link-preview?url=http%3A%2F%2F8.8.8.8%2F", None).await;
        assert_eq!(status, 403);
        assert_eq!(error["error"], "Link previews are disabled.");
        for value in ["", "wrong"] {
            let (status, error) = app.request("GET", "/api/link-preview?url=http%3A%2F%2F8.8.8.8%2F", None, &[("X-Grok-Bot", value), ("Origin", "")]).await;
            assert_eq!(status, 403);
            assert_eq!(serde_json::from_str::<Value>(&error).unwrap()["error"], "Link previews require X-Grok-Bot: 1.");
        }
        let route = "/api/link-preview/settings";
        assert_eq!(app.request("PUT", route, Some(json!({"enabled":true}).to_string()), &[("X-Grok-Bot", "")]).await.0, 403);
        for body in [json!({}), json!({"enabled":"true"}), json!({"enabled":true,"extra":true})] {
            assert_eq!(app.api("PUT", route, Some(body)).await.0, 400);
        }
        let (status, state) = app.api("PUT", route, Some(json!({"enabled":true}))).await;
        assert_eq!(status, 200);
        assert_eq!(state["linkPreviewsEnabled"], true);
        assert!(state["bots"].is_array());
        assert_eq!(app.api("GET", "/api/link-preview", None).await.0, 400);
        assert_eq!(app.api("GET", "/api/link-preview?url=http%3A%2F%2F127.0.0.1%2F", None).await.0, 403);
        assert_eq!(app.api("GET", "/api/link-preview?url=http%3A%2F%2F%5B%3A%3A1%5D%2F", None).await.0, 403);
        assert_eq!(app.api("GET", "/api/link-preview?url=file%3A%2F%2F%2Fetc%2Fpasswd", None).await.0, 403);
        let disk: Value = serde_json::from_str(&std::fs::read_to_string(data.join("state.json")).unwrap()).unwrap();
        assert_eq!(disk["linkPreviewsEnabled"], true);
        app.close().await;
        let app = TestApp::start(&engine, data.clone()).await;
        assert!(app.store.snapshot().link_previews_enabled);
        let (status, state) = app.api("PUT", route, Some(json!({"enabled":false}))).await;
        assert_eq!(status, 200);
        assert_eq!(state["linkPreviewsEnabled"], false);
        assert_eq!(app.api("GET", "/api/link-preview?url=http%3A%2F%2F8.8.8.8%2F", None).await.0, 403);
        app.close().await;
        let app = TestApp::start(&engine, data.clone()).await;
        assert!(!app.store.snapshot().link_previews_enabled);
        let disk: Value = serde_json::from_str(&std::fs::read_to_string(data.join("state.json")).unwrap()).unwrap();
        assert!(disk.get("linkPreviewsEnabled").is_none());
        app.close().await;
    });
}
