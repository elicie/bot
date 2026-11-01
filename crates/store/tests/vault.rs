//! Vault secrets and custom MCP servers at the store level: validation, loopback-only changes,
//! write-only values, snapshot redaction (HTTP snapshot and SSE broadcast), enabled-state rules,
//! secret resolution, fingerprints, load validation, and bot deletion cleanup.

mod common;

use common::{Fixture, assert_load_rejected, write_json};
use grok_bot_model::*;
use grok_bot_store::vault::{MCP_REQUIRES_LOOPBACK, VAULT_REQUIRES_LOOPBACK};
use serde_json::json;

const VALUE: &str = "vault-canary-0123456789";

fn secret(name: &str, value: &str, bot_id: Option<&str>) -> SecretInput {
    SecretInput { name: name.into(), value: value.into(), bot_id: bot_id.map(str::to_owned) }
}

#[tokio::test]
async fn secrets_are_write_only_loopback_only_and_redacted_everywhere() {
    let f = Fixture::new().await;
    let bot = f.bot_id();
    let invalid = |input: SecretInput| {
        let store = f.store.clone();
        async move { store.mutate(move |s| s.add_secret(input, true)).await.unwrap_err() }
    };
    assert!(invalid(secret("api_token", VALUE, None)).await.message.contains("대문자"));
    assert!(invalid(secret("", VALUE, None)).await.message.contains("이름을 입력"));
    assert!(invalid(secret("TOKEN", "short", None)).await.message.contains("8자 이상"));
    assert!(invalid(secret("TOKEN", "line one\nline two", None)).await.message.contains("제어 문자"));
    assert_eq!(invalid(secret("TOKEN", VALUE, Some(&new_id()))).await.status, 404);

    // LAN clients cannot add, replace, or delete.
    let lan = f.store.mutate(|s| s.add_secret(secret("API_TOKEN", VALUE, None), false)).await.unwrap_err();
    assert_eq!((lan.status, lan.message.as_str()), (403, VAULT_REQUIRES_LOOPBACK));

    let mut events = f.store.subscribe();
    let global = f.store.mutate(|s| s.add_secret(secret("API_TOKEN", &format!("  {VALUE} \n"), None), true)).await.unwrap();
    let scoped = f.store.mutate(|s| s.add_secret(secret("API_TOKEN", "bot-only-secret-value", Some(&bot)), true)).await.unwrap();
    assert_eq!(f.store.mutate(|s| s.add_secret(secret("API_TOKEN", VALUE, None), true)).await.unwrap_err().status, 409);
    assert_eq!(f.store.read(|s| s.secrets()[0].value.clone()), VALUE, "values are trimmed");

    // A secret typed into a bot's instructions is redacted in every snapshot.
    let patch = BotPatch { instructions: Some(format!("Use {VALUE} and bot-only-secret-value")), ..BotPatch::default() };
    f.store.mutate(|s| s.update_bot(&bot, patch)).await.unwrap();
    let snapshot = serde_json::to_string(&f.store.snapshot()).unwrap();
    assert!(!snapshot.contains(VALUE) && !snapshot.contains("bot-only-secret-value"), "{snapshot}");
    assert!(snapshot.contains("Use [redacted] and [redacted]"));
    let public = f.store.snapshot();
    assert_eq!(public.secrets.iter().map(|s| (s.name.as_str(), s.bot_id.clone())).collect::<Vec<_>>(), [("API_TOKEN", None), ("API_TOKEN", Some(bot.clone()))]);
    let mut last = None;
    while let Ok(event) = events.try_recv() {
        last = Some(event);
    }
    let broadcast = serde_json::to_string(&*last.expect("a broadcast snapshot")).unwrap();
    assert!(!broadcast.contains(VALUE) && !broadcast.contains("bot-only-secret-value"));
    assert!(f.disk_text().contains(VALUE), "the value is persisted in the private state file");

    let lan = f.store.mutate(|s| s.replace_secret(&global.id, "replacement-value-1", false)).await.unwrap_err();
    assert_eq!(lan.status, 403);
    assert_eq!(f.store.mutate(|s| s.delete_secret(&scoped.id, false)).await.unwrap_err().status, 403);
    f.store.mutate(|s| s.replace_secret(&global.id, "replacement-value-1", true)).await.unwrap();
    assert_eq!(f.store.read(|s| s.secrets()[0].value.clone()), "replacement-value-1");
    f.store.mutate(|s| s.delete_secret(&scoped.id, true)).await.unwrap();
    assert_eq!(f.store.mutate(|s| s.delete_secret(&scoped.id, true)).await.unwrap_err().status, 404);
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.read(|s| s.secrets().len()), 1);
    assert!(!serde_json::to_string(&reopened.snapshot()).unwrap().contains("replacement-value-1"));
}

const DOCUMENT: &str = r#"{"mcpServers": {
  "web": {"url": "https://example.com/mcp", "headers": {"Authorization": "Bearer ${secret:API_TOKEN}"}},
  "local": {"command": "node", "args": ["server.js"], "env": {"TOKEN": "${secret:LOCAL_TOKEN}"}}
}}"#;

#[tokio::test]
async fn mcp_servers_are_loopback_only_disabled_by_default_and_resolve_secrets_at_start() {
    let f = Fixture::new().await;
    let bot = f.bot_id();
    let set = |json: &str, local: bool| {
        let (store, bot, json) = (f.store.clone(), bot.clone(), json.to_owned());
        async move { store.mutate(move |s| s.set_mcp_servers(&bot, &json, local)).await }
    };
    let lan = set(DOCUMENT, false).await.unwrap_err();
    assert_eq!((lan.status, lan.message.as_str()), (403, MCP_REQUIRES_LOOPBACK));
    assert_eq!(set("{\"x\": {}}", true).await.unwrap_err().status, 400);
    set(DOCUMENT, true).await.unwrap();
    let public = f.store.snapshot().mcp_servers;
    assert_eq!(public.iter().map(|s| (s.name.as_str(), s.transport, s.enabled)).collect::<Vec<_>>(), [("local", McpTransportKind::Stdio, false), ("web", McpTransportKind::Http, false)]);
    assert_eq!(public[1].secret_refs, ["API_TOKEN"]);
    let snapshot = serde_json::to_string(&f.store.snapshot()).unwrap();
    assert!(!snapshot.contains("server.js") && !snapshot.contains("example.com"), "the snapshot carries no commands or URLs");
    assert_eq!(f.store.read(|s| s.mcp_fingerprint(&bot)), None, "disabled servers leave sessions untouched");
    let canonical = f.store.read(|s| s.mcp_json(&bot)).unwrap();
    assert_eq!(serde_json::from_str::<serde_json::Value>(&canonical).unwrap()["mcpServers"]["local"], json!({"command": "node", "args": ["server.js"], "env": {"TOKEN": "${secret:LOCAL_TOKEN}"}}));

    // Enabling needs loopback; disabling does not.
    let enable = |name: &'static str, enabled: bool, local: bool| {
        let (store, bot) = (f.store.clone(), bot.clone());
        async move { store.mutate(move |s| s.set_mcp_server_enabled(&bot, name, enabled, local)).await }
    };
    assert_eq!(enable("web", true, false).await.unwrap_err().status, 403);
    enable("web", true, true).await.unwrap();
    enable("local", true, true).await.unwrap();
    assert_eq!(enable("missing", true, true).await.unwrap_err().status, 404);
    let fingerprint = f.store.read(|s| s.mcp_fingerprint(&bot)).unwrap();

    // Missing secrets skip the server with a reason; present ones are substituted.
    let (servers, failures) = f.store.read(|s| s.resolve_mcp_servers(&bot));
    assert!(servers.is_empty());
    assert_eq!(failures.iter().map(|failure| failure.name.as_str()).collect::<Vec<_>>(), ["local", "web"]);
    assert!(failures[1].message.contains("API_TOKEN"));
    f.store.mutate(|s| s.add_secret(secret("API_TOKEN", "global-token-value", None), true)).await.unwrap();
    let scoped = bot.clone();
    f.store.mutate(move |s| s.add_secret(secret("API_TOKEN", "scoped-token-value", Some(&scoped)), true)).await.unwrap();
    let (servers, failures) = f.store.read(|s| s.resolve_mcp_servers(&bot));
    assert_eq!(failures.len(), 1);
    assert_eq!(servers[0].config, McpServerConfig::Http { url: "https://example.com/mcp".into(), headers: vec![McpPair { name: "Authorization".into(), value: "Bearer scoped-token-value".into() }] });
    assert!(!format!("{servers:?}").contains("scoped-token-value"), "debug output hides resolved values");
    assert_eq!(f.store.read(|s| s.mcp_fingerprint(&bot)).unwrap(), fingerprint, "secret values never enter the fingerprint");

    // Saving the same document keeps enabled state; changing a server disables only that one.
    set(DOCUMENT, true).await.unwrap();
    assert!(f.store.snapshot().mcp_servers.iter().all(|s| s.enabled));
    set(&DOCUMENT.replace("server.js", "other.js"), true).await.unwrap();
    let public = f.store.snapshot().mcp_servers;
    assert_eq!(public.iter().map(|s| (s.name.as_str(), s.enabled)).collect::<Vec<_>>(), [("local", false), ("web", true)]);
    assert_ne!(f.store.read(|s| s.mcp_fingerprint(&bot)).unwrap(), fingerprint);
    enable("web", false, false).await.unwrap();
    assert_eq!(f.store.read(|s| s.mcp_fingerprint(&bot)), None);

    // Persisted and reloaded; invalid persisted servers are rejected.
    let reopened = f.reopen().await.unwrap();
    assert_eq!(reopened.read(|s| s.mcp_servers_for(&bot).count()), 2);
    drop(reopened);
    let valid = f.disk_json();
    for (field, value) in [("name", json!("workspace")), ("name", json!("a__b")), ("botId", json!(new_id())), ("config", json!({"transport": "stdio", "command": ""}))] {
        let mut broken = valid.clone();
        broken["mcpServers"][0][field] = value;
        let bytes = write_json(&f.state_file(), &broken);
        assert_load_rejected(&f, &bytes).await;
    }
    let mut short = valid.clone();
    short["secrets"][0]["value"] = json!("short");
    let bytes = write_json(&f.state_file(), &short);
    assert_load_rejected(&f, &bytes).await;
    write_json(&f.state_file(), &valid);
}

#[tokio::test]
async fn bot_deletion_removes_its_secrets_and_servers() {
    let f = Fixture::new().await;
    let other = f.store.mutate(|s| s.add_bot(BotInput { name: "Other".into(), color: None, instructions: None, shape: None, pinned: None })).await.unwrap().id;
    let id = other.clone();
    f.store.mutate(move |s| s.set_mcp_servers(&id, DOCUMENT, true)).await.unwrap();
    let id = other.clone();
    f.store.mutate(move |s| s.add_secret(secret("LOCAL_TOKEN", VALUE, Some(&id)), true)).await.unwrap();
    f.store.mutate(|s| s.add_secret(secret("SHARED_TOKEN", VALUE, None), true)).await.unwrap();
    let id = other.clone();
    f.store.mutate(move |s| s.delete_bot(&id)).await.unwrap();
    let state = f.store.snapshot();
    assert!(state.mcp_servers.is_empty());
    assert_eq!(state.secrets.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["SHARED_TOKEN"]);
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn mcp_tool_rules_are_server_scoped_and_loopback_only_to_allow() {
    let f = Fixture::new().await;
    let bot = f.bot_id();
    let input = |server: Option<&str>, category: PermissionCategory| PermissionRuleInput { category, action: PermissionAction::Allow, bot_id: Some(bot.clone()), server: server.map(str::to_owned) };
    let wrong = input(Some("web"), PermissionCategory::FileWrite);
    assert_eq!(f.store.mutate(move |s| s.add_permission_rule(wrong, true)).await.unwrap_err().status, 400);
    let lan = input(Some("web"), PermissionCategory::McpTool);
    assert_eq!(f.store.mutate(move |s| s.add_permission_rule(lan, false)).await.unwrap_err().status, 403);
    let web = input(Some("web"), PermissionCategory::McpTool);
    let rule = f.store.mutate(move |s| s.add_permission_rule(web, true)).await.unwrap();
    let all = input(None, PermissionCategory::McpTool);
    f.store.mutate(move |s| s.add_permission_rule(all, true)).await.unwrap();
    let again = input(Some("web"), PermissionCategory::McpTool);
    assert_eq!(f.store.mutate(move |s| s.add_permission_rule(again, true)).await.unwrap_err().status, 409);
    assert_eq!(f.store.read(|s| s.mcp_tool_policy(&bot, "web")), grok_bot_store::permissions::PermissionPolicy::Allow { rule_id: rule.id });
    assert!(f.disk_text().contains("\"server\":\"web\""));
    f.reopen().await.unwrap();
}
