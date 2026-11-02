//! Broker tools that let a bot manage its own routines from chat ("루틴은 이 Bot이 일정에 따라
//! 실행하는 반복 작업입니다. 채팅으로 설정해 달라고 요청해 주세요."). Listing is a safe read; every
//! create, update, pause/resume, or delete waits for a one-use browser approval ("… 루틴을
//! 저장하도록 허용할까요?") and then goes through the same store validation as the routine editor.

use std::sync::Arc;

use grok_bot_model::{HttpError, PendingPermission, Routine, RoutineInput, RoutineSchedule, ToolActivity, ToolStatus, iso, new_id};
use serde_json::{Map, Value, json};

use super::{BrokerError, BrokerResult, approval_options, is_uuid, required, strict};
use crate::runtime::text::truncate_utf16;
use crate::runtime::{Run, Runtime};

/// Routine changes one model turn may make.
const MAX_ROUTINE_CHANGES_PER_RUN: usize = 3;

pub(super) const NAMES: [&str; 4] = ["list_routines", "save_routine", "set_routine_enabled", "delete_routine"];

pub(super) fn tools() -> Vec<Value> {
    let schedule = json!({ "oneOf": [
        { "type": "object", "properties": { "kind": { "const": "once" }, "at": { "type": "string", "description": "ISO 8601 time with offset, in the future" } }, "required": ["kind", "at"], "additionalProperties": false },
        { "type": "object", "properties": { "kind": { "const": "interval" }, "minutes": { "type": "integer", "minimum": 1, "maximum": 10080 } }, "required": ["kind", "minutes"], "additionalProperties": false },
        { "type": "object", "properties": { "kind": { "const": "daily" }, "time": { "type": "string", "pattern": "^(?:[01]\\d|2[0-3]):[0-5]\\d$" }, "timezone": { "type": "string", "description": "IANA timezone, e.g. Asia/Seoul" } }, "required": ["kind", "time", "timezone"], "additionalProperties": false }
    ] });
    vec![
        json!({ "name": "list_routines", "description": "List this bot's routines (recurring tasks it runs on a schedule) with IDs, names, prompts, schedules, enabled state, and next run times, plus the current server time. Use it before changing a routine.", "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false } }),
        json!({ "name": "save_routine", "description": "Create a routine for this bot, or update one of its routines when id is given, when the user asks you to set up a recurring or scheduled task. prompt is the instruction the bot runs each time. Every save requires explicit one-use browser approval. At most three routine changes per turn.", "inputSchema": { "type": "object", "properties": { "id": { "type": "string", "format": "uuid" }, "name": { "type": "string", "minLength": 1, "maxLength": 100 }, "prompt": { "type": "string", "minLength": 1, "maxLength": 100000 }, "schedule": schedule, "enabled": { "type": "boolean" } }, "required": ["name", "prompt", "schedule"], "additionalProperties": false } }),
        json!({ "name": "set_routine_enabled", "description": "Pause (enabled: false) or resume (enabled: true) one of this bot's routines. Requires explicit one-use browser approval.", "inputSchema": { "type": "object", "properties": { "id": { "type": "string", "format": "uuid" }, "enabled": { "type": "boolean" } }, "required": ["id", "enabled"], "additionalProperties": false } }),
        json!({ "name": "delete_routine", "description": "Delete one of this bot's routines and stop its future runs. Past run history and conversations are kept. Requires explicit one-use browser approval.", "inputSchema": { "type": "object", "properties": { "id": { "type": "string", "format": "uuid" } }, "required": ["id"], "additionalProperties": false } }),
    ]
}

pub(super) enum Call {
    List,
    Save { id: Option<String>, name: String, prompt: String, schedule: RoutineSchedule, enabled: bool },
    SetEnabled { id: String, enabled: bool },
    Delete { id: String },
}

fn uuid(arguments: &Map<String, Value>) -> BrokerResult<String> {
    let id = required(arguments, "id")?;
    if is_uuid(&id) { Ok(id) } else { Err(BrokerError::Invalid) }
}

/// Strict arguments of a tool in [`NAMES`]; `None` for other tools.
pub(super) fn parse(name: &str, arguments: &Map<String, Value>) -> Option<BrokerResult<Call>> {
    let call = || -> BrokerResult<Call> {
        Ok(match name {
            "list_routines" => {
                strict(arguments, &[])?;
                Call::List
            }
            "save_routine" => {
                let arguments = strict(arguments, &["id", "name", "prompt", "schedule", "enabled"])?;
                let id = match arguments.get("id") {
                    None => None,
                    Some(_) => Some(uuid(arguments)?),
                };
                let schedule: RoutineSchedule =
                    serde_json::from_value(arguments.get("schedule").cloned().ok_or(BrokerError::Invalid)?).map_err(|_| BrokerError::Invalid)?;
                let enabled = match arguments.get("enabled") {
                    None => true,
                    Some(value) => value.as_bool().ok_or(BrokerError::Invalid)?,
                };
                Call::Save { id, name: required(arguments, "name")?, prompt: required(arguments, "prompt")?, schedule, enabled }
            }
            "set_routine_enabled" => {
                let arguments = strict(arguments, &["id", "enabled"])?;
                Call::SetEnabled { id: uuid(arguments)?, enabled: arguments.get("enabled").and_then(Value::as_bool).ok_or(BrokerError::Invalid)? }
            }
            "delete_routine" => Call::Delete { id: uuid(strict(arguments, &["id"])?)? },
            _ => return Err(BrokerError::Invalid),
        })
    };
    NAMES.contains(&name).then(call)
}

fn schedule_text(schedule: &RoutineSchedule) -> String {
    match schedule {
        RoutineSchedule::Once { at } => format!("한 번: {at}"),
        RoutineSchedule::Interval { minutes } => format!("{minutes}분마다"),
        RoutineSchedule::Daily { time, timezone } => format!("매일 {time} ({timezone})"),
    }
}

fn routine_json(run: &Run, routine: &Routine) -> Value {
    json!({
        "id": routine.id, "name": run.redact_all(&routine.name), "prompt": run.redact_all(&routine.prompt),
        "schedule": routine.schedule, "enabled": routine.enabled, "nextRunAt": routine.next_run_at,
    })
}

/// Session rules describing the routine tools.
pub(crate) fn rules(recipient: bool) -> Vec<String> {
    if recipient {
        return Vec::new();
    }
    vec!["Routines are recurring tasks this bot runs on a schedule. When the user asks to set up, change, pause, resume, or remove one, discover workspace__list_routines, workspace__save_routine, workspace__set_routine_enabled, and workspace__delete_routine with search_tool and call them with use_tool. Call list_routines first: it returns the current server time for one-time schedules. Schedules are once {at: ISO time with offset}, interval {minutes}, or daily {time: HH:mm, timezone: IANA}. Every change waits for browser approval; do not claim a routine exists or changed until the broker confirms it.".to_owned()]
}

impl Runtime {
    pub(super) async fn routine_call(&self, run: &Arc<Run>, call: Call) -> BrokerResult<Value> {
        if self.is_recipient(run) {
            return Err(HttpError::forbidden("Recipient conversations are chat-only and cannot manage routines.").into());
        }
        let bot_id = run.bot.id.clone();
        if let Call::List = call {
            let (now, routines) = self.store.read(|state| {
                let routines: Vec<Value> = state.disk().routines.iter().filter(|r| r.bot_id == bot_id).map(|r| routine_json(run, r)).collect();
                (iso(state.now()), routines)
            });
            return Ok(json!({ "now": now, "routines": routines }));
        }
        if run.flags().routine_changes >= MAX_ROUTINE_CHANGES_PER_RUN {
            return Err(HttpError::conflict("At most three routine changes can be made per model turn. Ask the user before changing more.").into());
        }
        let existing = |id: &str| -> BrokerResult<Routine> {
            self.store
                .read(|state| state.disk().routines.iter().find(|r| r.id == id && r.bot_id == bot_id).cloned())
                .ok_or_else(|| HttpError::not_found("Routine does not exist for this bot.").into())
        };
        let (action, title_name, summary) = match &call {
            Call::Save { id: None, name, prompt, schedule, enabled } => (
                "생성",
                name.clone(),
                format!("이름: {name}\n일정: {}\n상태: {}\n\n지시:\n{prompt}", schedule_text(schedule), if *enabled { "사용" } else { "일시 중지" }),
            ),
            Call::Save { id: Some(id), name, prompt, schedule, enabled } => {
                existing(id)?;
                (
                    "업데이트",
                    name.clone(),
                    format!("이름: {name}\n일정: {}\n상태: {}\n\n지시:\n{prompt}", schedule_text(schedule), if *enabled { "사용" } else { "일시 중지" }),
                )
            }
            Call::SetEnabled { id, enabled } => {
                let routine = existing(id)?;
                (
                    if *enabled { "재개" } else { "일시 중지" },
                    routine.name.clone(),
                    format!("{} 루틴을 {}합니다.\n일정: {}", routine.name, if *enabled { "다시 시작" } else { "일시 중지" }, schedule_text(&routine.schedule)),
                )
            }
            Call::Delete { id } => {
                let routine = existing(id)?;
                (
                    "삭제",
                    routine.name.clone(),
                    format!(
                        "{} 루틴이 삭제되고 이후 실행이 중지됩니다. 이전 실행 기록과 대화는 유지됩니다.\n일정: {}",
                        routine.name,
                        schedule_text(&routine.schedule)
                    ),
                )
            }
            Call::List => unreachable!(),
        };
        let title = run.redact(&format!("루틴 {action}: {}", truncate_utf16(&title_name, 80)));
        let description = run.redact_all(&format!("{}이(가) 루틴을 저장하도록 허용할까요?\n\n{summary}", run.bot.name));
        let tool = ToolActivity {
            id: new_id(),
            title: title.clone(),
            kind: "edit".into(),
            status: ToolStatus::Pending,
            input: Some(description.clone()),
            output: None,
            paths: None,
            local_only: None,
            approved_by_rule: None,
        };
        let tool_id = tool.id.clone();
        let (allow, _, options) = approval_options("Save routine once", "Deny");
        let permission = PendingPermission { id: new_id(), title, kind: "edit".into(), description, options, paths: Vec::new(), local_only: None };
        let choice = self.approve(run, permission, Some(tool)).await?;
        if choice.as_deref() != Some(allow.as_str()) || run.inactive() {
            let message = "Routine change was denied or cancelled.";
            self.settle_tool(run, &tool_id, ToolStatus::Failed, Some(message.into())).await;
            return Err(HttpError::forbidden(message).into());
        }
        let result = {
            let (store, run) = (Arc::clone(&self.store), Arc::clone(run));
            self.queue
                .run(async move {
                    store
                        .mutate(move |state| {
                            if run.inactive() {
                                return Err(HttpError::conflict("The run ended before the routine could be changed."));
                            }
                            let bot_id = run.bot.id.clone();
                            match call {
                                Call::Save { id: None, name, prompt, schedule, enabled } => {
                                    state.add_routine(RoutineInput { bot_id, name, prompt, schedule, enabled }).map(|r| (Some(r.id), r.name))
                                }
                                Call::Save { id: Some(id), name, prompt, schedule, enabled } => {
                                    state.update_routine(&id, RoutineInput { bot_id, name: name.clone(), prompt, schedule, enabled }).map(|()| (Some(id), name))
                                }
                                Call::SetEnabled { id, enabled } => {
                                    let name = state.routine(&id)?.name.clone();
                                    state.set_routine_enabled(&id, enabled).map(|()| (Some(id), name))
                                }
                                Call::Delete { id } => {
                                    let name = state.routine(&id)?.name.clone();
                                    state.delete_routine(&id).map(|()| (None, name))
                                }
                                Call::List => unreachable!(),
                            }
                        })
                        .await
                })
                .await
        };
        match result {
            Ok((id, name)) => {
                run.flags().routine_changes += 1;
                let label = match action {
                    "생성" => "생성됨",
                    "업데이트" => "업데이트됨",
                    "삭제" => "삭제됨",
                    "일시 중지" => "일시 중지됨",
                    _ => "재개됨",
                };
                self.settle_tool(run, &tool_id, ToolStatus::Completed, Some(run.redact(&format!("{label} 루틴: {name}")))).await;
                let routine = id.and_then(|id| self.store.read(|state| state.disk().routines.iter().find(|r| r.id == id).map(|r| routine_json(run, r))));
                Ok(json!({ "status": label, "routine": routine, "deleted": routine.is_none() }))
            }
            Err(error) => {
                self.settle_tool(run, &tool_id, ToolStatus::Failed, Some(run.redact(&error.message))).await;
                Err(error.into())
            }
        }
    }
}
