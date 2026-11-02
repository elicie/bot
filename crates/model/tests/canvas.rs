use grok_bot_model::{Canvas, CanvasInput, CanvasSource, CanvasVersion, Conversation};
use serde_json::json;

#[test]
fn legacy_conversations_load_without_a_canvas_and_new_fields_round_trip() {
    let value = json!({
        "id": "conversation", "botId": "bot", "title": "Chat", "messages": [],
        "run": {"id": "", "status": "idle"}, "updatedAt": "2026-01-01T00:00:00.000Z"
    });
    let mut conversation: Conversation = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(conversation.canvas, None);
    assert_eq!(serde_json::to_value(&conversation).unwrap(), value);
    let canvas = Canvas {
        revision: 2,
        title: "Plan".into(),
        language: "markdown".into(),
        content: "# New\r\n🙂".into(),
        updated_at: "2026-01-01T00:01:00.000Z".into(),
        source: CanvasSource::Bot,
        history: vec![CanvasVersion {
            revision: 1,
            title: "Plan".into(),
            language: "markdown".into(),
            content: "# Original\r\n🙂".into(),
            updated_at: "2026-01-01T00:00:00.000Z".into(),
            source: CanvasSource::User,
            redacted: false,
        }],
        redacted: false,
    };
    conversation.canvas = Some(canvas.clone());
    let encoded = serde_json::to_value(&conversation).unwrap();
    assert_eq!(encoded["canvas"]["source"], "bot");
    assert!(encoded["canvas"].get("redacted").is_none());
    assert!(encoded["canvas"]["history"][0].get("redacted").is_none());
    assert_eq!(serde_json::from_value::<Conversation>(encoded).unwrap().canvas, Some(canvas));
}

#[test]
fn canvas_sources_and_public_read_only_markers_are_strict() {
    let value = json!({
        "revision": 1, "title": "Plan", "language": "text", "content": "[redacted]",
        "updatedAt": "2026-01-01T00:00:00.000Z", "source": "restore", "redacted": true
    });
    let version: CanvasVersion = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(version.source, CanvasSource::Restore);
    assert!(version.redacted);
    assert_eq!(serde_json::to_value(version).unwrap(), value);
    for invalid in [
        json!({"revision": 0, "title": "Plan", "language": "text", "content": "x", "redacted": true}),
        json!({"revision": 0, "title": "Plan", "language": "text", "content": "x", "history": []}),
        json!({"revision": -1, "title": "Plan", "language": "text", "content": "x"}),
        json!({"revision": 1.5, "title": "Plan", "language": "text", "content": "x"}),
    ] {
        assert!(serde_json::from_value::<CanvasInput>(invalid).is_err());
    }
    let mut invalid = value;
    invalid["source"] = json!("system");
    assert!(serde_json::from_value::<CanvasVersion>(invalid).is_err());
}
