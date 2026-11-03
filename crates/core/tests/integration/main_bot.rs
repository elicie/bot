//! HTTP route for choosing the main bot (`POST /api/bots/:id/main`).

use crate::common;

use common::{TestApp, run};
use serde_json::json;

#[test]
fn main_bot_route() {
    run("main-bot", async |root| {
        let app = TestApp::start(&root.join("engine"), root.join("data")).await;
        let (_, state) = app.api("GET", "/api/state", None).await;
        let first = state["bots"][0]["id"].as_str().unwrap().to_owned();
        assert!(state.get("mainBotId").is_none(), "unset: the first bot is main by default");

        let (_, state) = app.api("POST", "/api/bots", Some(json!({ "name": "Second" }))).await;
        let second = state["bots"][1]["id"].as_str().unwrap().to_owned();
        let (status, state) = app.api("POST", &format!("/api/bots/{second}/main"), Some(json!({}))).await;
        assert_eq!(status, 200, "{state}");
        assert_eq!(state["mainBotId"], second.as_str());
        assert_eq!(state["bots"][0]["pinned"], true, "the previous main bot stays pinned");

        assert_eq!(app.api("POST", &format!("/api/bots/{second}/main"), Some(json!({ "extra": 1 }))).await.0, 400);
        assert_eq!(app.api("POST", &format!("/api/bots/{}/main", uuid::Uuid::new_v4()), Some(json!({}))).await.0, 404);
        let (status, _) = app.request("POST", &format!("/api/bots/{first}/main"), Some("{}".into()), &[("X-Grok-Bot", "")]).await;
        assert_eq!(status, 403, "mutations keep the same-origin JSON guard");

        // Deleting the main bot makes the first remaining bot main.
        let (status, state) = app.api("DELETE", &format!("/api/bots/{second}"), Some(json!({}))).await;
        assert_eq!(status, 200, "{state}");
        assert!(state.get("mainBotId").is_none(), "the choice is cleared: the first remaining bot is main");
        assert_eq!(state["bots"][0]["id"], first.as_str());
        app.close().await;
    });
}
