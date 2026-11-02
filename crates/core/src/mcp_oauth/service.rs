//! Additive backend OAuth lifecycle, deliberately separate from application state and HTTP/UI.
//!
//! Admission (local + idle), browser opening, per-turn transport leases, and public-state wiring
//! belong to the caller. This service never enables a server or edits its saved JSON. Authorization
//! URLs are returned only from `login`; statuses and failures contain fixed diagnostics only.
//!
//! Understood as: a strict public-client OAuth/PKCE service and encrypted OAuth-only sidecar, not
//! a migration of unrelated secrets or a browser/account automation facility. Local automatic key
//! custody does not protect against a same-user process or theft of the whole data + key pair.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use grok_bot_model::{HttpError, HttpResult, McpPair, McpServerConfig, McpServerRecord};
use grok_bot_store::Store;
use grok_bot_store::vault::{McpStartupFailure, ResolvedMcpServer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, oneshot, watch};
use tokio::task::JoinHandle;
use zeroize::Zeroizing;

use super::{
    HttpsUrl,
    flow::{self, Error, Metadata, Network, Tokens},
    vault::{Vault, VaultError},
};

const MAX_BINDINGS: usize = 128;
const MAX_RETIRED: usize = 256;
const MAX_PENDING: usize = 16;
const REFRESH_EARLY_SECONDS: i64 = 60;
const LOGIN_SETUP_TIMEOUT: Duration = Duration::from_secs(35);
const PREPARE_TIMEOUT: Duration = Duration::from_secs(35);
const STARTUP_MESSAGE: &str = "OAuth 인증이 만료되었거나 서버 구성이 바뀌었어요. 다시 로그인한 뒤 다시 시도해 주세요.";
const STALE: &str = "The MCP server changed or was deleted. Start a new OAuth login.";
const UNSUPPORTED: &str = "OAuth requires an HTTPS remote MCP server without an existing Authorization header.";
const RESOURCE: &str = "OAuth resource must be the exact discovered identifier, on the same origin and a containing path of the MCP transport. Enter the resource explicitly when it differs from the endpoint.";
const STORAGE: &str = "The encrypted OAuth vault is unavailable. Credentials were not discarded.";

type Key = (String, String);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthStatus {
    pub status: &'static str,
    pub can_login: bool,
    pub message: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LoginInput {
    /// A pre-registered public client using `/mcp-oauth/callback` on 127.0.0.1. The authorization
    /// server must permit the ephemeral loopback port as specified by RFC8252 and advertise
    /// RFC9207 issuer-bound authorization responses. Other manual-client profiles fail closed.
    pub client_id: Option<String>,
    pub resource: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResponse {
    pub authorization_url: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    bot: String,
    server: String,
    name: String,
    config_sha256: String,
    transport: String,
    resource: String,
}

impl Binding {
    fn key(&self) -> Key {
        (self.bot.clone(), self.server.clone())
    }

    fn new(record: &McpServerRecord, resource: Option<&str>) -> HttpResult<Self> {
        let transport = eligible(record)?;
        let resource = resource.unwrap_or(transport);
        resource_contains_transport(resource, transport)?;
        Ok(Self {
            bot: record.bot_id.clone(),
            server: record.id.clone(),
            name: record.name.clone(),
            config_sha256: fingerprint(&record.config)?,
            transport: transport.into(),
            resource: resource.into(),
        })
    }

    fn matches(&self, record: &McpServerRecord) -> bool {
        self.bot == record.bot_id
            && self.server == record.id
            && self.name == record.name
            && eligible(record).is_ok_and(|transport| transport == self.transport)
            && fingerprint(&record.config).is_ok_and(|fingerprint| fingerprint == self.config_sha256)
            && resource_contains_transport(&self.resource, &self.transport).is_ok()
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    binding: Binding,
    metadata: Option<Metadata>,
    client_id: Option<String>,
    tokens: Option<Tokens>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Data {
    version: u8,
    entries: Vec<Entry>,
    /// Separate opaque values, not a serialized credential bundle. Encrypted and bounded so
    /// delayed echoes remain redacted across logout, token rotation and process restarts.
    retired: VecDeque<String>,
}

impl Default for Data {
    fn default() -> Self {
        Self { version: 1, entries: Vec::new(), retired: VecDeque::new() }
    }
}

impl Data {
    fn validate(&self) -> bool {
        let mut keys = std::collections::HashSet::new();
        self.version == 1
            && self.entries.len() <= MAX_BINDINGS
            && self.retired.len() <= MAX_RETIRED
            && self.retired.iter().all(|value| flow::credential_valid(value))
            && self.entries.iter().all(|entry| {
                let binding = &entry.binding;
                !binding.bot.is_empty()
                    && binding.bot.len() <= 128
                    && !binding.server.is_empty()
                    && binding.server.len() <= 128
                    && !binding.name.is_empty()
                    && binding.name.len() <= 128
                    && binding.config_sha256.len() == 64
                    && binding.config_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && keys.insert(binding.key())
                    && resource_contains_transport(&binding.resource, &binding.transport).is_ok()
                    && entry.metadata.as_ref().is_none_or(Metadata::valid)
                    && entry.client_id.as_deref().is_none_or(flow::client_id_valid)
                    && entry.tokens.as_ref().is_none_or(|tokens| {
                        entry.metadata.is_some()
                            && entry.client_id.is_some()
                            && flow::bearer_valid(&tokens.access)
                            && tokens.refresh.as_deref().is_none_or(flow::credential_valid)
                            && tokens.expires_at > 0
                    })
            })
    }

    fn entry(&self, key: &Key) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.binding.key() == *key)
    }
    fn entry_mut(&mut self, key: &Key) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|entry| entry.binding.key() == *key)
    }

    fn retire(&mut self, value: &str) {
        if !value.is_empty() && !self.retired.iter().any(|old| old == value) {
            self.retired.push_back(value.into());
        }
        while self.retired.len() > MAX_RETIRED {
            if let Some(value) = self.retired.pop_front() {
                drop(Zeroizing::new(value));
            }
        }
    }

    fn retire_tokens(&mut self, tokens: &Tokens) {
        self.retire(&tokens.access);
        if let Some(refresh) = &tokens.refresh {
            self.retire(refresh);
        }
    }

    fn redaction_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.retired.iter().cloned().collect();
        for tokens in self.entries.iter().filter_map(|entry| entry.tokens.as_ref()) {
            keys.push(tokens.access.clone());
            keys.extend(tokens.refresh.iter().cloned());
        }
        keys.sort();
        keys.dedup();
        keys
    }
}

struct Pending {
    generation: u64,
    binding: Binding,
    cancel: watch::Sender<bool>,
}

struct Inner {
    vault: Vault,
    data: Data,
    generations: HashMap<Key, u64>,
    next_generation: u64,
    pending: HashMap<Key, Pending>,
    notices: HashMap<Key, Error>,
    tasks: BTreeMap<u64, JoinHandle<()>>,
    failed: bool,
}

impl Inner {
    fn generation(&mut self, key: &Key) -> HttpResult<u64> {
        self.next_generation = self.next_generation.checked_add(1).ok_or_else(|| HttpError::unavailable(STORAGE))?;
        self.generations.insert(key.clone(), self.next_generation);
        Ok(self.next_generation)
    }
    fn current(&self, key: &Key, generation: u64) -> bool {
        self.generations.get(key) == Some(&generation)
    }
    fn pending(&self, key: &Key, generation: u64) -> bool {
        self.pending.get(key).is_some_and(|pending| pending.generation == generation) && self.current(key, generation)
    }
}

pub struct OAuthService {
    store: Arc<Store>,
    inner: Mutex<Inner>,
    refresh_gate: Mutex<()>,
    network: Arc<dyn Network>,
    closed: AtomicBool,
    stop: watch::Sender<bool>,
}

impl OAuthService {
    pub async fn open(store: Arc<Store>) -> anyhow::Result<Arc<Self>> {
        Self::open_with(store, Arc::new(flow::Production)).await
    }

    #[cfg(test)]
    pub(super) async fn open_for_test(store: Arc<Store>, network: Arc<dyn Network>) -> anyhow::Result<Arc<Self>> {
        Self::open_with(store, network).await
    }

    async fn open_with(store: Arc<Store>, network: Arc<dyn Network>) -> anyhow::Result<Arc<Self>> {
        let root = store.data_dir().to_path_buf();
        let (vault, data) = tokio::task::spawn_blocking(move || -> Result<_, VaultError> {
            let vault = Vault::open(&root)?;
            let data: Data = match vault.load()? {
                Some(data) => data,
                None => {
                    let data = Data::default();
                    vault.save(&data)?;
                    data
                }
            };
            if !data.validate() {
                return Err(VaultError);
            }
            Ok((vault, data))
        })
        .await
        .map_err(|_| anyhow::anyhow!(STORAGE))?
        .map_err(|_| anyhow::anyhow!(STORAGE))?;
        store.set_external_redaction_keys(data.redaction_keys());
        let (stop, _) = watch::channel(false);
        let service = Arc::new(Self {
            store,
            network,
            closed: AtomicBool::new(false),
            stop,
            refresh_gate: Mutex::new(()),
            inner: Mutex::new(Inner {
                vault,
                data,
                generations: HashMap::new(),
                next_generation: 0,
                pending: HashMap::new(),
                notices: HashMap::new(),
                tasks: BTreeMap::new(),
                failed: false,
            }),
        });
        service.reconcile().await.map_err(|_| anyhow::anyhow!(STORAGE))?;
        Ok(service)
    }

    /// Retire local OAuth state only when its authoritative raw record was deleted or replaced
    /// with an OAuth-ineligible config. Saving config and enabling capabilities remain caller-owned
    /// local-only operations. Eligible HTTPS edits retain their binding and require new consent.
    pub async fn reconcile(&self) -> HttpResult<()> {
        let mut inner = self.inner.lock().await;
        self.writable(&inner)?;
        let removed: Vec<Key> = self.store.read(|state| {
            inner
                .data
                .entries
                .iter()
                .filter(|entry| {
                    state.mcp_servers_for(&entry.binding.bot).find(|record| record.id == entry.binding.server).is_none_or(|record| eligible(record).is_err())
                })
                .map(|entry| entry.binding.key())
                .collect()
        });
        if removed.is_empty() {
            return Ok(());
        }
        // At most MAX_BINDINGS records are examined and one encrypted transaction is written.
        // Cancel transient attempts before retirement so no stale callback can restore a binding.
        for key in &removed {
            if let Some(pending) = inner.pending.remove(key) {
                pending.cancel.send_replace(true);
            }
            let _ = inner.generation(key)?;
        }
        let mut next = inner.data.clone();
        for key in &removed {
            if let Some(tokens) = inner.data.entry(key).and_then(|entry| entry.tokens.as_ref()) {
                next.retire_tokens(tokens);
            }
        }
        next.entries.retain(|entry| !removed.contains(&entry.binding.key()));
        self.commit(&mut inner, next)?;
        for key in &removed {
            inner.generations.remove(key);
            inner.notices.remove(key);
        }
        Ok(())
    }

    /// Positive ownership classification only; `prepare` is still the authorization gate. A
    /// changed eligible HTTPS record remains OAuth-bound until login/logout. Reconciliation locally
    /// retires saved manual/static/stdio transitions without enabling or modifying those configs.
    pub async fn manages(&self, record: &McpServerRecord) -> bool {
        self.inner.lock().await.data.entry(&key(record)).is_some()
    }

    /// A maintenance caller must not quiesce state while a consent callback or refresh can still
    /// write credentials. Nonblocking probes avoid inversion with the store's admission lock.
    pub fn is_busy(&self) -> bool {
        self.inner.try_lock().map_or(true, |inner| !inner.pending.is_empty()) || self.refresh_gate.try_lock().is_err()
    }

    pub async fn status(&self, record: &McpServerRecord) -> OAuthStatus {
        if self.closed.load(Ordering::Acquire) {
            return status("unavailable", false, Some("OAuth service is shut down."));
        }
        if eligible(record).is_err() {
            return status("unavailable", false, Some(UNSUPPORTED));
        }
        if !self.record_matches(record) {
            return status("unavailable", false, Some(STALE));
        }
        let inner = self.inner.lock().await;
        if inner.failed {
            return status("error", false, Some(STORAGE));
        }
        let key = key(record);
        if let Some(pending) = inner.pending.get(&key)
            && pending.binding.matches(record)
        {
            return status("awaiting_login", true, Some("Complete OAuth login in the browser on this computer."));
        }
        let Some(entry) = inner.data.entry(&key) else { return status("not_connected", true, None) };
        if !entry.binding.matches(record) {
            return status("login_required", true, Some(STALE));
        }
        if let Some(error) = inner.notices.get(&key) {
            return status(
                if matches!(error, Error::Relogin | Error::Cancelled | Error::Timeout) { "login_required" } else { "error" },
                true,
                Some(error.message()),
            );
        }
        if entry.tokens.as_ref().is_some_and(|tokens| tokens.expires_at > self.store.now().timestamp()) {
            status("connected", true, None)
        } else {
            status("login_required", true, Some("OAuth needs refresh or a new login before the next turn."))
        }
    }

    pub async fn login(self: &Arc<Self>, record: McpServerRecord, input: LoginInput) -> HttpResult<LoginResponse> {
        self.available()?;
        self.reconcile().await?;
        let binding = Binding::new(&record, input.resource.as_deref())?;
        if input.client_id.as_deref().is_some_and(|client| !flow::client_id_valid(client)) {
            return Err(HttpError::bad_request("Enter a valid public OAuth client ID."));
        }
        if !self.record_matches(&record) {
            return Err(HttpError::conflict(STALE));
        }
        let key = binding.key();
        let (generation, cancel) = {
            let mut inner = self.inner.lock().await;
            self.writable(&inner)?;
            inner.tasks.retain(|_, task| !task.is_finished());
            if inner.tasks.len() >= MAX_PENDING * 2 || (inner.pending.len() >= MAX_PENDING && !inner.pending.contains_key(&key)) {
                return Err(HttpError::conflict("Too many OAuth logins are already pending."));
            }
            if inner.data.entry(&key).is_none() && inner.data.entries.len() >= MAX_BINDINGS {
                return Err(HttpError::conflict("The OAuth credential vault has reached its binding limit."));
            }
            if let Some(previous) = inner.pending.remove(&key) {
                let _ = previous.cancel.send(true);
            }
            let generation = inner.generation(&key)?;
            let mut next = inner.data.clone();
            if let Some(previous) = next.entry(&key).and_then(|entry| entry.tokens.clone()) {
                next.retire_tokens(&previous);
            }
            next.entries.retain(|entry| entry.binding.key() != key);
            next.entries.push(Entry { binding: binding.clone(), metadata: None, client_id: None, tokens: None });
            self.commit(&mut inner, next)?;
            inner.notices.remove(&key);
            let (cancel, receiver) = watch::channel(false);
            inner.pending.insert(key.clone(), Pending { generation, binding: binding.clone(), cancel });
            (generation, receiver)
        };
        let (sender, receiver) = oneshot::channel();
        let mut inner = self.inner.lock().await;
        self.writable(&inner)?;
        if !inner.pending(&key, generation) {
            return Err(HttpError::conflict(STALE));
        }
        inner.tasks.retain(|_, task| !task.is_finished());
        let service = self.clone();
        let task = tokio::spawn(async move { service.run_login(binding, generation, input, cancel, sender).await });
        inner.tasks.insert(generation, task);
        drop(inner);
        // Dropping a caller during setup closes this receiver. The managed task observes that,
        // closes its listener/request, and clears pending status instead of leaking an attempt.
        receiver.await.unwrap_or_else(|_| Err(protocol_error(Error::Cancelled)))
    }

    async fn run_login(
        self: Arc<Self>,
        binding: Binding,
        generation: u64,
        input: LoginInput,
        mut cancel: watch::Receiver<bool>,
        mut response: oneshot::Sender<HttpResult<LoginResponse>>,
    ) {
        let key = binding.key();
        let mut stop = self.stop.subscribe();
        let setup = async {
            let callback = flow::Loopback::bind(input.client_id.is_some()).await?;
            let metadata = self.network.discover(&binding.resource).await?;
            // A manually pre-registered path is shared across attempts/issuers. Require RFC9207
            // rather than treating random state alone as an authorization-server mix-up defense.
            if input.client_id.is_some() && !metadata.require_iss {
                return Err(Error::UnsupportedIssuerBinding);
            }
            let client = match input.client_id {
                Some(client) if metadata.public_auth == Some(true) => client,
                Some(_) => return Err(Error::UnsupportedClient),
                None => flow::register(self.network.as_ref(), &metadata, &callback.redirect).await?,
            };
            let state = Zeroizing::new(flow::random()?);
            let verifier = Zeroizing::new(flow::random()?);
            let url = flow::authorization_url(&metadata, &client, &binding.resource, &callback.redirect, &state, &verifier)?;
            Ok::<_, Error>((callback, metadata, client, state, verifier, url))
        };
        let result = tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => Err(Error::Cancelled),
            _ = cancelled(&mut stop) => Err(Error::Cancelled),
            _ = response.closed() => Err(Error::Cancelled),
            result = tokio::time::timeout(LOGIN_SETUP_TIMEOUT, setup) => result.unwrap_or(Err(Error::Timeout)),
        };
        let (callback, metadata, client, state, verifier, url) = match result {
            Ok(setup) => setup,
            Err(error) => {
                self.finish_error(&key, generation, error).await;
                let _ = response.send(Err(protocol_error(error)));
                return;
            }
        };
        let commit = {
            let mut inner = self.inner.lock().await;
            if self.writable(&inner).is_err() || !inner.pending(&key, generation) || !self.binding_current(&binding) {
                Err(HttpError::conflict(STALE))
            } else {
                let mut next = inner.data.clone();
                if let Some(entry) = next.entry_mut(&key) {
                    entry.metadata = Some(metadata.clone());
                    entry.client_id = Some(client.clone());
                    self.commit(&mut inner, next)
                } else {
                    Err(HttpError::conflict(STALE))
                }
            }
        };
        if let Err(error) = commit {
            let _ = response.send(Err(error));
            self.finish_error(&key, generation, Error::Cancelled).await;
            return;
        }
        if response.send(Ok(LoginResponse { authorization_url: url })).is_err() {
            self.finish_error(&key, generation, Error::Cancelled).await;
            return;
        }
        // Setup and consent share the same owned, bounded task. Cancellation drops the listening
        // socket and any token request. No native/browser automation or plaintext PKCE storage.
        let redirect = callback.redirect.clone();
        let work = async {
            let code = callback.code(&state, &metadata).await?;
            if !self.attempt_current(&binding, generation).await {
                return Err(Error::Cancelled);
            }
            let tokens =
                flow::exchange(self.network.as_ref(), &metadata, &client, &binding.resource, &redirect, &verifier, &code, self.store.now().timestamp()).await?;
            self.accept_tokens(&binding, generation, tokens).await
        };
        let result = tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => Err(Error::Cancelled),
            _ = cancelled(&mut stop) => Err(Error::Cancelled),
            result = tokio::time::timeout(flow::ATTEMPT_TIMEOUT, work) => result.unwrap_or(Err(Error::Timeout)),
        };
        if let Err(error) = result {
            self.finish_error(&key, generation, error).await;
        }
    }

    pub async fn logout(&self, record: &McpServerRecord) -> HttpResult<OAuthStatus> {
        self.available()?;
        let key = key(record);
        let removed = {
            let mut inner = self.inner.lock().await;
            self.writable(&inner)?;
            if let Some(pending) = inner.pending.remove(&key) {
                let _ = pending.cancel.send(true);
            }
            inner.generations.remove(&key);
            let removed = inner.data.entry(&key).cloned();
            let mut next = inner.data.clone();
            if let Some(tokens) = removed.as_ref().and_then(|entry| entry.tokens.as_ref()) {
                next.retire_tokens(tokens);
            }
            next.entries.retain(|entry| entry.binding.key() != key);
            self.commit(&mut inner, next)?;
            inner.notices.remove(&key);
            removed
        };
        let mut stop = self.stop.subscribe();
        let message = match removed.as_ref().and_then(|entry| Some((entry.metadata.as_ref()?, entry.client_id.as_deref()?, entry.tokens.as_ref()?))) {
            Some((metadata, client, tokens)) if metadata.revocation.is_some() && removed.as_ref().is_some_and(|entry| self.binding_current(&entry.binding)) => {
                let revoked = tokio::select! {
                    _ = cancelled(&mut stop) => false,
                    result = tokio::time::timeout(Duration::from_secs(18), flow::revoke(self.network.as_ref(), metadata, client, tokens)) => result.unwrap_or(false),
                };
                if revoked {
                    "Local OAuth credentials were removed; remote revocation was requested."
                } else {
                    "Local OAuth credentials were removed; remote revocation could not be confirmed."
                }
            }
            _ => "Local OAuth credentials were removed; remote revocation is unsupported or unavailable.",
        };
        let mut result = self.status(record).await;
        result.message = Some(message.into());
        Ok(result)
    }

    /// Refresh *before* a turn and freeze that access token in the returned ephemeral config. No
    /// persisted MCP JSON is changed and no tool request is retried by this service.
    pub async fn prepare(&self, bot_id: &str, resolved: Vec<ResolvedMcpServer>) -> (Vec<ResolvedMcpServer>, Vec<McpStartupFailure>) {
        let mut prepared = Vec::new();
        let mut failures = Vec::new();
        let deadline = tokio::time::Instant::now() + PREPARE_TIMEOUT;
        let previously_managed: std::collections::HashSet<String> =
            self.inner.lock().await.data.entries.iter().filter(|entry| entry.binding.bot == bot_id).map(|entry| entry.binding.name.clone()).collect();
        // A failed reconciliation poisons the vault; bound servers below still fail closed while
        // existing unbound/manual/stdio callers retain their unchanged behavior.
        let _ = self.reconcile().await;
        for mut server in resolved {
            let record = self.store.read(|state| state.mcp_servers_for(bot_id).find(|record| record.name == server.name).cloned());
            let bound = {
                let inner = self.inner.lock().await;
                match record.as_ref() {
                    Some(record) => inner.data.entry(&key(record)).is_some(),
                    None => inner.data.entries.iter().any(|entry| entry.binding.bot == bot_id && entry.binding.name == server.name),
                }
            };
            if !bound {
                if previously_managed.contains(&server.name) && record.as_ref().is_none_or(|record| !self.resolved_current(record, &server)) {
                    failures.push(McpStartupFailure { name: server.name, message: STARTUP_MESSAGE.into() });
                } else {
                    prepared.push(server);
                }
                continue;
            }
            if let Some(record) = record.as_ref()
                && eligible(record).is_err()
            {
                match self.retire_ineligible_transition(record, &server).await {
                    Ok(()) => prepared.push(server),
                    Err(_) => failures.push(McpStartupFailure { name: server.name, message: STARTUP_MESSAGE.into() }),
                }
                continue;
            }
            let result = match record {
                Some(record) => tokio::time::timeout_at(deadline, self.prepare_one(&record, &server)).await.unwrap_or(Err(Error::Timeout)),
                None => Err(Error::Relogin),
            };
            match result {
                Ok(access) => {
                    if let McpServerConfig::Http { headers, .. } = &mut server.config {
                        headers.push(McpPair { name: "Authorization".into(), value: format!("Bearer {}", access.as_str()) });
                        prepared.push(server);
                    } else {
                        failures.push(McpStartupFailure { name: server.name, message: STARTUP_MESSAGE.into() });
                    }
                }
                Err(_) => failures.push(McpStartupFailure { name: server.name, message: STARTUP_MESSAGE.into() }),
            }
        }
        (prepared, failures)
    }

    /// A local saved config replacement starts disabled, and enabling it is separately local-only.
    /// Once that exact replacement is enabled/resolved, do not strand a manual/stdio server behind
    /// an OAuth binding whose UI no longer exists. Retire secrets before allowing the new config;
    /// eligible HTTPS edits deliberately keep the binding and require a new login instead.
    async fn retire_ineligible_transition(&self, record: &McpServerRecord, resolved: &ResolvedMcpServer) -> Result<(), Error> {
        if eligible(record).is_ok() || !self.resolved_current(record, resolved) {
            return Err(Error::Relogin);
        }
        self.reconcile().await.map_err(|_| Error::Failed)?;
        if self.manages(record).await || !self.resolved_current(record, resolved) {
            return Err(Error::Relogin);
        }
        Ok(())
    }

    async fn prepare_one(&self, record: &McpServerRecord, resolved: &ResolvedMcpServer) -> Result<Zeroizing<String>, Error> {
        self.available().map_err(|_| Error::Failed)?;
        let mut stop = self.stop.subscribe();
        let _gate = tokio::select! { _ = cancelled(&mut stop) => return Err(Error::Cancelled), gate = self.refresh_gate.lock() => gate };
        let key = key(record);
        let (entry, generation) = {
            let mut inner = self.inner.lock().await;
            if inner.failed || inner.pending.contains_key(&key) {
                return Err(Error::Relogin);
            }
            let entry = inner.data.entry(&key).cloned().ok_or(Error::Relogin)?;
            if !entry.binding.matches(record) || !self.resolved_current(record, resolved) {
                return Err(Error::Relogin);
            }
            let generation =
                if let Some(generation) = inner.generations.get(&key) { *generation } else { inner.generation(&key).map_err(|_| Error::Failed)? };
            (entry, generation)
        };
        let tokens = entry.tokens.as_ref().ok_or(Error::Relogin)?;
        if tokens.expires_at > self.store.now().timestamp() + REFRESH_EARLY_SECONDS {
            // Setter precedes exposure even after loading a sidecar or a prior logout/rotation.
            let inner = self.inner.lock().await;
            if inner.failed
                || self.closed.load(Ordering::Acquire)
                || !inner.current(&key, generation)
                || inner.pending.contains_key(&key)
                || !self.resolved_current(record, resolved)
            {
                return Err(Error::Relogin);
            }
            self.store.set_external_redaction_keys(inner.data.redaction_keys());
            return Ok(Zeroizing::new(tokens.access.clone()));
        }
        let metadata = entry.metadata.as_ref().ok_or(Error::Relogin)?;
        let client = entry.client_id.as_deref().ok_or(Error::Relogin)?;
        let work = async {
            // Do not send a refresh token to a newly selected or changed issuer/endpoint. Exact
            // discovery is repeated before refresh; no fallback after a mismatched resource.
            let fresh = self.network.discover(&entry.binding.resource).await?;
            if fresh.issuer != metadata.issuer || fresh.token != metadata.token || fresh.public_auth == Some(false) {
                return Err(Error::Relogin);
            }
            {
                let inner = self.inner.lock().await;
                if inner.failed || !inner.current(&key, generation) || inner.pending.contains_key(&key) || !self.binding_current(&entry.binding) {
                    return Err(Error::Relogin);
                }
                self.store.set_external_redaction_keys(inner.data.redaction_keys());
            }
            flow::refresh(self.network.as_ref(), &fresh, client, &entry.binding.resource, tokens, self.store.now().timestamp()).await
        };
        let result = tokio::select! { _ = cancelled(&mut stop) => Err(Error::Cancelled), result = work => result };
        let mut inner = self.inner.lock().await;
        if inner.failed
            || self.closed.load(Ordering::Acquire)
            || !inner.current(&key, generation)
            || inner.pending.contains_key(&key)
            || !self.resolved_current(record, resolved)
        {
            return Err(Error::Relogin);
        }
        match result {
            Ok(tokens) => {
                let access = Zeroizing::new(tokens.access.clone());
                let mut next = inner.data.clone();
                next.retire_tokens(entry.tokens.as_ref().ok_or(Error::Relogin)?);
                next.retire_tokens(&tokens);
                next.entry_mut(&key).ok_or(Error::Relogin)?.tokens = Some(tokens);
                self.commit(&mut inner, next).map_err(|_| Error::Failed)?;
                inner.notices.remove(&key);
                Ok(access)
            }
            Err(error) => {
                inner.notices.insert(key.clone(), error);
                if error == Error::Relogin {
                    let mut next = inner.data.clone();
                    next.retire_tokens(tokens);
                    next.entry_mut(&key).ok_or(Error::Relogin)?.tokens = None;
                    self.commit(&mut inner, next).map_err(|_| Error::Failed)?;
                }
                Err(error)
            }
        }
    }

    /// Only explicit authenticated-resource 401 evidence belongs here. Generic unavailable/tool
    /// errors must not be classified as revocation, and this method never retries a tool call.
    pub async fn report_unauthorized(&self, bot_id: &str, name: &str) {
        // Compatibility for immediate, already-bound evidence. A long-lived/frozen relay MUST
        // use report_unauthorized_token so an older lease cannot invalidate a newer login.
        let access = {
            let inner = self.inner.lock().await;
            inner
                .data
                .entries
                .iter()
                .find(|entry| entry.binding.bot == bot_id && entry.binding.name == name)
                .and_then(|entry| entry.tokens.as_ref())
                .map(|tokens| Zeroizing::new(tokens.access.clone()))
        };
        if let Some(access) = access {
            self.report_unauthorized_token(bot_id, name, &access).await;
        }
    }

    /// Bind explicit 401 evidence to the exact access token frozen into that turn's lease.
    pub async fn report_unauthorized_token(&self, bot_id: &str, name: &str, expected_access: &str) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let record = self.store.read(|state| state.mcp_servers_for(bot_id).find(|record| record.name == name).cloned());
        let Some(record) = record else { return };
        let key = key(&record);
        let mut inner = self.inner.lock().await;
        if inner.failed || inner.pending.contains_key(&key) {
            return;
        }
        let Some(entry) = inner
            .data
            .entry(&key)
            .cloned()
            .filter(|entry| entry.binding.matches(&record) && entry.tokens.as_ref().is_some_and(|tokens| tokens.access == expected_access))
        else {
            return;
        };
        let mut next = inner.data.clone();
        if let Some(tokens) = entry.tokens.as_ref() {
            next.retire_tokens(tokens);
        }
        if let Some(entry) = next.entry_mut(&key) {
            entry.tokens = None;
        }
        if self.commit(&mut inner, next).is_ok() {
            let _ = inner.generation(&key);
            inner.notices.insert(key, Error::Relogin);
        }
    }

    pub async fn shutdown(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.stop.send_replace(true);
        let tasks = {
            let mut inner = self.inner.lock().await;
            for pending in inner.pending.values() {
                let _ = pending.cancel.send(true);
            }
            inner.pending.clear();
            std::mem::take(&mut inner.tasks)
        };
        let mut tasks: Vec<_> = tasks.into_values().collect();
        if tokio::time::timeout(Duration::from_secs(2), futures::future::join_all(tasks.iter_mut())).await.is_err() {
            for task in &tasks {
                task.abort();
            }
            let _ = futures::future::join_all(tasks).await;
        }
    }

    fn available(&self) -> HttpResult<()> {
        if self.closed.load(Ordering::Acquire) { Err(HttpError::unavailable("OAuth service is shut down.")) } else { Ok(()) }
    }
    fn writable(&self, inner: &Inner) -> HttpResult<()> {
        self.available()?;
        if inner.failed { Err(HttpError::unavailable(STORAGE)) } else { Ok(()) }
    }
    fn commit(&self, inner: &mut Inner, next: Data) -> HttpResult<()> {
        if !next.validate() || inner.vault.save(&next).is_err() {
            inner.failed = true;
            // Retain both old and newly-issued secrets even on a failed encrypted save, without
            // making failed credentials available to MCP. No silent discard or plaintext fallback.
            let mut keys = inner.data.redaction_keys();
            keys.extend(next.redaction_keys());
            keys.sort();
            keys.dedup();
            self.store.set_external_redaction_keys(keys);
            return Err(HttpError::unavailable(STORAGE));
        }
        inner.data = next;
        self.store.set_external_redaction_keys(inner.data.redaction_keys());
        Ok(())
    }
    fn record_matches(&self, record: &McpServerRecord) -> bool {
        self.store.read(|state| {
            state.mcp_servers_for(&record.bot_id).any(|current| current.id == record.id && current.config == record.config && current.name == record.name)
        })
    }
    fn binding_current(&self, binding: &Binding) -> bool {
        self.store.read(|state| state.mcp_servers_for(&binding.bot).any(|record| binding.matches(record)))
    }
    fn resolved_current(&self, record: &McpServerRecord, resolved: &ResolvedMcpServer) -> bool {
        if !record.enabled || resolved.name != record.name {
            return false;
        }
        self.store.read(|state| {
            state
                .mcp_servers_for(&record.bot_id)
                .any(|current| current.id == record.id && current.enabled && current.config == record.config && current.name == record.name)
                && state.resolve_mcp_servers(&record.bot_id).0.iter().any(|current| current == resolved)
        })
    }
    async fn attempt_current(&self, binding: &Binding, generation: u64) -> bool {
        let inner = self.inner.lock().await;
        !inner.failed && !self.closed.load(Ordering::Acquire) && inner.pending(&binding.key(), generation) && self.binding_current(binding)
    }
    async fn accept_tokens(&self, binding: &Binding, generation: u64, tokens: Tokens) -> Result<(), Error> {
        let mut inner = self.inner.lock().await;
        if inner.failed || self.closed.load(Ordering::Acquire) || !inner.pending(&binding.key(), generation) || !self.binding_current(binding) {
            return Err(Error::Cancelled);
        }
        let mut next = inner.data.clone();
        next.retire_tokens(&tokens);
        next.entry_mut(&binding.key()).ok_or(Error::Cancelled)?.tokens = Some(tokens);
        self.commit(&mut inner, next).map_err(|_| Error::Failed)?;
        inner.pending.remove(&binding.key());
        inner.notices.remove(&binding.key());
        Ok(())
    }
    async fn finish_error(&self, key: &Key, generation: u64, error: Error) {
        let mut inner = self.inner.lock().await;
        if inner.pending(key, generation) {
            inner.pending.remove(key);
            inner.notices.insert(key.clone(), error);
        }
    }
}

fn key(record: &McpServerRecord) -> Key {
    (record.bot_id.clone(), record.id.clone())
}
fn status(state: &'static str, can_login: bool, message: Option<&'static str>) -> OAuthStatus {
    OAuthStatus { status: state, can_login, message: message.map(str::to_owned) }
}
fn protocol_error(error: Error) -> HttpError {
    HttpError::new(
        if matches!(error, Error::UnsupportedClient | Error::UnsupportedIssuerBinding | Error::ResourceMismatch) {
            400
        } else if error == Error::Cancelled {
            409
        } else {
            502
        },
        error.message(),
    )
}
async fn cancelled(receiver: &mut watch::Receiver<bool>) {
    loop {
        if *receiver.borrow_and_update() {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}
fn eligible(record: &McpServerRecord) -> HttpResult<&str> {
    match &record.config {
        McpServerConfig::Http { url, headers }
            if !headers.iter().any(|pair| pair.name.eq_ignore_ascii_case("authorization")) && HttpsUrl::parse(url, false).is_ok() =>
        {
            Ok(url)
        }
        _ => Err(HttpError::bad_request(UNSUPPORTED)),
    }
}
fn fingerprint(config: &McpServerConfig) -> HttpResult<String> {
    serde_json::to_vec(config).map(|bytes| hex::encode(Sha256::digest(bytes))).map_err(|_| HttpError::bad_request(UNSUPPORTED))
}

fn resource_contains_transport(resource: &str, transport: &str) -> HttpResult<()> {
    let resource = HttpsUrl::parse(resource, false).map_err(|_| HttpError::bad_request(RESOURCE))?;
    let transport = HttpsUrl::parse(transport, false).map_err(|_| HttpError::bad_request(RESOURCE))?;
    let r = resource.as_url();
    let t = transport.as_url();
    // Preserve empty/trailing path segments: /tools// must never broaden to /tools.
    let path = r.path();
    let contained = t.path() == path
        || if path.ends_with('/') { t.path().starts_with(path) } else { t.path().strip_prefix(path).is_some_and(|suffix| suffix.starts_with('/')) };
    // Reject encoded path separators/dot-segments rather than trust a proxy/server's different
    // decoding order. Queries can identify resources: a query-bearing ID must be the exact URL.
    if r.origin() != t.origin()
        || !safe_path(r.path())
        || !safe_path(t.path())
        || !contained
        || (r.query().is_some() && resource.as_str() != transport.as_str())
    {
        return Err(HttpError::bad_request(RESOURCE));
    }
    Ok(())
}
fn safe_path(path: &str) -> bool {
    for segment in path.split('/') {
        let mut decoded = Vec::new();
        let bytes = segment.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'%' {
                if index + 2 >= bytes.len() {
                    return false;
                }
                let Ok(value) = u8::from_str_radix(&segment[index + 1..index + 3], 16) else { return false };
                if matches!(value, b'/' | b'\\' | b'%' | 0..=32 | 127) {
                    return false;
                }
                decoded.push(value);
                index += 3;
            } else {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
        if decoded == b"." || decoded == b".." {
            return false;
        }
    }
    true
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
