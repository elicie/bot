//! Enqueue-only same-bot delegation. The current run never waits for workers while holding the
//! global execution slot; the dispatcher starts approved tasks only after their parent completes.

use std::sync::Arc;

use grok_bot_model::{HttpError, PendingPermission, ToolActivity, ToolStatus, new_id};
use serde_json::{Value, json};

use super::{BrokerResult, approval_options, conflict_finished};
use crate::runtime::{Run, Runtime, workers::receipt};

pub(super) fn tool() -> Value {
    json!({ "name": "spawn_worker", "description": "Optionally delegate a self-contained read-only subtask to this same bot only when splitting the user's task would materially help. Answer ordinary questions directly. Each new task requires explicit one-use browser approval. At most three distinct tasks per ordinary 1:1 turn, depth one, and one globally active run. Exact task retries return the original receipt without approval. Workers run after this turn ends; all siblings are reported together in one automatic follow-up. Never wait or poll. Workers cannot write, message bots, use computers/custom MCP servers, or spawn more workers.", "inputSchema": { "type": "object", "properties": { "task": { "type": "string", "minLength": 1, "maxLength": 4000 } }, "required": ["task"], "additionalProperties": false } })
}

impl Runtime {
    pub(super) async fn spawn_worker(&self, run: &Arc<Run>, task: String) -> BrokerResult<Value> {
        // These flags also reject report turns before they have any durable child record.
        if !self.worker_spawn_allowed(run) {
            return Err(HttpError::forbidden("Workers and automatic follow-up turns cannot spawn workers (maximum depth one).").into());
        }
        self.maintain_worker_history(false).await?;
        let existing = self.store.read(|state| {
            state.validate_worker_source(&run.conversation_id, &run.message_id, &run.run_id, &task)?;
            Ok::<_, HttpError>(
                state.find_existing_worker(&run.conversation_id, &run.message_id, &run.run_id, &task).map(|(batch, worker)| receipt(&batch.id, worker)),
            )
        })?;
        if let Some(existing) = existing {
            return Ok(existing);
        }
        let title = "Spawn read-only worker".to_owned();
        let workspace = if run.settings.workspace.is_empty() {
            "No file workspace authorized.".to_owned()
        } else {
            format!("Authorized read-only workspace (fixed for this task): {}", run.settings.workspace)
        };
        let description = run.redact_all(&format!(
            "Delegate this task to a separate read-only worker of {}. It can read this bot's skills, memories and knowledge. {workspace} No writes, computer access, custom MCP servers, or further delegation. It runs after this turn and all sibling results return in one automatic follow-up.\n\n{task}",
            run.bot.name
        ));
        let tool = ToolActivity {
            id: new_id(),
            title: title.clone(),
            kind: "other".into(),
            status: ToolStatus::Pending,
            input: Some(description.clone()),
            output: None,
            paths: None,
            local_only: None,
            approved_by_rule: None,
        };
        let tool_id = tool.id.clone();
        let (allow, _, options) = approval_options("Run worker once", "Deny");
        let permission = PendingPermission { id: new_id(), title, kind: "other".into(), description, options, paths: Vec::new(), local_only: None };
        // Deliberately bypass category auto-allow rules: every distinct task is reviewed once.
        let decision = self.approve(run, permission, Some(tool)).await?;
        if run.inactive() || decision.as_deref() != Some(allow.as_str()) {
            let message = "The user denied this worker or cancelled the run.";
            self.settle_tool(run, &tool_id, ToolStatus::Failed, Some(message.into())).await;
            return Err(HttpError::forbidden(message).into());
        }
        let queue_task = || {
            let (store, run, task) = (Arc::clone(&self.store), Arc::clone(run), task.clone());
            self.queue.run(async move {
                store
                    .mutate(move |state| {
                        if run.inactive() {
                            return Err(conflict_finished());
                        }
                        if state.settings().workspace != run.settings.workspace {
                            return Err(HttpError::conflict("The authorized workspace changed. Submit and approve a new task."));
                        }
                        state.spawn_worker(&run.conversation_id, &run.message_id, &run.run_id, &task)
                    })
                    .await
            })
        };
        let mut queued = queue_task().await;
        if queued.as_ref().is_err_and(|error| error.status == 413) && !run.inactive() {
            // Admission failed before mutation. Reclaim only terminal private cache in a separate
            // durable maintenance write, then retry the already-approved task exactly once.
            queued = match self.reclaim_worker_capacity().await {
                Ok(()) => queue_task().await,
                Err(error) => Err(error),
            };
        }
        match queued {
            Ok((batch, worker)) => {
                self.settle_tool(run, &tool_id, ToolStatus::Completed, Some(run.redact(&format!("Queued worker {}.", worker.id)))).await;
                Ok(receipt(&batch.id, &worker))
            }
            Err(error) => {
                self.settle_tool(run, &tool_id, ToolStatus::Failed, Some(run.redact(&error.message))).await;
                Err(error.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::CommandQueue;
    use crate::runtime::{RunFlags, RuntimeOptions, custom_mcp};
    use agent_client_protocol as acp;
    use grok_bot_model::{PrivateSettings, system_clock};
    use grok_bot_store::Store;
    use std::sync::Mutex;
    use tokio::sync::watch;

    #[tokio::test]
    async fn worker_broker_defaults_to_deny_and_native_permissions_never_ask() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path(), system_clock()).await.unwrap();
        let bot = store.read(|state| state.disk().bots[0].clone());
        let conversation = store.mutate(|state| state.add_conversation(&bot.id)).await.unwrap();
        let runtime = Runtime::new(store, CommandQueue::new(), RuntimeOptions::new(directory.path().join("engine")));
        let run = Arc::new(Run {
            conversation_id: conversation.id,
            run_id: new_id(),
            message_id: new_id(),
            settings: PrivateSettings::default(),
            bot,
            token: "worker-fixture-token".into(),
            fingerprint: String::new(),
            history: Vec::new(),
            attachments: Vec::new(),
            speaker: None,
            fresh_prompt: None,
            loaded_prompt: None,
            extra_rules: None,
            report_deliveries: Vec::new(),
            worker_id: Some(new_id()),
            worker_deadline: None,
            worker_report: None,
            keys: Vec::new(),
            mcp: custom_mcp::RunMcp::new(Vec::new(), Vec::new()),
            flags: Mutex::new(RunFlags { session_id: Some("worker-session".into()), ..RunFlags::default() }),
            activity: tokio::sync::Notify::new(),
            stop: watch::Sender::new(false),
            done: watch::Sender::new(false),
            broker: tokio::sync::Mutex::new(()),
        });
        runtime.runs().push(Arc::clone(&run));
        let authorization = format!("Bearer {}", run.token);
        let listed = runtime.mcp(&run.run_id, Some(&authorization), json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).await.unwrap().unwrap();
        let names: Vec<&str> = listed["result"]["tools"].as_array().unwrap().iter().map(|tool| tool["name"].as_str().unwrap()).collect();
        assert_eq!(names.len(), 7);
        assert!(names.iter().all(|name| crate::runtime::workers::READ_TOOLS.contains(name)));
        for name in [
            "write_file",
            "create_skill",
            "create_bot",
            "send_message",
            "save_memory",
            "forget_memory",
            "computer_observe",
            "computer_act",
            "spawn_worker",
            "save_routine",
            "set_routine_enabled",
            "delete_routine",
            "list_routines",
            "list_bots",
            "future_mutating_tool",
            "custom__tool",
        ] {
            let response = runtime
                .mcp(&run.run_id, Some(&authorization), json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": name, "arguments": {}}}))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(response["result"]["isError"], true, "{name}: {response}");
            assert!(response["result"]["content"][0]["text"].as_str().unwrap().contains("read-only"));
        }
        let request = acp::RequestPermissionRequest::new(
            "worker-session",
            acp::ToolCallUpdate::new("fixture-call", acp::ToolCallUpdateFields::new().title("use_tool").kind(acp::ToolKind::Other)),
            vec![acp::PermissionOption::new("allow", "Allow once", acp::PermissionOptionKind::AllowOnce)],
        );
        let response = runtime.request_permission(request).await;
        assert!(matches!(response.outcome, acp::RequestPermissionOutcome::Cancelled));
        assert!(run.flags().approvals.is_empty());
        runtime.runs().clear();
    }
}
