//! Real engine plus scripted provider coverage for canvas approval, concurrency, and undo.

use grok_bot_model::{CanvasInput, CanvasSource, PermissionAction, PermissionCategory, PermissionRuleInput};

use super::*;

const ORIGINAL: &str = "머리말 😀\r\n\r\n이 문단은 아주 길게 설명한 초안입니다.\r\n\r\n끝 문단 그대로.\r\n";
const ANCHOR: &str = "이 문단은 아주 길게 설명한 초안입니다.";

fn input(revision: u64, content: &str) -> CanvasInput {
    CanvasInput { revision, title: "Canvas draft".into(), language: "markdown".into(), content: content.into() }
}

fn replace(revision: u64, old_text: &str, replacement: &str) -> Reply {
    use_tool("replace_canvas_range", json!({"revision": revision, "oldText": old_text, "replacement": replacement}))
}

pub(super) async fn canvas_edits(f: &Fixture) {
    f.configure("", false).await;
    let id = f.conversation(&f.bot_id()).await;
    let other = f.conversation(&f.bot_id()).await;
    f.provider.plan(vec![discover("workspace read_canvas"), use_tool("read_canvas", json!({})), text("No canvas yet.")]);
    f.runtime.start(&id, "Read this conversation's saved canvas.").await.unwrap();
    f.completed(&id).await;
    assert!(tool_outcomes(&f.provider.inference()).iter().any(|value| value.contains("\"canvas\":null")));
    f.store.mutate(|state| state.save_canvas(&id, input(0, "old history content must not be sent by read_canvas"), CanvasSource::User)).await.unwrap();
    f.store.mutate(|state| state.save_canvas(&id, input(1, ORIGINAL), CanvasSource::User)).await.unwrap();
    f.store.mutate(|state| state.save_canvas(&other, input(0, "OTHER_CONVERSATION_CANVAS_CANARY"), CanvasSource::User)).await.unwrap();
    // Workspace writes may be allowed by a rule; in-app canvas edits must still ask every time.
    f.store
        .mutate(|state| {
            state.add_permission_rule(
                PermissionRuleInput { category: PermissionCategory::FileWrite, action: PermissionAction::Allow, bot_id: None, server: None },
                true,
            )
        })
        .await
        .unwrap();

    f.provider.clear();
    f.provider.plan(vec![
        discover("workspace read_canvas replace_canvas_range"),
        use_tool("read_canvas", json!({})),
        replace(2, ANCHOR, "짧은 문단."),
        // Retrying an approved mutation with its old base must not create another version.
        replace(2, ANCHOR, "짧은 문단."),
        text("Shortened the requested range."),
    ]);
    f.runtime.start(&id, "이 문단을 더 짧게 고쳐 줘.").await.unwrap();
    let pending = f.pending(&id).await;
    let permission = pending.run.permission.clone().unwrap();
    assert!(permission.description.contains(ANCHOR) && permission.description.contains("짧은 문단.") && permission.description.contains("base revision 2"));
    assert_eq!(permission.options.iter().map(|option| option.kind.as_str()).collect::<Vec<_>>(), ["allow_once", "reject_once"]);
    assert_eq!(pending.canvas.as_ref().unwrap().content, ORIGINAL, "approval must precede any version change");
    let wire = serde_json::to_string(&f.provider.inference()).unwrap();
    assert!(!wire.contains("OTHER_CONVERSATION_CANVAS_CANARY"));
    assert!(!wire.contains("old history content must not be sent by read_canvas"));
    let reads: Vec<Value> = tool_outcomes(&f.provider.inference())
        .into_iter()
        .filter_map(|value| serde_json::from_str::<Value>(&value).ok())
        .filter(|value| value.get("canvas").is_some())
        .collect();
    let read = reads.last().unwrap();
    assert_eq!(read["canvas"]["revision"], 2);
    assert_eq!(read["canvas"]["history"][0]["revision"], 1);
    assert!(read["canvas"]["history"][0].get("content").is_none());
    f.decide(&id, "allow_once").await;
    let completed = f.completed(&id).await;
    let canvas = completed.canvas.unwrap();
    assert_eq!(canvas.revision, 3);
    assert_eq!(canvas.source, CanvasSource::Bot);
    assert_eq!(canvas.content, ORIGINAL.replacen(ANCHOR, "짧은 문단.", 1));
    assert_eq!(canvas.history.len(), 2);
    assert_eq!(f.get(&other).canvas.unwrap().revision, 1);
    assert!(tool_outcomes(&f.provider.inference()).iter().any(|value| value.contains("Canvas changed")));

    // User edits while the immutable approval is on screen; approving the old range cannot rebase
    // silently onto that newer revision or overwrite it, even if the anchor still exists.
    f.provider.clear();
    f.provider.plan(vec![replace(3, "짧은 문단.", "Bot stale proposal."), text("The canvas changed; no edit applied.")]);
    f.runtime.start(&id, "Edit the short paragraph again.").await.unwrap();
    f.pending(&id).await;
    let concurrent = "머리말 😀\r\n\r\n짧은 문단.\r\n\r\n사용자가 새로 고친 끝 문단.\r\n";
    f.store.mutate(|state| state.save_canvas(&id, input(3, concurrent), CanvasSource::User)).await.unwrap();
    f.decide(&id, "allow_once").await;
    let conflicted = f.completed(&id).await;
    assert_eq!(conflicted.canvas.as_ref().unwrap().revision, 4);
    assert_eq!(conflicted.canvas.as_ref().unwrap().content, concurrent);
    assert_eq!(conflicted.canvas.as_ref().unwrap().history.len(), 3);
    assert!(
        tool_outcomes(&f.provider.inference())
            .iter()
            .any(|value| value.to_ascii_lowercase().contains("canvas") && value.to_ascii_lowercase().contains("changed"))
    );

    f.provider.plan(vec![replace(4, "짧은 문단.", "Denied proposal."), text("The user denied the canvas edit.")]);
    f.runtime.start(&id, "Propose a canvas edit for denial.").await.unwrap();
    f.decide(&id, "reject_once").await;
    assert_eq!(f.completed(&id).await.canvas.unwrap().revision, 4);
    f.provider.plan(vec![replace(4, "짧은 문단.", "Cancelled proposal.")]);
    f.runtime.start(&id, "Propose a canvas edit for cancellation.").await.unwrap();
    let stale = f.pending(&id).await.run.permission.unwrap();
    f.runtime.cancel(&id).await.unwrap();
    let cancelled = f.get(&id);
    assert_eq!(cancelled.run.status, RunStatus::Cancelled);
    assert_eq!(cancelled.canvas.unwrap().revision, 4);
    assert_eq!(f.runtime.resolve(&id, &stale.id, &stale.options[0].id, false).await.unwrap_err().status, 409);

    // A saved undo is a fresh revision, not an old head that could resurrect a stale approval.
    let restored = f.store.mutate(|state| state.restore_canvas(&id, 4, 3)).await.unwrap();
    assert_eq!(restored.revision, 5);
    assert_eq!(restored.source, CanvasSource::Restore);
    assert_eq!(restored.content, ORIGINAL.replacen(ANCHOR, "짧은 문단.", 1));
    f.provider.plan(vec![use_tool("read_canvas", json!({})), text("Read restored current revision.")]);
    f.runtime.start(&id, "Read the restored canvas.").await.unwrap();
    assert_eq!(f.completed(&id).await.canvas.unwrap().revision, 5);
}

pub(super) async fn canvas_literal_data(f: &Fixture) {
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==";
    let content = format!("const icon = \"data:image/png;base64,{PNG}\";\r\nconst label = \"DaTa:text/plain;base64,Y2FudmFz\";\r\n");
    f.configure("", false).await;
    let id = f.conversation(&f.bot_id()).await;
    f.store.mutate(|state| state.save_canvas(&id, input(0, &content), CanvasSource::User)).await.unwrap();
    f.provider.clear();
    f.provider.plan(vec![discover("workspace read_canvas"), use_tool("read_canvas", json!({})), text("Read canvas code as literal text.")]);
    f.runtime.start(&id, "Read this canvas code without rendering embedded image literals.").await.unwrap();
    assert_eq!(f.completed(&id).await.canvas.unwrap().content, content);
    let requests = f.provider.inference();
    let outcomes = tool_outcomes(&requests);
    let read = outcomes
        .iter()
        .filter_map(|outcome| serde_json::from_str::<Value>(outcome).ok())
        .find(|value| value.get("canvas").is_some())
        .expect("complete canvas JSON reaches the actual engine's next model request");
    assert_eq!(read["canvas"]["content"], content);
    let literal = outcomes.iter().find(|outcome| outcome.contains("\"canvas\"")).unwrap();
    assert!(!literal.to_ascii_lowercase().contains("data:"), "MCP text must escape data URI delimiters before upstream scanning");
    assert!(literal.contains("data\\u003a") && literal.contains("DaTa\\u003a"));
    for message in requests.iter().flat_map(messages) {
        assert!(
            message["content"].as_array().is_none_or(|parts| parts.iter().all(|part| part["type"] != "image_url")),
            "canvas code must not become provider image input: {message}"
        );
    }
}

pub(super) async fn canvas_redaction(f: &Fixture) {
    const KEY: &str = "synthetic-canvas-provider-secret";
    f.configure("", false).await;
    let id = f.conversation(&f.bot_id()).await;
    f.store.mutate(|state| state.save_canvas(&id, input(0, &format!("Safe text {KEY}")), CanvasSource::User)).await.unwrap();
    // Content can predate protection of a string as a provider/vault secret.
    f.configure(KEY, false).await;
    f.provider.clear();
    f.provider.plan(vec![
        discover("workspace read_canvas replace_canvas_range"),
        use_tool("read_canvas", json!({})),
        replace(1, "Safe text", "Unsafe rewrite"),
        text("Protected content cannot be edited."),
    ]);
    f.runtime.start(&id, "Read then edit safe text in the canvas.").await.unwrap();
    let completed = f.completed(&id).await;
    assert_eq!(completed.canvas.unwrap().revision, 1);
    let wire = serde_json::to_string(&f.provider.inference()).unwrap();
    assert!(!wire.contains(KEY));
    assert!(wire.contains("[redacted]"));
    assert!(tool_outcomes(&f.provider.inference()).iter().any(|value| value.contains("protected secret text")));
    assert!(f.public().conversations.iter().find(|conversation| conversation.id == id).unwrap().canvas.as_ref().unwrap().redacted);
}
