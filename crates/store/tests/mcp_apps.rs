//! MCP app display artifacts are durable, bounded, actor-owned, and explicitly redacted. They do
//! not participate in model-run permission or execution state.

mod common;

use common::{Fixture, assert_load_rejected, write_json};
use grok_bot_model::*;
use grok_bot_store::mcp_apps::{conversation_mcp_apps_valid, persisted_mcp_app_valid};
use serde_json::{Value, json};

const CAPTURED_AT: &str = "2026-01-01T00:00:00.000Z";

fn app(bot: &str) -> McpApp {
    McpApp {
        bot_id: bot.into(),
        server_id: new_id(),
        server_updated_at: CAPTURED_AT.into(),
        server: "records".into(),
        tool: "lookup".into(),
        uri: "ui://records/app.html".into(),
        html: "<!doctype html><p>기록</p><script>const count = 1;</script>".into(),
        input: r#"{"query":"record"}"#.into(),
        output: "Record found".into(),
    }
}

fn tool(app: Option<McpApp>) -> ToolActivity {
    ToolActivity {
        id: new_id(),
        title: "Look up record".into(),
        kind: "other".into(),
        status: ToolStatus::Completed,
        input: Some("Existing tool input".into()),
        output: Some("Existing tool output".into()),
        paths: None,
        local_only: None,
        approved_by_rule: None,
        mcp_app: app,
    }
}

fn message(app: McpApp, speaker: Option<&str>) -> Message {
    Message {
        id: new_id(),
        role: Role::Assistant,
        text: "Initial answer".into(),
        created_at: CAPTURED_AT.into(),
        tools: vec![tool(Some(app))],
        bot_exchange_ids: None,
        source_bot_id: None,
        speaker_bot_id: speaker.map(str::to_owned),
        attachments: None,
        report_delivery_ids: None,
    }
}

async fn append(f: &Fixture, conversation: &str, message: Message) {
    f.store
        .mutate(|state| {
            state.conversation_mut(conversation)?.messages.push(message);
            Ok(())
        })
        .await
        .unwrap();
}

fn stored_app(disk: &mut Value) -> &mut Value {
    &mut disk["conversations"][0]["messages"][0]["tools"][0]["mcpApp"]
}

#[tokio::test]
async fn old_state_omits_apps_and_new_resources_survive_restart_without_changing_run_state() {
    let f = Fixture::new().await;
    let conversation = f.store.mutate(|s| s.add_conversation(&f.bot)).await.unwrap();
    let legacy_tool = tool(None);
    assert!(serde_json::to_value(legacy_tool).unwrap().get("mcpApp").is_none());
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.snapshot().conversations[0], conversation);
    drop(reopened);

    let resource = app(&f.bot);
    append(&f, &conversation.id, message(resource.clone(), None)).await;
    assert!(f.store.read(|s| s.disk().mcp_servers.is_empty()), "historical resources need no current server record");
    let stored = f.conversation(&conversation.id);
    assert!(conversation_mcp_apps_valid(&stored));
    assert_eq!(stored.run, RunState::idle());
    assert_eq!(stored.messages[0].tools[0].input.as_deref(), Some("Existing tool input"));
    assert_eq!(stored.messages[0].tools[0].output.as_deref(), Some("Existing tool output"));
    assert_eq!(stored.messages[0].tools[0].mcp_app.as_ref(), Some(&resource));
    assert_eq!(f.disk_json()["conversations"][0]["messages"][0]["tools"][0]["mcpApp"], serde_json::to_value(&resource).unwrap());
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.snapshot().conversations[0], stored);
    assert!(reopened.read(|s| s.active().is_none()));
}

#[tokio::test]
async fn disabling_editing_and_removing_servers_keeps_historical_html_loadable() {
    let f = Fixture::new().await;
    let configuration = r#"{"records":{"url":"https://example.com/mcp"}}"#;
    f.store.mutate(|s| s.set_mcp_servers(&f.bot, configuration, true)).await.unwrap();
    f.store.mutate(|s| s.set_mcp_server_enabled(&f.bot, "records", true, true)).await.unwrap();
    let server = f.store.read(|s| s.mcp_servers_for(&f.bot).next().unwrap().clone());
    let mut resource = app(&f.bot);
    resource.server_id = server.id;
    resource.server_updated_at = server.updated_at;
    let conversation = f.store.mutate(|s| s.add_conversation(&f.bot)).await.unwrap();
    append(&f, &conversation.id, message(resource.clone(), None)).await;

    f.store.mutate(|s| s.set_mcp_server_enabled(&f.bot, "records", false, false)).await.unwrap();
    f.reopen().await.unwrap().flush().await.unwrap();
    f.set_time("2026-01-02T00:00:00.000Z");
    f.store.mutate(|s| s.set_mcp_servers(&f.bot, &configuration.replace("example.com", "other.example.com"), true)).await.unwrap();
    assert_ne!(f.store.read(|s| s.mcp_servers_for(&f.bot).next().unwrap().updated_at.clone()), resource.server_updated_at);
    f.reopen().await.unwrap().flush().await.unwrap();
    f.store.mutate(|s| s.set_mcp_servers(&f.bot, "{}", true)).await.unwrap();
    let reopened = f.reopen().await.unwrap();
    assert!(reopened.snapshot().mcp_servers.is_empty());
    assert_eq!(reopened.snapshot().conversations[0].messages[0].tools[0].mcp_app.as_ref(), Some(&resource));
}

#[tokio::test]
async fn invalid_persisted_app_fields_and_wrong_normal_actor_are_rejected_without_replacing_state() {
    let f = Fixture::new().await;
    let conversation = f.store.mutate(|s| s.add_conversation(&f.bot)).await.unwrap();
    append(&f, &conversation.id, message(app(&f.bot), None)).await;
    let valid = f.disk_json();
    let invalid: Vec<(&str, Value)> = vec![
        ("botId", json!("invalid")),
        ("botId", json!(new_id())),
        ("serverId", json!("invalid")),
        ("serverUpdatedAt", json!("yesterday")),
        ("serverUpdatedAt", json!("2025-02-29T00:00:00.000Z")),
        ("server", json!("workspace")),
        ("server", json!("a__b")),
        ("server", json!("Records")),
        ("tool", json!("")),
        ("tool", json!("a\nb")),
        ("tool", json!("x".repeat(MAX_MCP_APP_TOOL_CHARS + 1))),
        ("uri", json!("https://example.com/app.html")),
        ("uri", json!("ui://")),
        ("uri", json!("ui://app\0")),
        ("uri", json!(format!("ui://{}", "x".repeat(MAX_MCP_APP_URI_CHARS)))),
        ("html", json!(" \n\t")),
        ("html", json!("x".repeat(MAX_MCP_APP_HTML_BYTES + 1))),
        ("html", json!("한".repeat(MAX_MCP_APP_HTML_BYTES / 3 + 1))),
        ("input", json!("[]")),
        ("input", json!("null")),
        ("input", json!("{invalid}")),
        ("input", json!(format!("{{\"x\":\"{}\"}}", "x".repeat(MAX_MCP_APP_INPUT_BYTES)))),
        ("output", json!("x".repeat(MAX_MCP_APP_OUTPUT_BYTES + 1))),
        ("output", json!("한".repeat(MAX_MCP_APP_OUTPUT_BYTES / 3 + 1))),
    ];
    for (field, value) in invalid {
        let mut broken = valid.clone();
        stored_app(&mut broken)[field] = value;
        let bytes = write_json(&f.state_file(), &broken);
        assert_load_rejected(&f, &bytes).await;
    }
    for unknown in ["extra", "permission", "requestId"] {
        let mut broken = valid.clone();
        stored_app(&mut broken)[unknown] = true.into();
        let bytes = write_json(&f.state_file(), &broken);
        assert_load_rejected(&f, &bytes).await;
    }
    write_json(&f.state_file(), &valid);
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn group_resources_bind_to_the_message_speaker_not_the_group_or_another_member() {
    let f = Fixture::new().await;
    let second = f.store.mutate(|s| s.add_bot(BotInput { name: "Second".into(), color: None, instructions: None, shape: None, pinned: None })).await.unwrap();
    let group = f
        .store
        .mutate(|s| s.add_group(GroupInput { name: "Team".into(), color: None, shape: None, member_bot_ids: vec![f.bot.clone(), second.id.clone()] }))
        .await
        .unwrap();
    let conversation = f.store.mutate(|s| s.add_group_conversation(&group.id)).await.unwrap();
    append(&f, &conversation.id, message(app(&second.id), Some(&second.id))).await;
    assert!(conversation_mcp_apps_valid(&f.conversation(&conversation.id)));
    f.reopen().await.unwrap().flush().await.unwrap();
    let valid = f.disk_json();
    for actor in [&group.id, &f.bot] {
        let mut broken = valid.clone();
        stored_app(&mut broken)["botId"] = json!(actor);
        let bytes = write_json(&f.state_file(), &broken);
        assert_load_rejected(&f, &bytes).await;
    }
    let mut missing_speaker = valid.clone();
    missing_speaker["conversations"][0]["messages"][0].as_object_mut().unwrap().remove("speakerBotId");
    let bytes = write_json(&f.state_file(), &missing_speaker);
    assert_load_rejected(&f, &bytes).await;
    write_json(&f.state_file(), &valid);
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn resource_byte_limits_and_sixteen_app_conversation_limit_include_their_boundaries() {
    let f = Fixture::new().await;
    let conversation = f.store.mutate(|s| s.add_conversation(&f.bot)).await.unwrap();
    let mut resource = app(&f.bot);
    resource.html = "한".repeat(MAX_MCP_APP_HTML_BYTES / 3) + &"x".repeat(MAX_MCP_APP_HTML_BYTES % 3);
    resource.input = format!("{{\"x\":\"{}\"}}", "x".repeat(MAX_MCP_APP_INPUT_BYTES - 8));
    resource.output = "x".repeat(MAX_MCP_APP_OUTPUT_BYTES);
    resource.tool = "x".repeat(MAX_MCP_APP_TOOL_CHARS);
    resource.uri = "ui://".to_owned() + &"x".repeat(MAX_MCP_APP_URI_CHARS - 5);
    assert_eq!(resource.html.len(), MAX_MCP_APP_HTML_BYTES);
    assert_eq!(resource.input.len(), MAX_MCP_APP_INPUT_BYTES);
    assert!(persisted_mcp_app_valid(&resource));
    let mut entry = message(resource, None);
    entry.tools.extend((1..MAX_MCP_APPS_PER_CONVERSATION).map(|_| tool(Some(app(&f.bot)))));
    append(&f, &conversation.id, entry).await;
    assert!(conversation_mcp_apps_valid(&f.conversation(&conversation.id)));
    f.reopen().await.unwrap().flush().await.unwrap();
    let valid = f.disk_json();
    let mut too_many = valid.clone();
    // The limit counts all messages, not just tools attached to one answer.
    let mut extra_message = valid["conversations"][0]["messages"][0].clone();
    extra_message["id"] = json!(new_id());
    extra_message["tools"] = json!([serde_json::to_value(tool(Some(app(&f.bot)))).unwrap()]);
    too_many["conversations"][0]["messages"].as_array_mut().unwrap().push(extra_message);
    let bytes = write_json(&f.state_file(), &too_many);
    assert_load_rejected(&f, &bytes).await;
    write_json(&f.state_file(), &valid);
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn encoded_argument_keys_and_nested_strings_redact_quotes_backslashes_and_unicode_escapes() {
    let f = Fixture::new().await;
    let quoted = r#"private-"quote-\backslash"#;
    let unicode = "한글-secret-9876";
    for (name, value) in [("QUOTED_TOKEN", quoted), ("UNICODE_TOKEN", unicode)] {
        f.store.mutate(|s| s.add_secret(SecretInput { name: name.into(), value: value.into(), bot_id: None }, true)).await.unwrap();
    }
    let conversation = f.store.mutate(|s| s.add_conversation(&f.bot)).await.unwrap();
    let mut resource = app(&f.bot);
    let mut fields = serde_json::Map::new();
    fields.insert(quoted.into(), json!({"nested": [quoted, {"value": unicode}]}));
    fields.insert("unicode".into(), json!(unicode));
    let encoded_unicode: String = unicode.chars().map(|character| format!("\\u{:04x}", character as u32)).collect();
    resource.input = Value::Object(fields).to_string().replace(unicode, &encoded_unicode);
    assert!(!resource.input.contains(quoted), "the quoted credential is JSON escaped");
    assert!(!resource.input.contains(unicode), "the Unicode credential is Unicode escaped");
    let mut events = f.store.subscribe();
    append(&f, &conversation.id, message(resource.clone(), None)).await;
    let snapshot = f.store.snapshot();
    let published = events.recv().await.unwrap();
    for public in [&snapshot, published.as_ref()] {
        let projected = &public.conversations[0].messages[0].tools[0].mcp_app.as_ref().unwrap().input;
        let arguments: Value = serde_json::from_str(projected).unwrap();
        assert_eq!(arguments, json!({"[redacted]": {"nested": ["[redacted]", {"value": "[redacted]"}]}, "unicode": "[redacted]"}));
    }
    assert_eq!(f.conversation(&conversation.id).messages[0].tools[0].mcp_app.as_ref(), Some(&resource));
    let reopened = f.reopen().await.unwrap();
    let public = reopened.snapshot();
    let arguments: Value = serde_json::from_str(&public.conversations[0].messages[0].tools[0].mcp_app.as_ref().unwrap().input).unwrap();
    assert_eq!(arguments["unicode"], "[redacted]");
}

#[tokio::test]
async fn redacted_argument_key_collisions_fail_closed_instead_of_merging_fields() {
    let f = Fixture::new().await;
    let secret = "collision-token-12345";
    f.store.mutate(|s| s.add_secret(SecretInput { name: "COLLISION_TOKEN".into(), value: secret.into(), bot_id: None }, true)).await.unwrap();
    let conversation = f.store.mutate(|s| s.add_conversation(&f.bot)).await.unwrap();
    let mut resource = app(&f.bot);
    resource.input = format!(r#"{{"nested":[{{"{secret}":"secret field","[redacted]":"different field"}}]}}"#);
    append(&f, &conversation.id, message(resource.clone(), None)).await;
    assert_eq!(f.store.snapshot().conversations[0].messages[0].tools[0].mcp_app.as_ref().unwrap().input, "{}");
    assert_eq!(f.conversation(&conversation.id).messages[0].tools[0].mcp_app.as_ref(), Some(&resource));
}

#[tokio::test]
async fn snapshots_and_published_resources_redact_every_new_string_but_keep_structural_ids() {
    let f = Fixture::new().await;
    let secret = "mcp_app_secret_87654321";
    f.store.mutate(|s| s.add_secret(SecretInput { name: "APP_TOKEN".into(), value: secret.into(), bot_id: None }, true)).await.unwrap();
    let conversation = f.store.mutate(|s| s.add_conversation(&f.bot)).await.unwrap();
    let mut resource = app(&f.bot);
    resource.server = secret.into();
    resource.tool = format!("tool_{secret}");
    resource.uri = format!("ui://records/{secret}");
    resource.html = format!("<p>{secret}</p>");
    resource.input = json!({"token": secret}).to_string();
    resource.output = format!("Result {secret}");
    let mut events = f.store.subscribe();
    append(&f, &conversation.id, message(resource.clone(), None)).await;
    let snapshot = f.store.snapshot();
    let published = events.recv().await.unwrap();
    for public in [&snapshot, published.as_ref()] {
        assert!(!serde_json::to_string(public).unwrap().contains(secret));
        let redacted = public.conversations[0].messages[0].tools[0].mcp_app.as_ref().unwrap();
        assert_eq!(redacted.bot_id, resource.bot_id);
        assert_eq!(redacted.server_id, resource.server_id);
        assert_eq!(redacted.server_updated_at, resource.server_updated_at);
        assert_eq!(redacted.server, "[redacted]");
        assert_eq!(redacted.tool, "tool_[redacted]");
        assert_eq!(redacted.uri, "ui://records/[redacted]");
        assert_eq!(redacted.html, "<p>[redacted]</p>");
        assert_eq!(redacted.input, r#"{"token":"[redacted]"}"#);
        assert_eq!(redacted.output, "Result [redacted]");
    }
    assert_eq!(f.conversation(&conversation.id).messages[0].tools[0].mcp_app.as_ref(), Some(&resource), "public redaction never mutates durable data");
    let reopened = f.reopen().await.unwrap();
    assert!(!serde_json::to_string(&reopened.snapshot()).unwrap().contains(secret));

    // The timestamp and the server ID also pass through the standard field-aware redactor.
    f.store.mutate(|s| s.add_secret(SecretInput { name: "CAPTURE_TIME".into(), value: CAPTURED_AT.into(), bot_id: None }, true)).await.unwrap();
    f.store.mutate(|s| s.add_secret(SecretInput { name: "SERVER_ID".into(), value: resource.server_id.clone(), bot_id: None }, true)).await.unwrap();
    let public = f.store.snapshot();
    let redacted = public.conversations[0].messages[0].tools[0].mcp_app.as_ref().unwrap();
    assert_eq!(redacted.server_updated_at, "[redacted]");
    assert_eq!(redacted.server_id, resource.server_id, "valid UUIDs keep the same structural-ID exception");
}
