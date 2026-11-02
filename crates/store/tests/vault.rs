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

#[tokio::test]
async fn externally_managed_tokens_redact_state_and_sse_without_persisting_the_key_registry() {
    let f = Fixture::new().await;
    let bot = f.bot_id();
    let token = "oauth-external-canary-value-42";
    f.store.set_external_redaction_keys(vec![token.into(), token.into(), String::new()]);
    assert!(!f.disk_text().contains(token), "registering a server-only token must not write it to state.json");
    assert!(f.store.read(|s| s.redaction_keys()).contains(&token.to_owned()));
    assert_eq!(f.store.read(|s| s.redact(&format!("Bearer {token}"))), "Bearer [redacted]");
    let mut events = f.store.subscribe();
    // Exercise publication with hostile text. Actual OAuth runtime results are redacted before
    // persistence; this direct mutation deliberately bypasses that runtime boundary.
    f.store.mutate(|s| s.update_bot(&bot, BotPatch { instructions: Some(format!("untrusted echo: {token}")), ..BotPatch::default() })).await.unwrap();
    assert!(!serde_json::to_string(&f.store.snapshot()).unwrap().contains(token));
    let event = events.try_recv().expect("committed public state");
    assert!(!serde_json::to_string(&*event).unwrap().contains(token));
    assert!(event.bots[0].instructions.contains("[redacted]"));
    assert!(f.store.snapshot().secrets.is_empty(), "external tokens do not become visible vault records");
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
    assert_eq!(
        public.iter().map(|s| (s.name.as_str(), s.transport, s.enabled)).collect::<Vec<_>>(),
        [("local", McpTransportKind::Stdio, false), ("web", McpTransportKind::Http, false)]
    );
    assert_eq!(public[1].secret_refs, ["API_TOKEN"]);
    let snapshot = serde_json::to_string(&f.store.snapshot()).unwrap();
    assert!(!snapshot.contains("server.js") && !snapshot.contains("example.com"), "the snapshot carries no commands or URLs");
    assert_eq!(f.store.read(|s| s.mcp_fingerprint(&bot)), None, "disabled servers leave sessions untouched");
    let canonical = f.store.read(|s| s.mcp_json(&bot)).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&canonical).unwrap()["mcpServers"]["local"],
        json!({"command": "node", "args": ["server.js"], "env": {"TOKEN": "${secret:LOCAL_TOKEN}"}})
    );

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
    assert_eq!(
        servers[0].config,
        McpServerConfig::Http {
            url: "https://example.com/mcp".into(),
            headers: vec![McpPair { name: "Authorization".into(), value: "Bearer scoped-token-value".into() }]
        }
    );
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
    for (field, value) in
        [("name", json!("workspace")), ("name", json!("a__b")), ("botId", json!(new_id())), ("config", json!({"transport": "stdio", "command": ""}))]
    {
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
async fn mcp_drafts_resolve_scopes_and_ephemeral_secrets_without_mutating_the_store() {
    let f = Fixture::new().await;
    let bot = f.bot_id();
    let other = f.store.mutate(|s| s.add_bot(BotInput { name: "Other".into(), color: None, instructions: None, shape: None, pinned: None })).await.unwrap().id;
    for (name, value, scope) in [
        ("API_TOKEN", "global-token-value", None),
        ("API_TOKEN", "target-token-value", Some(bot.as_str())),
        ("GLOBAL_TOKEN", "global-fallback-value", None),
        ("GLOBAL_TOKEN", "other-bot-value", Some(other.as_str())),
        ("DIRECT_TOKEN", "global-direct-value", None),
    ] {
        let input = secret(name, value, scope);
        f.store.mutate(move |s| s.add_secret(input, true)).await.unwrap();
    }
    let document = json!({"draft": {"url": "https://example.com/mcp", "headers": {
        "A": "Bearer ${secret:API_TOKEN}", "B": "${secret:GLOBAL_TOKEN}", "C": "${secret:DIRECT_TOKEN}", "D": "${secret:NEW_TOKEN}"
    }}})
    .to_string();
    let ephemeral = [secret(" DIRECT_TOKEN ", " direct-draft-value \n", None), secret("NEW_TOKEN", "new-draft-value", Some(&bot))];
    let disk = f.disk_text();
    let snapshot = serde_json::to_value(f.store.snapshot()).unwrap();
    let mut events = f.store.subscribe();
    let resolved = f.store.read(|s| s.resolve_mcp_draft(&bot, &document, &ephemeral, true)).unwrap();
    let McpServerConfig::Http { headers, .. } = resolved.config else { panic!("HTTP draft") };
    assert_eq!(
        headers.iter().map(|pair| pair.value.as_str()).collect::<Vec<_>>(),
        ["Bearer target-token-value", "global-fallback-value", "direct-draft-value", "new-draft-value"]
    );
    assert_eq!(f.disk_text(), disk);
    assert_eq!(serde_json::to_value(f.store.snapshot()).unwrap(), snapshot);
    assert!(events.try_recv().is_err(), "a draft emits no state event");
    assert!(f.store.read(|s| s.mcp_servers_for(&bot).next().is_none()));

    // Same validation as the vault, target scope only, no replacing existing scoped values.
    for (inputs, status) in [
        (vec![secret("lower", VALUE, None)], 400),
        (vec![secret("TOKEN", "short", None)], 400),
        (vec![secret("TOKEN", "line one\nline two", None)], 400),
        (vec![secret("TOKEN", &"x".repeat(8193), None)], 400),
        (vec![secret("TOKEN", VALUE, Some(&other))], 400),
        (vec![secret("TOKEN", VALUE, Some("not-a-bot"))], 400),
        (vec![secret(" TOKEN ", VALUE, None), secret("TOKEN", VALUE, Some(&bot))], 400),
        (vec![secret("API_TOKEN", VALUE, None)], 409),
        (vec![secret("API_TOKEN", VALUE, Some(&bot))], 409),
    ] {
        assert_eq!(f.store.read(|s| s.resolve_mcp_draft(&bot, &document, &inputs, true)).unwrap_err().status, status);
    }
    for document in [
        "{",
        "{}",
        r#"{"a":{"url":"http://x"},"b":{"url":"http://x"}}"#,
        r#"{"a":{"url":"http://x"},"a":{"url":"http://x"}}"#,
        r#"{"a":{"url":"http://x","headers":{"X":"${secret:MISSING}"}}}"#,
    ] {
        let error = f.store.read(|s| s.resolve_mcp_draft(&bot, document, &[], true)).unwrap_err();
        assert_eq!(error.status, 400);
    }
    assert_eq!(f.store.read(|s| s.resolve_mcp_draft(&bot, "{", &[], false)).unwrap_err().status, 403, "loopback before parsing");
    assert_eq!(f.store.read(|s| s.resolve_mcp_draft(&new_id(), &document, &ephemeral, true)).unwrap_err().status, 404);
    assert_eq!(f.disk_text(), disk);

    // The same idle policy as saving configuration, even though probing is read-only.
    let conversation = f.store.mutate(|s| s.add_conversation(&bot)).await.unwrap().id;
    f.start(&conversation).await;
    assert_eq!(f.store.read(|s| s.resolve_mcp_draft(&bot, &document, &ephemeral, true)).unwrap_err().status, 409);
}

#[tokio::test]
async fn form_credentials_and_servers_commit_together_or_leave_no_changes() {
    let f = Fixture::new().await;
    let bot = f.bot_id();
    let document = r#"{"draft":{"url":"https://example.com/mcp","headers":{"X":"${secret:NEW_TOKEN}"}}}"#;
    let original = f.disk_text();
    let before = serde_json::to_value(f.store.snapshot()).unwrap();
    let mut events = f.store.subscribe();
    for (json, inputs) in [
        ("{", vec![secret("NEW_TOKEN", VALUE, Some(&bot))]),
        (document, vec![secret("NEW_TOKEN", VALUE, Some(&bot)), secret("NEW_TOKEN", "duplicate-value", Some(&bot))]),
        (document, vec![secret("NEW_TOKEN", VALUE, Some(&bot)), secret("UNUSED_TOKEN", "unused-value", Some(&bot))]),
        (document, vec![secret("NEW_TOKEN", "short", Some(&bot))]),
        (document, vec![secret("NEW_TOKEN", VALUE, Some(&new_id()))]),
    ] {
        assert!(f.store.mutate(|s| s.set_mcp_servers_with_secrets(&bot, json, &inputs, true)).await.is_err());
        assert_eq!(serde_json::to_value(f.store.snapshot()).unwrap(), before);
        assert_eq!(f.disk_text(), original);
        assert!(events.try_recv().is_err(), "failed batch never commits or broadcasts partial secrets");
    }
    assert_eq!(f.store.mutate(|s| s.set_mcp_servers_with_secrets(&bot, document, &[secret("NEW_TOKEN", VALUE, None)], false)).await.unwrap_err().status, 403);
    f.store.mutate(|s| s.set_mcp_servers_with_secrets(&bot, document, &[secret("NEW_TOKEN", VALUE, None)], true)).await.unwrap();
    let state = f.store.snapshot();
    assert_eq!(state.secrets.len(), 1);
    assert_eq!(state.secrets[0].bot_id.as_deref(), Some(bot.as_str()));
    assert_eq!(state.mcp_servers.len(), 1);
    assert!(!state.mcp_servers[0].enabled);
    assert_eq!(state.mcp_servers[0].secret_refs, ["NEW_TOKEN"]);
    assert!(!serde_json::to_string(&state).unwrap().contains(VALUE));
    assert_eq!(state.revision, serde_json::from_value::<grok_bot_model::AppState>(before).unwrap().revision + 1);
    assert!(events.try_recv().is_ok());
    assert!(events.try_recv().is_err(), "batch emits one complete state, not a partial-secret event");
    f.reopen().await.unwrap();
}

#[tokio::test]
async fn probe_errors_name_the_invalid_field_without_echoing_credential_material() {
    let f = Fixture::new().await;
    let bot = f.bot_id();
    for (document, field) in [
        (json!({"workspace-canary-secret": {"url":"https://example.com"}}), "이름(name)"),
        (json!({"draft":{"command":"./credential-canary"}}), "명령(command)"),
        (json!({"draft":{"command":"node","args":[""]}}), "인자(args)"),
        (json!({"draft":{"command":"node","args":["   "]}}), "인자(args)"),
        (json!({"draft":{"command":"node","args":[false]}}), "인자(args)"),
        (json!({"draft":{"url":"https://user:credential-canary@example.com"}}), "URL(url)"),
        (json!({"draft":{"url":"https://example.com","headers":{"bad credential-canary":"secret-value"}}}), "헤더(headers)"),
        (json!({"draft":{"command":"node","env":{"TOKEN":"${secret:MISSING}"}}}), "환경 변수(env)"),
        (json!({"draft":{"url":"https://example.com","credential-canary":true}}), "JSON(json)"),
    ] {
        let error = f.store.read(|s| s.resolve_mcp_draft(&bot, &document.to_string(), &[], true)).unwrap_err();
        assert_eq!(error.status, 400);
        assert!(error.message.contains(field), "{field}: {}", error.message);
        for value in ["credential-canary", "secret-value", "MISSING", "workspace-canary-secret"] {
            assert!(!error.message.contains(value));
        }
    }
    // Advanced JSON callers retain intentional empty argv; the form/probe alone rejects blank rows.
    f.store.mutate(|s| s.set_mcp_servers(&bot, r#"{"legacy":{"command":"node","args":[""]}}"#, true)).await.unwrap();
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
    let input = |server: Option<&str>, category: PermissionCategory| PermissionRuleInput {
        category,
        action: PermissionAction::Allow,
        bot_id: Some(bot.clone()),
        server: server.map(str::to_owned),
    };
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
