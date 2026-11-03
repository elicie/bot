mod common;

use common::{Fixture, assert_load_rejected, write_json};
use grok_bot_model::RunState;

#[tokio::test]
async fn opt_in_defaults_false_is_always_public_and_survives_both_restart_states() {
    let fixture = Fixture::new().await;
    assert!(!fixture.store.snapshot().link_previews_enabled);
    assert_eq!(serde_json::to_value(fixture.store.snapshot()).unwrap()["linkPreviewsEnabled"], false);
    assert!(fixture.disk_json().get("linkPreviewsEnabled").is_none());
    let revision = fixture.store.snapshot().revision;
    fixture.store.mutate(|state| state.set_link_previews_enabled(false)).await.unwrap();
    assert_eq!(fixture.store.snapshot().revision, revision, "unchanged settings do not write or broadcast");
    let mut events = fixture.store.subscribe();
    fixture.store.mutate(|state| state.set_link_previews_enabled(true)).await.unwrap();
    assert!(events.recv().await.unwrap().link_previews_enabled);
    assert_eq!(fixture.disk_json()["linkPreviewsEnabled"], true);
    let restarted = fixture.reopen().await.unwrap();
    assert!(restarted.snapshot().link_previews_enabled);
    restarted.mutate(|state| state.set_link_previews_enabled(false)).await.unwrap();
    restarted.flush().await.unwrap();
    assert!(fixture.disk_json().get("linkPreviewsEnabled").is_none());
    assert!(!fixture.reopen().await.unwrap().snapshot().link_previews_enabled);
}

#[tokio::test]
async fn preview_setting_can_be_disabled_during_an_active_run() {
    let fixture = Fixture::new().await;
    fixture.store.mutate(|state| state.set_link_previews_enabled(true)).await.unwrap();
    let conversation = fixture.store.mutate(|state| state.add_conversation(&fixture.bot)).await.unwrap();
    fixture.set_run(&conversation.id, RunState { id: "active".into(), status: grok_bot_model::RunStatus::Running, ..RunState::idle() }).await;
    fixture.store.mutate(|state| state.set_link_previews_enabled(false)).await.unwrap();
    assert!(!fixture.store.snapshot().link_previews_enabled);
}

#[tokio::test]
async fn disk_setting_accepts_only_a_boolean() {
    let fixture = Fixture::new().await;
    let original = fixture.disk_json();
    for invalid in [serde_json::json!(null), serde_json::json!("true"), serde_json::json!(1)] {
        let mut disk = original.clone();
        disk["linkPreviewsEnabled"] = invalid;
        let bytes = write_json(&fixture.state_file(), &disk);
        assert_load_rejected(&fixture, &bytes).await;
    }
    write_json(&fixture.state_file(), &original);
}
