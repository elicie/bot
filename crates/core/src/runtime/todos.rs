//! Turn-local projection of the pinned native todo tool's successful, authoritative full state.
//! The engine owns partial merges. Its raw output supplies IDs that ordinary ACP Plans omit.

use std::collections::{HashMap, HashSet};

use grok_bot_model::{TodoItem, TodoStatus};
use serde_json::{Value, json};

fn item(value: &Value) -> Option<TodoItem> {
    Some(TodoItem { content: value.get("content")?.as_str()?.to_owned(), status: serde_json::from_value::<TodoStatus>(value.get("status")?.clone()).ok()? })
}

fn signature(todo: &TodoItem) -> String {
    json!([todo.content, todo.status]).to_string()
}

#[derive(Default)]
pub(super) struct TodoTracker {
    calls: HashMap<String, Vec<String>>,
    active: HashSet<String>,
    pub(super) native: bool,
}

impl TodoTracker {
    pub(super) fn project(&mut self, update: &Value) -> Option<Vec<TodoItem>> {
        let call = update.get("toolCallId")?.as_str()?;
        let input = &update["rawInput"];
        if input["variant"] == "TodoWrite"
            && let Some(todos) = input["todos"].as_array()
            && let Some(ids) = todos.iter().map(|todo| todo.get("id")?.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>()
        {
            self.calls.insert(call.to_owned(), ids);
        }
        let output = &update["rawOutput"];
        if output["type"] != "Todo" {
            return None;
        }
        let success = &output["TodosUpdated"];
        let state = success["state"]["todos"].as_object()?;
        let ids = self.calls.get(call)?;
        let ordered = success["todos"].as_array()?.iter().map(item).collect::<Option<Vec<_>>>()?;
        let active = self.active.iter().chain(ids).filter(|id| state.contains_key(*id)).cloned().collect::<HashSet<_>>();
        let mut counts = HashMap::<String, usize>::new();
        for id in &active {
            *counts.entry(signature(&item(state.get(id)?)?)).or_default() += 1;
        }
        // Match by value/count to preserve the authoritative array order even for numeric IDs.
        // Identical old/current values are visually indistinguishable; keep only current counts.
        let todos = ordered
            .into_iter()
            .filter(|todo| {
                let Some(count) = counts.get_mut(&signature(todo)).filter(|count| **count > 0) else {
                    return false;
                };
                *count -= 1;
                true
            })
            .collect();
        if counts.values().any(|count| *count != 0) {
            return None;
        }
        self.active = active;
        self.native = true;
        Some(todos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todo(content: &str, status: &str) -> Value {
        json!({"content": content, "priority": "medium", "status": status})
    }

    fn update(call: &str, input: Value, state: Value, ordered: Vec<Value>) -> Value {
        json!({
            "toolCallId": call,
            "rawInput": input,
            "rawOutput": {"type": "Todo", "TodosUpdated": {"todos": ordered, "state": {"todos": state}}},
        })
    }

    fn input(ids: &[&str]) -> Value {
        json!({"variant": "TodoWrite", "todos": ids.iter().map(|id| json!({"id": id})).collect::<Vec<_>>()})
    }

    fn expected(todos: &[(&str, TodoStatus)]) -> Vec<TodoItem> {
        todos.iter().map(|(content, status)| TodoItem { content: (*content).into(), status: *status }).collect()
    }

    #[test]
    fn filters_prior_turn_ids_and_keeps_array_order_with_numeric_ids() {
        let mut tracker = TodoTracker::default();
        let old = todo("Old", "completed");
        let first = todo("First", "in_progress");
        let second = todo("Second", "pending");
        let result = tracker.project(&update(
            "call",
            input(&["10", "2"]),
            json!({
                "1": old, "2": second, "10": first,
            }),
            vec![old, first, second],
        ));
        assert_eq!(result, Some(expected(&[("First", TodoStatus::InProgress), ("Second", TodoStatus::Pending)])));
        assert!(tracker.native);
    }

    #[test]
    fn separate_input_and_success_updates_use_authoritative_partial_merge_state() {
        let mut tracker = TodoTracker::default();
        let a = todo("A", "pending");
        let b = todo("B", "pending");
        tracker.project(&update("first", input(&["a", "b"]), json!({"a": a, "b": b}), vec![a.clone(), b.clone()]));
        let partial = json!({"variant": "TodoWrite", "merge": false, "todos": [{"id": "b", "status": "completed"}]});
        assert_eq!(tracker.project(&json!({"toolCallId": "partial", "rawInput": partial})), None);
        let done = todo("B", "completed");
        let result = tracker.project(&update("partial", Value::Null, json!({"a": a, "b": done}), vec![a, done]));
        assert_eq!(result, Some(expected(&[("A", TodoStatus::Pending), ("B", TodoStatus::Completed)])));
    }

    #[test]
    fn explicitly_touching_a_prior_id_admits_its_authoritative_content() {
        let mut tracker = TodoTracker::default();
        let old = todo("Prior task intentionally resumed", "in_progress");
        let new = todo("New", "pending");
        tracker.project(&update("first", input(&["new"]), json!({"old": old, "new": new}), vec![old.clone(), new.clone()]));
        let result = tracker.project(&update("resume", input(&["old"]), json!({"old": old, "new": new}), vec![old, new]));
        assert_eq!(result, Some(expected(&[("Prior task intentionally resumed", TodoStatus::InProgress), ("New", TodoStatus::Pending)])));
    }

    #[test]
    fn failed_calls_do_not_admit_ids_or_enter_native_mode() {
        let mut tracker = TodoTracker::default();
        let failed = json!({"toolCallId": "failed", "rawInput": input(&["rejected"]), "rawOutput": {"type": "Todo", "DuplicateId": "duplicate"}});
        assert_eq!(tracker.project(&failed), None);
        assert!(!tracker.native);
        let old = todo("Rejected ID already persisted", "pending");
        let new = todo("Accepted", "pending");
        let result = tracker.project(&update("good", input(&["accepted"]), json!({"rejected": old, "accepted": new}), vec![old, new]));
        assert_eq!(result, Some(expected(&[("Accepted", TodoStatus::Pending)])));
    }

    #[test]
    fn replacements_prune_missing_ids_and_empty_success_clears_the_list() {
        let mut tracker = TodoTracker::default();
        let old = todo("Old", "pending");
        tracker.project(&update("old", input(&["old"]), json!({"old": old}), vec![old]));
        let new = todo("Replacement", "cancelled");
        let result = tracker.project(&update("new", input(&["new"]), json!({"new": new}), vec![new]));
        assert_eq!(result, Some(expected(&[("Replacement", TodoStatus::Cancelled)])));
        assert_eq!(tracker.active, HashSet::from(["new".to_owned()]));
        assert_eq!(tracker.project(&update("empty", input(&[]), json!({}), vec![])), Some(vec![]));
        assert!(tracker.native);
        assert!(tracker.active.is_empty());
    }

    #[test]
    fn duplicate_values_keep_only_the_number_of_current_run_ids() {
        let mut tracker = TodoTracker::default();
        let same = todo("Same task", "pending");
        let result = tracker.project(&update(
            "same",
            input(&["2", "10"]),
            json!({
                "1": same, "2": same, "10": same,
            }),
            vec![same.clone(), same.clone(), same],
        ));
        assert_eq!(result, Some(expected(&[("Same task", TodoStatus::Pending), ("Same task", TodoStatus::Pending)])));
    }

    #[test]
    fn malformed_or_unrelated_outputs_keep_fallback_and_previous_tracker_state() {
        let mut tracker = TodoTracker::default();
        let good = todo("Good", "pending");
        let missing = update("missing", input(&["missing"]), json!({"missing": good}), vec![]);
        assert_eq!(tracker.project(&missing), None);
        assert!(!tracker.native);
        assert!(tracker.active.is_empty());
        let unrelated = json!({"toolCallId": "unrelated", "rawOutput": {"type": "MCP", "TodosUpdated": {"todos": [], "state": {"todos": {}}}}});
        assert_eq!(tracker.project(&unrelated), None);
        assert!(!tracker.native);
        let no_input = update("unknown", Value::Null, json!({}), vec![]);
        assert_eq!(tracker.project(&no_input), None);
        let bad = update("bad", input(&["bad"]), json!({"bad": {"content": "Bad", "status": "other"}}), vec![good]);
        assert_eq!(tracker.project(&bad), None);
        assert!(!tracker.native);
    }
}
