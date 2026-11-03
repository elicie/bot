//! HTTP routes for bot memories, knowledge files, and templates.

use crate::common;

use std::path::Path;

use common::{TestApp, run};
use serde_json::{Value, json};

#[test]
fn bot_data_routes() {
    run("http-bots", async |root| {
        let engine = root.join("engine");
        memories(&root, &engine).await;
        knowledge(&root, &engine).await;
        templates(&root, &engine).await;
    });
}

async fn start(root: &Path, engine: &Path, name: &str) -> (TestApp, String) {
    let app = TestApp::start(engine, root.join(name)).await;
    let (_, state) = app.api("GET", "/api/state", None).await;
    let bot = state["bots"][0]["id"].as_str().unwrap().to_owned();
    (app, bot)
}

async fn memories(root: &Path, engine: &Path) {
    let (app, bot) = start(root, engine, "memories").await;
    let route = format!("/api/bots/{bot}/memories");
    let (status, body) = app.api("POST", &route, Some(json!({ "texts": ["Prefers short answers"] }))).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["added"][0]["text"], "Prefers short answers");
    assert_eq!(body["state"]["bots"][0]["memories"][0]["source"], "user");
    let (status, body) = app.api("POST", &route, Some(json!({ "texts": ["prefers SHORT answers"] }))).await;
    assert_eq!((status, body["added"].as_array().unwrap().len(), body["existing"].as_array().unwrap().len()), (200, 0, 1));
    let (status, body) = app.api("POST", &route, Some(json!({ "texts": ["x"], "extra": true }))).await;
    assert_eq!(status, 400, "{body}");

    let (status, preview) = app.api("POST", &format!("{route}/preview"), Some(json!({ "text": "- Prefers short answers\n- Lives in Busan\n" }))).await;
    assert_eq!(status, 200, "{preview}");
    assert_eq!(preview["memories"], json!([{ "text": "Prefers short answers", "duplicate": true }, { "text": "Lives in Busan", "duplicate": false }]));
    let (status, body) = app.api("POST", &route, Some(json!({ "texts": ["Lives in Busan"], "imported": true }))).await;
    assert_eq!(status, 201);
    let memories = body["state"]["bots"][0]["memories"].as_array().unwrap().clone();
    assert_eq!(memories[1]["source"], "import");

    let id = memories[0]["id"].as_str().unwrap();
    let (status, body) = app.api("DELETE", &format!("{route}/{id}"), Some(json!({}))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["bots"][0]["memories"].as_array().unwrap().len(), 1);
    assert_eq!(app.api("DELETE", &format!("{route}/{id}"), Some(json!({}))).await.0, 404);
    let (status, _) = app.request("POST", &route, Some(json!({ "texts": ["no header"] }).to_string()), &[("X-Grok-Bot", "")]).await;
    assert_eq!(status, 403, "mutations keep the same-origin JSON guard");
    app.close().await;
}

async fn knowledge(root: &Path, engine: &Path) {
    let (app, bot) = start(root, engine, "knowledge").await;
    let route = format!("/api/bots/{bot}/knowledge");
    let (status, body) = app.api("POST", &route, Some(json!({ "name": "handbook.md", "content": "# Handbook\n연차는 15일" }))).await;
    assert_eq!(status, 201, "{body}");
    let file: Value = body["bots"][0]["knowledge"][0].clone();
    assert_eq!((file["name"].as_str(), file["bytes"].as_u64()), (Some("handbook.md"), Some("# Handbook\n연차는 15일".len() as u64)));
    let id = file["id"].as_str().unwrap();
    let (status, document) = app.api("GET", &format!("{route}/{id}"), None).await;
    assert_eq!((status, document["content"].as_str()), (200, Some("# Handbook\n연차는 15일")));
    assert_eq!(document["file"], file);
    let (status, body) = app.api("POST", &route, Some(json!({ "name": "big.txt", "content": "a".repeat(512 * 1024 + 1) }))).await;
    assert_eq!(status, 413, "{body}");
    assert_eq!(app.api("GET", &format!("{route}/00000000-0000-4000-8000-000000000000"), None).await.0, 404);
    let (status, body) = app.api("DELETE", &format!("{route}/{id}"), Some(json!({}))).await;
    assert_eq!(status, 200);
    assert!(body["bots"][0].get("knowledge").is_none());
    assert!(std::fs::read_dir(app.data_dir.join("knowledge").join(&bot)).unwrap().next().is_none());
    app.close().await;
}

async fn templates(root: &Path, engine: &Path) {
    let (app, bot) = start(root, engine, "templates").await;
    app.api("PATCH", &format!("/api/bots/{bot}"), Some(json!({ "name": "Reviewer", "instructions": "Review carefully.", "shape": "heart" }))).await;
    app.api(
        "POST",
        &format!("/api/bots/{bot}/skills"),
        Some(json!({ "name": "review", "description": "Use to review.", "instructions": "Check everything." })),
    )
    .await;
    app.api("POST", &format!("/api/bots/{bot}/memories"), Some(json!({ "texts": ["Team uses Rust"] }))).await;
    let (status, template) = app.api("GET", &format!("/api/bots/{bot}/template"), None).await;
    assert_eq!(status, 200, "{template}");
    assert_eq!(template["format"], "grok-bot.bot-template");
    assert!(template.get("memories").is_none());
    let (_, with_memories) = app.api("GET", &format!("/api/bots/{bot}/template?memories=true"), None).await;
    assert_eq!(with_memories["memories"], json!([{ "text": "Team uses Rust" }]));
    assert_eq!(app.api("GET", &format!("/api/bots/{bot}/template?other=1"), None).await.0, 400);

    let (status, created) = app.api("POST", "/api/bot-templates", Some(json!({ "template": with_memories }))).await;
    assert_eq!(status, 201, "{created}");
    assert_ne!(created["bot"]["id"], json!(bot));
    assert_eq!((created["bot"]["name"].as_str(), created["bot"]["shape"].as_str()), (Some("Reviewer"), Some("heart")));
    assert_eq!(created["state"]["bots"].as_array().unwrap().len(), 2);
    assert_ne!(created["bot"]["skills"][0]["id"], with_memories["skills"][0].get("id").cloned().unwrap_or(Value::Null));

    let mut tampered = template.clone();
    tampered["bot"]["id"] = json!(bot);
    let (status, body) = app.api("POST", "/api/bot-templates", Some(json!({ "template": tampered }))).await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"].as_str().unwrap().contains("unknown field"), "{body}");
    let mut wrong = template.clone();
    wrong["format"] = json!("something-else");
    assert_eq!(app.api("POST", "/api/bot-templates", Some(json!({ "template": wrong }))).await.1["error"], "This file is not a Grok Bot template.");
    let (_, state) = app.api("GET", "/api/state", None).await;
    assert_eq!(state["bots"].as_array().unwrap().len(), 2);
    app.close().await;
}
