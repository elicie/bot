//! Hide local-computer approval and tool details from non-loopback clients. Port of
//! `server/public-state.ts`.

use grok_bot_model::{AppState, PendingPermission, ToolActivity};
use serde_json::{Map, Value};

const LOCAL_TOOL_TITLE: &str = "로컬 브라우저 작업";
const LOCAL_PERMISSION_TITLE: &str = "로컬 컴퓨터 승인이 필요합니다";
const LOCAL_PERMISSION_DESCRIPTION: &str = "서버 컴퓨터의 localhost 연결에서 요청을 확인하고 승인하세요.";

/// `projectComputerPermissions`: for a non-local client, replace every `localOnly` tool with a
/// placeholder (no input, output or paths) and every `localOnly` pending permission with a notice
/// that it must be approved from localhost.
pub fn project_computer_permissions(mut state: AppState, local: bool) -> AppState {
    if local {
        return state;
    }
    for conversation in &mut state.conversations {
        for message in &mut conversation.messages {
            for tool in &mut message.tools {
                if tool.local_only == Some(true) {
                    *tool = ToolActivity {
                        id: std::mem::take(&mut tool.id),
                        title: LOCAL_TOOL_TITLE.into(),
                        kind: std::mem::take(&mut tool.kind),
                        status: tool.status,
                        input: None,
                        output: None,
                        paths: None,
                        local_only: Some(true),
                        approved_by_rule: None,
                    };
                }
            }
        }
        if let Some(permission) = conversation.run.permission.as_mut().filter(|permission| permission.local_only == Some(true)) {
            *permission = PendingPermission {
                id: std::mem::take(&mut permission.id),
                title: LOCAL_PERMISSION_TITLE.into(),
                kind: "other".into(),
                description: LOCAL_PERMISSION_DESCRIPTION.into(),
                options: Vec::new(),
                paths: Vec::new(),
                local_only: Some(true),
            };
        }
    }
    state
}

/// `projectResponse`: apply [`project_computer_permissions`] to a JSON response body that is an
/// `AppState` (has a `conversations` array) or wraps one in `state`. Other bodies pass through.
pub fn project_response(mut body: Value, local: bool) -> Value {
    if local {
        return body;
    }
    let Some(object) = body.as_object_mut() else { return body };
    if object.get("conversations").is_some_and(Value::is_array) {
        project_value(object);
    } else if let Some(Value::Object(state)) = object.get_mut("state")
        && state.contains_key("conversations")
    {
        project_value(state);
    }
    body
}

/// JavaScript truthiness of an optional JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// Copy the listed fields of `source` (when present) into a new object, in order.
fn pick(source: &Map<String, Value>, fields: &[&str]) -> Map<String, Value> {
    fields.iter().filter_map(|field| source.get(*field).map(|value| ((*field).to_owned(), value.clone()))).collect()
}

fn project_value(state: &mut Map<String, Value>) {
    let Some(Value::Array(conversations)) = state.get_mut("conversations") else { return };
    for conversation in conversations.iter_mut().filter_map(Value::as_object_mut) {
        if let Some(Value::Array(messages)) = conversation.get_mut("messages") {
            for message in messages.iter_mut().filter_map(Value::as_object_mut) {
                let Some(Value::Array(tools)) = message.get_mut("tools") else { continue };
                for tool in tools.iter_mut() {
                    let Value::Object(fields) = tool else { continue };
                    if truthy(fields.get("localOnly")) {
                        let mut placeholder = pick(fields, &["id"]);
                        placeholder.insert("title".into(), LOCAL_TOOL_TITLE.into());
                        placeholder.extend(pick(fields, &["kind", "status"]));
                        placeholder.insert("localOnly".into(), true.into());
                        *fields = placeholder;
                    }
                }
            }
        }
        let Some(Value::Object(run)) = conversation.get_mut("run") else { continue };
        let Some(Value::Object(permission)) = run.get_mut("permission") else { continue };
        if truthy(permission.get("localOnly")) {
            let mut notice = pick(permission, &["id"]);
            notice.insert("title".into(), LOCAL_PERMISSION_TITLE.into());
            notice.insert("kind".into(), "other".into());
            notice.insert("description".into(), LOCAL_PERMISSION_DESCRIPTION.into());
            notice.insert("paths".into(), Value::Array(Vec::new()));
            notice.insert("options".into(), Value::Array(Vec::new()));
            notice.insert("localOnly".into(), true.into());
            *permission = notice;
        }
    }
}

#[cfg(test)]
mod tests {
    use grok_bot_model::{Conversation, Message, PermissionOption, PublicSettings, Role, RunState, RunStatus, RuntimeInfo, ToolStatus};
    use serde_json::json;

    use super::*;

    const CANARY: &str = "LOCAL_COMPUTER_QUERY_AND_ACTION_SECRET";

    const FILE_DETAIL: &str = "notes.txt visible to LAN clients";

    fn tool(id: &str, local_only: Option<bool>) -> ToolActivity {
        let detail = if local_only == Some(true) { CANARY } else { FILE_DETAIL };
        ToolActivity {
            id: id.into(),
            title: format!("workspace__computer_act {detail}"),
            kind: "other".into(),
            status: ToolStatus::Pending,
            input: Some(detail.into()),
            output: Some(detail.into()),
            paths: Some(vec![detail.into()]),
            local_only,
            approved_by_rule: None,
        }
    }

    fn permission(local_only: Option<bool>) -> PendingPermission {
        PendingPermission {
            id: "permission-1".into(),
            title: "Observe managed browser".into(),
            kind: "edit".into(),
            description: format!("https://public.example/?token={CANARY}#{CANARY}"),
            options: vec![PermissionOption { id: "once".into(), label: "Allow once".into(), kind: "allow_once".into() }],
            paths: vec![CANARY.into()],
            local_only,
        }
    }

    fn state(permission: Option<PendingPermission>) -> AppState {
        let conversation = Conversation {
            id: "conversation-1".into(),
            bot_id: "bot-1".into(),
            title: "Chat".into(),
            messages: vec![Message {
                id: "message-1".into(),
                role: Role::Assistant,
                text: String::new(),
                created_at: "2026-11-01T00:00:00.000Z".into(),
                tools: vec![tool("local-tool", Some(true)), tool("file-tool", None), tool("explicit-false", Some(false))],
                bot_exchange_ids: None,
                source_bot_id: None,
                attachments: None,
                speaker_bot_id: None,
                report_delivery_ids: None,
            }],
            session_id: None,
            session_fingerprint: None,
            run: RunState {
                id: "run-1".into(),
                status: RunStatus::AwaitingPermission,
                error: None,
                permission,
                todos: None,
                started_at: Some("2026-11-01T00:00:00.000Z".into()),
                finished_at: None,
            },
            updated_at: "2026-11-01T00:00:00.000Z".into(),
            group_id: None,
            thread: None,
            group_round: None,
            member_sessions: None,
            canvas: None,
        };
        AppState {
            revision: 3,
            bots: Vec::new(),
            conversations: vec![conversation],
            routines: Vec::new(),
            routine_runs: Vec::new(),
            bot_exchanges: Vec::new(),
            bot_deliveries: Vec::new(),
            permission_rules: Vec::new(),
            groups: Vec::new(),
            routine_scheduling_error: None,
            settings: PublicSettings { base_url: String::new(), model: String::new(), has_api_key: false, workspace: String::new() },
            runtime: RuntimeInfo { available: true, version: None, error: None },
            secrets: Vec::new(),
            mcp_servers: Vec::new(),
            main_bot_id: None,
        }
    }

    fn expected_tool() -> Value {
        json!({ "id": "local-tool", "title": "로컬 브라우저 작업", "kind": "other", "status": "pending", "localOnly": true })
    }

    fn expected_permission() -> Value {
        json!({
            "id": "permission-1", "title": "로컬 컴퓨터 승인이 필요합니다", "kind": "other",
            "description": "서버 컴퓨터의 localhost 연결에서 요청을 확인하고 승인하세요.",
            "paths": [], "options": [], "localOnly": true,
        })
    }

    #[test]
    fn local_clients_see_everything() {
        let original = state(Some(permission(Some(true))));
        assert_eq!(project_computer_permissions(original.clone(), true), original);
        let body = serde_json::to_value(&original).unwrap();
        assert_eq!(project_response(body.clone(), true), body);
    }

    #[test]
    fn remote_clients_get_placeholders_for_local_only_tools_and_permissions() {
        let projected = project_computer_permissions(state(Some(permission(Some(true)))), false);
        let text = serde_json::to_string(&projected).unwrap();
        assert!(!text.contains(CANARY));
        assert!(text.contains("localhost"));
        let value = serde_json::to_value(&projected).unwrap();
        let conversation = &value["conversations"][0];
        assert_eq!(conversation["messages"][0]["tools"][0], expected_tool());
        assert_eq!(conversation["run"]["permission"], expected_permission());
        assert_eq!(conversation["run"]["status"], "awaiting_permission");
        assert_eq!(conversation["run"]["startedAt"], "2026-11-01T00:00:00.000Z");
    }

    #[test]
    fn file_approvals_and_ordinary_tools_stay_visible_remotely() {
        let original = state(Some(permission(Some(false))));
        let projected = project_computer_permissions(original.clone(), false);
        assert_eq!(projected.conversations[0].run.permission, original.conversations[0].run.permission);
        assert_eq!(projected.conversations[0].messages[0].tools[1..], original.conversations[0].messages[0].tools[1..]);
        let text = serde_json::to_string(&projected).unwrap();
        assert!(text.contains(CANARY));
        assert_eq!(text.matches(FILE_DETAIL).count(), 8);
        let without = project_computer_permissions(state(None), false);
        assert_eq!(without.conversations[0].run.permission, None);
    }

    #[test]
    fn responses_are_projected_when_they_are_or_contain_a_state() {
        let original = state(Some(permission(Some(true))));
        let expected = serde_json::to_value(project_computer_permissions(original.clone(), false)).unwrap();
        let body = serde_json::to_value(&original).unwrap();
        assert_eq!(project_response(body.clone(), false), expected);

        let wrapped = json!({ "bot": { "id": "bot-1" }, "state": body });
        let projected = project_response(wrapped, false);
        assert_eq!(projected["state"], expected);
        assert_eq!(projected["bot"], json!({ "id": "bot-1" }));
        assert!(!projected.to_string().contains(CANARY));
    }

    #[test]
    fn other_responses_pass_through() {
        for body in [
            Value::Null,
            json!("text"),
            json!(1),
            json!([{ "conversations": [] }]),
            json!({ "conversations": { "localOnly": true } }),
            json!({ "state": null }),
            json!({ "state": { "other": CANARY } }),
            json!({ "ok": true, "models": [CANARY] }),
        ] {
            assert_eq!(project_response(body.clone(), false), body);
        }
    }

    #[test]
    fn json_projection_uses_javascript_truthiness() {
        let body = json!({ "conversations": [{
            "messages": [{ "tools": [
                { "id": "a", "title": CANARY, "kind": "other", "status": "completed", "input": CANARY, "localOnly": 1 },
                { "id": "b", "title": "kept", "kind": "other", "status": "completed", "localOnly": 0 },
            ] }],
            "run": { "id": "r", "status": "running", "permission": { "id": "p", "description": CANARY, "localOnly": "yes" } },
        }] });
        let projected = project_response(body, false);
        let conversation = &projected["conversations"][0];
        assert_eq!(
            conversation["messages"][0]["tools"][0],
            json!({ "id": "a", "title": "로컬 브라우저 작업", "kind": "other", "status": "completed", "localOnly": true })
        );
        assert_eq!(conversation["messages"][0]["tools"][1]["title"], "kept");
        assert_eq!(conversation["run"]["permission"]["title"], "로컬 컴퓨터 승인이 필요합니다");
        assert!(!projected.to_string().contains(CANARY));
    }
}
