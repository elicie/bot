//! Category-aware approval: the user's permission rules decide whether a model-initiated action
//! shows its one-use approval card or proceeds without asking. Every broker mutation goes through
//! [`Runtime::approve_category`]; engine (ACP) permission requests have no category and always ask.
//!
//! Safety floor that rules never lift:
//! - Local-only (managed computer) approvals always ask and stay loopback-only.
//! - Bot-delivery recipient conversations always ask: their prompt is another bot's message.
//! - Webhook-triggered routine runs always ask: their prompt can carry external payload data.
//! - Report (follow-up) turns always ask: their input is another bot's answer.
//! - The callers' own checks (file hashes and size limits, delivery hop limits, recipient
//!   restrictions, one-per-turn limits) run before this function and are unchanged.
//!
//! An auto-approved action is still visible: its tool activity records the rule in
//! `approvedByRule`, which the chat shows as "내 규칙으로 자동 승인됨".

use std::sync::Arc;

use grok_bot_model::{HttpResult, PendingPermission, PermissionCategory, ToolActivity};
use grok_bot_store::permissions::PermissionPolicy;

use super::{Run, Runtime, message_mut};

impl Runtime {
    /// The category policy for this run: `Some(rule_id)` when a rule allows the action without
    /// asking and no safety floor applies.
    fn auto_approval(&self, run: &Run, category: PermissionCategory, permission: &PendingPermission) -> Option<String> {
        if permission.local_only == Some(true) || self.rules_suspended(run) {
            return None;
        }
        self.store.read(|state| match state.permission_policy(category, &run.bot.id) {
            PermissionPolicy::Allow { rule_id } => Some(rule_id),
            PermissionPolicy::Ask => None,
        })
    }

    /// The safety floor for runs whose prompt is not the user's own message: bot-delivery recipient
    /// turns, webhook-triggered routine runs, and report turns (another bot's answer as data).
    /// Permission rules never auto-approve their actions.
    pub(crate) fn rules_suspended(&self, run: &Run) -> bool {
        !run.report_deliveries.is_empty()
            || self.store.read(|state| {
                state.disk().bot_deliveries.iter().any(|delivery| delivery.recipient_conversation_id == run.conversation_id)
                    || state.is_webhook_conversation(&run.conversation_id)
            })
    }

    /// Approve a model-initiated action of `category`. When the user's rules allow it, the tool
    /// activity is recorded as approved by that rule and the card's one-use allow option is
    /// returned without waiting; otherwise this is [`Runtime::approve`] (the card asks).
    pub(crate) async fn approve_category(
        &self,
        run: &Arc<Run>,
        category: PermissionCategory,
        permission: PendingPermission,
        mut tool: ToolActivity,
    ) -> HttpResult<Option<String>> {
        let allow = permission.options.iter().find(|option| option.kind == "allow_once").map(|option| option.id.clone());
        let (Some(rule_id), Some(allow)) = (self.auto_approval(run, category, &permission), allow) else {
            return self.approve(run, permission, Some(tool)).await;
        };
        if run.inactive() {
            return Ok(None);
        }
        tool.approved_by_rule = Some(rule_id);
        run.reset_idle();
        self.persist(run, move |state, run| {
            message_mut(state, run)?.tools.push(tool);
            Ok(())
        })
        .await?;
        Ok(Some(allow))
    }
}
