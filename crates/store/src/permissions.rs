//! Permission rules ("권한" settings): per action category, whether a model-initiated action asks
//! for a one-use browser approval ("항상 묻기", the default) or proceeds without asking
//! ("묻지 않고 허용").
//!
//! Precedence follows the original app: when rules conflict, an ask rule always wins ("누가
//! 작성했든 항상 묻기 규칙이 우선해요"). A rule applies to every bot or to one bot. Rules only
//! decide whether the approval card is shown; every other check of the action (file hashes and
//! size limits, delivery hop limits, recipient restrictions, local-only approvals) stays in force.
//!
//! Loosening (anything that can make an action proceed without asking: adding or switching a rule
//! to allow, or deleting an ask rule) requires `may_loosen`, which the HTTP layer grants only to
//! loopback clients. The server has no login, so a LAN client must not be able to remove the
//! approvals that protect the server computer's files.

use grok_bot_model::{
    HttpError, HttpResult, PermissionAction, PermissionCategory, PermissionRule, PermissionRuleInput, PermissionRulePatch, iso, new_id,
};

use crate::State;
use crate::validate::{is_timestamp, is_uuid};

pub const MAX_PERMISSION_RULES: usize = 100;

pub const LOOSEN_REQUIRES_LOOPBACK: &str = "Only a loopback connection on the server computer can add or loosen rules that allow actions without asking. This network server has no user authentication.";

/// The decision for one action: ask (show the card) or allow because of `rule_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionPolicy {
    Ask,
    Allow { rule_id: String },
}

/// Persisted rule shape beyond what serde checks.
pub(crate) fn rule_valid(rule: &PermissionRule) -> bool {
    is_uuid(&rule.id) && rule.bot_id.as_deref().is_none_or(is_uuid) && is_timestamp(&rule.created_at) && is_timestamp(&rule.updated_at)
}

/// Load-time set checks: bounded count, unique IDs, one rule per (category, bot scope).
pub(crate) fn rules_valid(rules: &[PermissionRule]) -> bool {
    if rules.len() > MAX_PERMISSION_RULES || !rules.iter().all(rule_valid) {
        return false;
    }
    let ids: std::collections::HashSet<&str> = rules.iter().map(|rule| rule.id.as_str()).collect();
    let scopes: std::collections::HashSet<(PermissionCategory, Option<&str>)> =
        rules.iter().map(|rule| (rule.category, rule.bot_id.as_deref())).collect();
    ids.len() == rules.len() && scopes.len() == rules.len()
}

/// Rules of `rules` that govern `category` for `bot_id`, decided with ask precedence.
pub fn policy_for(rules: &[PermissionRule], category: PermissionCategory, bot_id: &str) -> PermissionPolicy {
    let matching = rules.iter().filter(|rule| rule.category == category && rule.bot_id.as_deref().is_none_or(|id| id == bot_id));
    let mut allow: Option<&PermissionRule> = None;
    for rule in matching {
        match rule.action {
            PermissionAction::Ask => return PermissionPolicy::Ask,
            // A bot-specific allow is reported in preference to the all-bots allow.
            PermissionAction::Allow => {
                if allow.is_none_or(|current| current.bot_id.is_none()) {
                    allow = Some(rule);
                }
            }
        }
    }
    match allow {
        Some(rule) => PermissionPolicy::Allow { rule_id: rule.id.clone() },
        None => PermissionPolicy::Ask,
    }
}

fn loosen(may_loosen: bool) -> HttpResult<()> {
    if may_loosen { Ok(()) } else { Err(HttpError::forbidden(LOOSEN_REQUIRES_LOOPBACK)) }
}

fn rule_index(state: &State, id: &str) -> HttpResult<usize> {
    state.disk.permission_rules.iter().position(|rule| rule.id == id).ok_or_else(|| HttpError::not_found("Permission rule does not exist."))
}

impl State {
    pub fn permission_rules(&self) -> &[PermissionRule] {
        &self.disk.permission_rules
    }

    /// The category policy for one bot. This is the single lookup the runtime's approval path
    /// uses (`Runtime::approve_category`); features that add a category call that, not this.
    pub fn permission_policy(&self, category: PermissionCategory, bot_id: &str) -> PermissionPolicy {
        policy_for(&self.disk.permission_rules, category, bot_id)
    }

    pub fn add_permission_rule(&mut self, input: PermissionRuleInput, may_loosen: bool) -> HttpResult<PermissionRule> {
        if let Some(bot_id) = input.bot_id.as_deref()
            && !is_uuid(bot_id)
        {
            return Err(HttpError::bad_request("botId: Invalid uuid"));
        }
        self.assert_writable()?;
        if input.action == PermissionAction::Allow {
            loosen(may_loosen)?;
        }
        if let Some(bot_id) = input.bot_id.as_deref() {
            self.bot(bot_id)?;
        }
        if self.disk.permission_rules.len() >= MAX_PERMISSION_RULES {
            return Err(HttpError::conflict("At most 100 permission rules can be saved."));
        }
        if self.disk.permission_rules.iter().any(|rule| rule.category == input.category && rule.bot_id == input.bot_id) {
            return Err(HttpError::conflict("A rule for this action and bot already exists. Edit its action instead."));
        }
        let now = iso(self.now());
        let rule = PermissionRule {
            id: new_id(),
            category: input.category,
            action: input.action,
            bot_id: input.bot_id,
            created_at: now.clone(),
            updated_at: now,
        };
        self.disk_mut().permission_rules.push(rule.clone());
        Ok(rule)
    }

    pub fn update_permission_rule(&mut self, id: &str, patch: PermissionRulePatch, may_loosen: bool) -> HttpResult<PermissionRule> {
        self.assert_writable()?;
        let index = rule_index(self, id)?;
        let current = &self.disk.permission_rules[index];
        if current.action == patch.action {
            return Ok(current.clone());
        }
        if patch.action == PermissionAction::Allow {
            loosen(may_loosen)?;
        }
        let updated = PermissionRule { action: patch.action, updated_at: iso(self.now()), ..current.clone() };
        self.disk_mut().permission_rules[index] = updated.clone();
        Ok(updated)
    }

    /// Deleting an ask rule can expose a broader allow rule, so it counts as loosening.
    pub fn delete_permission_rule(&mut self, id: &str, may_loosen: bool) -> HttpResult<PermissionRule> {
        self.assert_writable()?;
        let index = rule_index(self, id)?;
        if self.disk.permission_rules[index].action == PermissionAction::Ask {
            loosen(may_loosen)?;
        }
        Ok(self.disk_mut().permission_rules.remove(index))
    }

    /// Drop the rules scoped to a bot (for bot deletion). Marks dirty only when one is removed.
    pub fn remove_permission_rules_for_bot(&mut self, bot_id: &str) {
        if self.disk.permission_rules.iter().any(|rule| rule.bot_id.as_deref() == Some(bot_id)) {
            self.disk_mut().permission_rules.retain(|rule| rule.bot_id.as_deref() != Some(bot_id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(category: PermissionCategory, action: PermissionAction, bot_id: Option<&str>) -> PermissionRule {
        PermissionRule {
            id: new_id(),
            category,
            action,
            bot_id: bot_id.map(str::to_owned),
            created_at: "2026-11-01T00:00:00.000Z".into(),
            updated_at: "2026-11-01T00:00:00.000Z".into(),
        }
    }

    #[test]
    fn ask_rules_take_precedence_and_scope_narrows() {
        use PermissionAction::*;
        use PermissionCategory::*;
        let bot = new_id();
        let other = new_id();
        assert_eq!(policy_for(&[], FileWrite, &bot), PermissionPolicy::Ask);
        let all = rule(FileWrite, Allow, None);
        assert_eq!(policy_for(std::slice::from_ref(&all), FileWrite, &bot), PermissionPolicy::Allow { rule_id: all.id.clone() });
        assert_eq!(policy_for(std::slice::from_ref(&all), BotMessage, &bot), PermissionPolicy::Ask);
        let ask_bot = rule(FileWrite, Ask, Some(&bot));
        let rules = [all.clone(), ask_bot.clone()];
        assert_eq!(policy_for(&rules, FileWrite, &bot), PermissionPolicy::Ask);
        assert_eq!(policy_for(&rules, FileWrite, &other), PermissionPolicy::Allow { rule_id: all.id.clone() });
        let only_bot = rule(SkillSave, Allow, Some(&bot));
        let rules = [only_bot.clone(), rule(SkillSave, Ask, None)];
        assert_eq!(policy_for(&rules, SkillSave, &bot), PermissionPolicy::Ask, "an all-bots ask rule beats a bot allow rule");
        let specific = rule(MemorySave, Allow, Some(&bot));
        let rules = [rule(MemorySave, Allow, None), specific.clone()];
        assert_eq!(policy_for(&rules, MemorySave, &bot), PermissionPolicy::Allow { rule_id: specific.id });
    }

    #[test]
    fn persisted_rule_sets_are_bounded_and_unique() {
        use PermissionAction::*;
        use PermissionCategory::*;
        let a = rule(FileWrite, Allow, None);
        assert!(rules_valid(std::slice::from_ref(&a)));
        assert!(!rules_valid(&[a.clone(), a.clone()]));
        assert!(!rules_valid(&[a.clone(), rule(FileWrite, Ask, None)]), "one rule per scope");
        assert!(rules_valid(&[a.clone(), rule(FileWrite, Ask, Some(&new_id()))]));
        assert!(!rule_valid(&PermissionRule { bot_id: Some("bot".into()), ..a.clone() }));
        assert!(!rule_valid(&PermissionRule { created_at: "yesterday".into(), ..a }));
    }
}
