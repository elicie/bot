use grok_bot_model::{RunState, TodoItem, TodoStatus};
use serde_json::json;

#[test]
fn run_todos_are_optional_for_old_state_and_reset_for_new_runs() {
    let legacy: RunState = serde_json::from_value(json!({"id": "", "status": "idle"})).unwrap();
    assert_eq!(legacy.todos, None);
    assert_eq!(legacy, RunState::idle());
    assert_eq!(serde_json::to_value(legacy).unwrap(), json!({"id": "", "status": "idle"}));
    let empty: RunState = serde_json::from_value(json!({"id": "run", "status": "completed", "todos": []})).unwrap();
    assert_eq!(empty.todos, Some(Vec::new()));
    assert_eq!(serde_json::to_value(empty).unwrap()["todos"], json!([]));
}

#[test]
fn todo_items_round_trip_the_public_content_and_status_contract() {
    for (status, name) in
        [(TodoStatus::Pending, "pending"), (TodoStatus::InProgress, "in_progress"), (TodoStatus::Completed, "completed"), (TodoStatus::Cancelled, "cancelled")]
    {
        let todo = TodoItem { content: "한글 checklist".into(), status };
        let value = json!({"content": "한글 checklist", "status": name});
        assert_eq!(serde_json::to_value(&todo).unwrap(), value);
        assert_eq!(serde_json::from_value::<TodoItem>(value).unwrap(), todo);
    }
    for invalid in [json!({"content": "Task", "status": "unknown"}), json!({"content": "Task"}), json!({"content": "Task", "status": "pending", "extra": true})]
    {
        assert!(serde_json::from_value::<TodoItem>(invalid).is_err());
    }
}
