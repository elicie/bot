//! Proposal/decision HTTP admission tests. Seed only a durable app artifact; proposals and denials
//! must work without contacting its intentionally unavailable MCP endpoint or starting an LLM.

mod common;

use common::{TestApp, run};
use grok_bot_model::{McpApp, Message, Role, ToolActivity, ToolStatus, iso, new_id};
use serde_json::{Value, json};

async fn seed(app: &TestApp) -> (String, String, String) {
    app.store
        .mutate(|state| {
            let bot = state.disk().bots[0].id.clone();
            state.set_mcp_servers(&bot, r#"{"apps":{"url":"http://127.0.0.1:9/mcp"}}"#, true)?;
            state.set_mcp_server_enabled(&bot, "apps", true, true)?;
            let server = state.mcp_servers_for(&bot).next().unwrap().clone();
            let conversation = state.add_conversation(&bot)?.id;
            let activity = new_id();
            let message = Message {
                id: new_id(),
                role: Role::Assistant,
                text: "A persisted MCP app".into(),
                created_at: iso(state.now()),
                tools: vec![ToolActivity {
                    id: activity.clone(),
                    title: "apps__render".into(),
                    kind: "other".into(),
                    status: ToolStatus::Completed,
                    input: None,
                    output: Some("counter: 0".into()),
                    paths: None,
                    local_only: None,
                    approved_by_rule: None,
                    mcp_app: Some(McpApp {
                        bot_id: bot.clone(),
                        server_id: server.id,
                        server_updated_at: server.updated_at,
                        server: "apps".into(),
                        tool: "render".into(),
                        uri: "ui://fixture/app".into(),
                        html: "<!doctype html><p>app</p>".into(),
                        input: "{}".into(),
                        output: "counter: 0".into(),
                    }),
                }],
                bot_exchange_ids: None,
                source_bot_id: None,
                speaker_bot_id: None,
                attachments: None,
                report_delivery_ids: None,
            };
            state.conversation_mut(&conversation)?.messages.push(message);
            Ok((conversation, activity, bot))
        })
        .await
        .unwrap()
}

#[test]
fn app_proposals_are_same_origin_bounded_bound_and_one_use() {
    run("mcp-app-http", async |root| {
        let app = TestApp::start(&root.join("engine"), root.join("data")).await;
        let (conversation, activity, bot) = seed(&app).await;
        let route = format!("/api/conversations/{conversation}/mcp-apps/{activity}/requests");
        let body = json!({"name": "render", "arguments": {"delta": 1}});
        for headers in [[("Origin", "http://evil.invalid")], [("Origin", "")], [("X-Grok-Bot", "")]] {
            assert_eq!(app.request("POST", &route, Some(body.to_string()), &headers).await.0, 403);
        }
        for invalid in [
            json!({"name": "render", "arguments": [], "server": "workspace"}),
            json!({"name": "workspace__write_file", "arguments": {}}),
            json!({"name": "render", "arguments": {"large": "한".repeat(12_000)}}),
            json!({"name": "render", "arguments": {}, "url": "http://example.invalid"}),
        ] {
            assert_eq!(app.api("POST", &route, Some(invalid)).await.0, 400);
        }
        let (status, created) = app.api("POST", &route, Some(body.clone())).await;
        assert_eq!(status, 201);
        let request = created["requestId"].as_str().unwrap();
        assert_eq!(
            app.api("POST", &format!("/api/conversations/{conversation}/mcp-apps/{}/requests/{request}", new_id()), Some(json!({"allow": true}))).await.0,
            409
        );
        let decision = format!("{route}/{request}");
        let (status, denied) = app.api("POST", &decision, Some(json!({"allow": false}))).await;
        assert_eq!(status, 200);
        assert_eq!(denied["isError"], true);
        assert!(denied["content"][0]["text"].as_str().unwrap().contains("did not run"));
        assert_eq!(app.api("POST", &decision, Some(json!({"allow": true}))).await.0, 409);

        // Hidden local-only activity cannot gain a new LAN-readable result path, even if its
        // activity ID is known from the public placeholder.
        app.store
            .mutate(|state| {
                state.conversation_mut(&conversation)?.messages[0].tools[0].local_only = Some(true);
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(app.api("POST", &route, Some(body.clone())).await.0, 409);
        app.store
            .mutate(|state| {
                state.conversation_mut(&conversation)?.messages[0].tools[0].local_only = None;
                Ok(())
            })
            .await
            .unwrap();

        // Pending request limits are bounded even when a hostile app never answers its cards.
        for _ in 0..8 {
            assert_eq!(app.api("POST", &route, Some(body.clone())).await.0, 201);
        }
        assert_eq!(app.api("POST", &route, Some(body.clone())).await.0, 409);
        assert_eq!(app.api("POST", &format!("/api/conversations/{conversation}/cancel"), Some(json!({}))).await.0, 200);
        let (_, renewed) = app.api("POST", &route, Some(body.clone())).await;
        let request = renewed["requestId"].as_str().unwrap();

        // Disable/re-enable is a configuration epoch change, even on clocks returning the same ms.
        app.store.mutate(|state| state.set_mcp_server_enabled(&bot, "apps", false, true)).await.unwrap();
        app.store.mutate(|state| state.set_mcp_server_enabled(&bot, "apps", true, true)).await.unwrap();
        assert_eq!(app.api("POST", &format!("{route}/{request}"), Some(json!({"allow": true}))).await.0, 409);
        assert_eq!(app.api("POST", &route, Some(body)).await.0, 409);
        let (_, snapshot) = app.api("GET", "/api/state", None).await;
        assert!(snapshot["conversations"][0]["messages"][0]["tools"][0]["mcpApp"]["html"].as_str().unwrap().contains("app"));
        let _typed: Value = snapshot;
        app.close().await;
    });
}
