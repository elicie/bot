use grok_bot_model::{McpApp, ToolActivity};
use serde_json::json;

fn app() -> serde_json::Value {
    json!({
        "botId": "6c168cd1-662b-49aa-a302-795bdc87c833",
        "serverId": "8ee7dc6d-7730-4600-819c-dea387263495",
        "serverUpdatedAt": "2026-01-01T00:00:00.000Z",
        "server": "records",
        "tool": "lookup",
        "uri": "ui://records/app.html",
        "html": "<p>Record</p>",
        "input": "{\"query\":\"record\"}",
        "output": "Record found"
    })
}

#[test]
fn old_tools_keep_their_exact_shape_without_an_app() {
    let legacy = json!({"id": "tool", "title": "Look up record", "kind": "other", "status": "completed"});
    let tool: ToolActivity = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(tool.mcp_app, None);
    assert_eq!(serde_json::to_value(tool).unwrap(), legacy);
}

#[test]
fn apps_round_trip_as_optional_tool_activity_data() {
    let resource: McpApp = serde_json::from_value(app()).unwrap();
    assert_eq!(serde_json::to_value(resource).unwrap(), app());
    let value = json!({"id": "tool", "title": "Look up record", "kind": "other", "status": "completed", "mcpApp": app()});
    let tool: ToolActivity = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(tool.mcp_app.as_ref().unwrap().server, "records");
    assert_eq!(serde_json::to_value(tool).unwrap(), value);
}

#[test]
fn app_shape_is_strict_and_requires_every_string_field() {
    let fields: Vec<String> = app().as_object().unwrap().keys().cloned().collect();
    for field in fields {
        let mut missing = app();
        missing.as_object_mut().unwrap().remove(&field);
        assert!(serde_json::from_value::<McpApp>(missing).is_err(), "missing {field}");
        let mut wrong_type = app();
        wrong_type[&field] = json!({});
        assert!(serde_json::from_value::<McpApp>(wrong_type).is_err(), "non-string {field}");
    }
    let mut unknown = app();
    unknown["extra"] = true.into();
    assert!(serde_json::from_value::<McpApp>(unknown).is_err());
}
