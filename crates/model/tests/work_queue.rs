use grok_bot_model::{PrivateSettings, QueuedWork, RunState, WorkKind};
use serde_json::json;

#[test]
fn work_items_have_a_flat_tag_and_camel_case_receipt_fields() {
    let base = QueuedWork {
        id: "queue-id".into(),
        bot_id: "bot-id".into(),
        speaker_bot_ids: None,
        conversation_id: "conversation-id".into(),
        created_at: "2026-01-01T00:00:00.000Z".into(),
        kind: WorkKind::User { text: "Pending request".into(), attachments: Vec::new() },
    };
    for kind in [
        base.kind.clone(),
        WorkKind::Routine { run_id: "routine-run-id".into() },
        WorkKind::Delivery { delivery_id: "delivery-id".into() },
        WorkKind::Group { round_id: "round-id".into() },
        WorkKind::Report { delivery_id: "reported-delivery-id".into() },
    ] {
        let work = QueuedWork { kind, ..base.clone() };
        let value = serde_json::to_value(&work).unwrap();
        assert!(value["kind"].is_string());
        assert_eq!(value["botId"], "bot-id");
        assert_eq!(value["conversationId"], "conversation-id");
        assert_eq!(value["createdAt"], "2026-01-01T00:00:00.000Z");
        assert_eq!(serde_json::from_value::<QueuedWork>(value.clone()).unwrap(), work);
        let mut extra = value;
        extra["unknown"] = json!(true);
        assert!(serde_json::from_value::<QueuedWork>(extra).is_err());
    }
    let mut user = serde_json::to_value(&base).unwrap();
    user.as_object_mut().unwrap().remove("attachments");
    assert_eq!(serde_json::from_value::<QueuedWork>(user).unwrap(), base);
}

#[test]
fn legacy_settings_and_run_owners_keep_their_omitted_shape() {
    let settings = PrivateSettings::default();
    assert_eq!(settings.max_concurrent_runs(), 3);
    assert!(serde_json::to_value(&settings).unwrap().get("maxConcurrentRuns").is_none());
    assert!(serde_json::to_value(RunState::idle()).unwrap().get("botId").is_none());
    let configured = PrivateSettings { max_concurrent_runs: Some(16), ..settings };
    assert_eq!(configured.max_concurrent_runs(), 16);
    assert_eq!(serde_json::to_value(&configured).unwrap()["maxConcurrentRuns"], 16);
}
