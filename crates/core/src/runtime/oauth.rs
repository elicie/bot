//! Bounded OAuth preparation before admitting a turn. Existing turn/session APIs are unchanged.
//! Credentials are frozen behind private loopback leases before the ACP declarations are built.

use futures::future::BoxFuture;
use grok_bot_model::{HttpError, HttpResult, McpPair, McpServerConfig};
use grok_bot_store::vault::{McpStartupFailure, ResolvedMcpServer};
use sha2::{Digest, Sha256};

use super::{RUN_BUSY, Runtime, State, Turn, turns};
use crate::mcp_oauth::relay;

pub(super) struct Prepared {
    pub bot_id: String,
    pub signature: String,
    pub servers: Vec<ResolvedMcpServer>,
    pub failures: Vec<McpStartupFailure>,
    pub keys: Vec<String>,
    pub leases: Vec<relay::Lease>,
}

/// Unlike the normal session fingerprint this also binds stable record IDs and edits. A deletion
/// and recreation with the same visible name cannot reuse a prepared credential lease.
pub(super) fn signature(state: &State, bot_id: &str) -> String {
    let records: Vec<_> = state.mcp_servers_for(bot_id).map(|record| (&record.id, &record.bot_id, &record.name, record.enabled, &record.config)).collect();
    hex::encode(Sha256::digest(serde_json::to_vec(&records).unwrap_or_default()))
}

impl Runtime {
    pub(super) async fn prepare_oauth(&self, conversation_id: &str, turn: &Turn) -> HttpResult<Option<Prepared>> {
        let Some(service) = self.oauth() else { return Ok(None) };
        if self.closing.load(std::sync::atomic::Ordering::SeqCst) || self.is_paused() {
            return Ok(None);
        }
        let Some((bot_id, signature, servers, mut failures, mut keys)) = self.store.read(|state| -> HttpResult<_> {
            if state.active().is_some() {
                state.assert_writable()?;
                return Err(HttpError::conflict(RUN_BUSY));
            }
            state.assert_idle()?;
            if !state.runtime().available || state.settings().base_url.is_empty() || state.settings().model.is_empty() {
                return Ok(None);
            }
            if state.disk().bot_deliveries.iter().any(|delivery| delivery.recipient_conversation_id == conversation_id) {
                return Ok(None);
            }
            let report = match turn {
                Turn::Report => Some(turns::report(state, conversation_id)?),
                _ => None,
            };
            let requested = match turn {
                Turn::Prompt(prompt) => Some(prompt.as_str()),
                Turn::Group => None,
                Turn::Report => report.as_ref().map(|report| report.prompt.as_str()),
            };
            let plan = turns::plan(state, conversation_id, requested)?;
            let (servers, failures) = state.resolve_mcp_servers(&plan.bot.id);
            Ok(Some((plan.bot.id.clone(), signature(state, &plan.bot.id), servers, failures, state.redaction_keys())))
        })?
        else {
            return Ok(None);
        };
        // No store lock is held while discovering/refreshing remote credentials. The caller's
        // command queue still orders final admission with other application commands.
        // Keep provenance before the service can add credentials. A concurrent logout/retirement
        // must never make a service-injected bearer fall through as an unmanaged static config.
        let original_configs: std::collections::HashMap<_, _> = servers.iter().map(|server| (server.name.clone(), server.config.clone())).collect();
        let (prepared, oauth_failures) = service.prepare(&bot_id, servers).await;
        failures.extend(oauth_failures);
        keys.extend(self.store.read(State::redaction_keys));
        keys.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        keys.dedup();
        let mut servers = Vec::new();
        let mut leases = Vec::new();
        for mut server in prepared {
            let record = self.store.read(|state| state.mcp_servers_for(&bot_id).find(|record| record.name == server.name).cloned());
            let Some(record) = record else { continue };
            let managed = service.manages(&record).await;
            if !managed && original_configs.get(&server.name) != Some(&server.config) {
                failures
                    .push(McpStartupFailure { name: server.name, message: "OAuth 서버 구성이 변경되었어요. 다시 로그인해 주세요.".into() });
                continue;
            }
            if managed {
                let McpServerConfig::Http { url, headers } = &server.config else { continue };
                let original = match &record.config {
                    McpServerConfig::Http { url, .. } => url,
                    _ => continue,
                };
                if url.as_str() != original.as_str() {
                    failures.push(McpStartupFailure {
                        name: server.name, message: "OAuth 서버 구성이 변경되었어요. 다시 로그인해 주세요.".into()
                    });
                    continue;
                }
                let on_unauthorized = {
                    let service = service.clone();
                    let bot = bot_id.clone();
                    let name = server.name.clone();
                    let access = headers
                        .iter()
                        .find(|header| header.name.eq_ignore_ascii_case("authorization"))
                        .and_then(|header| header.value.strip_prefix("Bearer "))
                        .unwrap_or_default()
                        .to_owned();
                    std::sync::Arc::new(move || -> BoxFuture<'static, ()> {
                        let service = service.clone();
                        let bot = bot.clone();
                        let name = name.clone();
                        let access = access.clone();
                        Box::pin(async move { service.report_unauthorized_token(&bot, &name, &access).await })
                    })
                };
                #[cfg(test)]
                let started = match self.oauth_test_upstream.get() {
                    Some(upstream) => upstream.start(url.as_str(), headers.clone(), keys.clone(), on_unauthorized).await,
                    None => relay::start(url.as_str(), headers.clone(), keys.clone(), on_unauthorized).await,
                };
                #[cfg(not(test))]
                let started = relay::start(url.as_str(), headers.clone(), keys.clone(), on_unauthorized).await;
                match started {
                    Ok(lease) => {
                        // Only the ephemeral lease bearer reaches ACP/engine state. The remote
                        // token stays in the in-process relay and encrypted OAuth vault.
                        server.config = McpServerConfig::Http {
                            url: lease.url.clone(),
                            headers: vec![McpPair { name: "Authorization".into(), value: lease.authorization.clone() }],
                        };
                        keys.push(lease.authorization.clone());
                        if let Some(token) = lease.authorization.strip_prefix("Bearer ") {
                            keys.push(token.to_owned());
                        }
                        leases.push(lease);
                    }
                    Err(_) => {
                        failures.push(McpStartupFailure {
                            name: server.name, message: "OAuth MCP 연결을 준비하지 못했어요. 다시 시도해 주세요.".into()
                        });
                        continue;
                    }
                }
            }
            servers.push(server);
        }
        Ok(Some(Prepared { bot_id, signature, servers, failures, keys, leases }))
    }
}

#[cfg(test)]
#[path = "oauth_tests.rs"]
mod tests;
