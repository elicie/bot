use super::*;
use futures::{FutureExt, future::BoxFuture};
use serde_json::json;
use std::sync::{
    Mutex as StdMutex,
    atomic::{AtomicI64, AtomicUsize},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

type RecordedRequest = (String, Vec<(String, String)>);

struct Fake {
    metadata: StdMutex<Metadata>,
    replies: StdMutex<VecDeque<Result<flow::Reply, Error>>>,
    requests: StdMutex<Vec<RecordedRequest>>,
    calls: AtomicUsize,
    confidential: AtomicBool,
}
impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            metadata: StdMutex::new(metadata()),
            replies: StdMutex::new(VecDeque::new()),
            requests: StdMutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            confidential: AtomicBool::new(false),
        })
    }
    fn token(&self, access: &str, refresh: Option<&str>, seconds: u64) {
        let mut body = json!({"access_token":access, "token_type":"Bearer", "expires_in":seconds});
        if let Some(refresh) = refresh {
            body["refresh_token"] = refresh.into();
        }
        self.replies.lock().unwrap().push_back(Ok(reply(200, body)));
    }
}
impl Network for Fake {
    fn discover<'a>(&'a self, resource: &'a str) -> BoxFuture<'a, Result<Metadata, Error>> {
        async move {
            assert!(resource.starts_with("https://mcp.example"));
            Ok(self.metadata.lock().unwrap().clone())
        }
        .boxed()
    }
    fn post<'a>(&'a self, endpoint: &'a str, body: flow::Body) -> BoxFuture<'a, Result<flow::Reply, Error>> {
        async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match body {
                flow::Body::Json(bytes) => {
                    assert_eq!(endpoint, "https://issuer.example/register");
                    let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(request["token_endpoint_auth_method"], "none");
                    let mut response =
                        json!({"client_id":"synthetic-public-client", "token_endpoint_auth_method":"none", "redirect_uris":request["redirect_uris"]});
                    if self.confidential.load(Ordering::SeqCst) {
                        response["client_secret"] = "synthetic-client-secret".into();
                    }
                    Ok(reply(201, response))
                }
                flow::Body::Form(pairs) => {
                    self.requests.lock().unwrap().push((endpoint.into(), pairs));
                    self.replies.lock().unwrap().pop_front().expect("unexpected synthetic OAuth request")
                }
            }
        }
        .boxed()
    }
}
fn metadata() -> Metadata {
    Metadata {
        issuer: "https://issuer.example".into(),
        authorization: "https://issuer.example/authorize".into(),
        token: "https://issuer.example/token".into(),
        registration: Some("https://issuer.example/register".into()),
        revocation: None,
        require_iss: true,
        public_auth: Some(true),
    }
}
fn reply(status: u16, body: serde_json::Value) -> flow::Reply {
    flow::Reply { status, json: true, body: Zeroizing::new(serde_json::to_vec(&body).unwrap()) }
}
struct Fixture {
    _root: tempfile::TempDir,
    store: Arc<Store>,
    service: Arc<OAuthService>,
    fake: Arc<Fake>,
    record: McpServerRecord,
    now: Arc<AtomicI64>,
}
impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicI64::new(1_800_000_000));
        let clock = now.clone();
        let store = Store::open(root.path(), Arc::new(move || chrono::DateTime::from_timestamp(clock.load(Ordering::SeqCst), 0).unwrap())).await.unwrap();
        let bot = store.snapshot().bots[0].id.clone();
        store.mutate(|state| state.set_mcp_servers(&bot, r#"{"mcpServers":{"remote":{"url":"https://mcp.example/mcp"}}}"#, true)).await.unwrap();
        let record = store.read(|state| state.mcp_servers_for(&bot).next().unwrap().clone());
        let fake = Fake::new();
        let service = OAuthService::open_with(store.clone(), fake.clone()).await.unwrap();
        Self { _root: root, store, service, fake, record, now }
    }
    async fn enable(&mut self) {
        self.store.mutate(|state| state.set_mcp_server_enabled(&self.record.bot_id, &self.record.name, true, true)).await.unwrap();
        self.record = self.store.read(|state| state.mcp_servers_for(&self.record.bot_id).next().unwrap().clone());
    }
    fn resolved(&self) -> Vec<ResolvedMcpServer> {
        self.store.read(|state| state.resolve_mcp_servers(&self.record.bot_id).0)
    }
    async fn login(&self) -> LoginResponse {
        self.service
            .login(self.record.clone(), LoginInput { client_id: Some("synthetic-manual-client".into()), resource: Some("https://mcp.example".into()) })
            .await
            .unwrap()
    }
    async fn connect(&self, access: &str, refresh: Option<&str>, seconds: u64) {
        self.fake.token(access, refresh, seconds);
        let login = self.login().await;
        send_callback(&login, None).await;
        finish(&self.service).await;
        assert_eq!(self.service.status(&self.record).await.status, "connected");
    }
}
fn callback_url(login: &LoginResponse) -> url::Url {
    let url = url::Url::parse(&login.authorization_url).unwrap();
    let pairs: HashMap<_, _> = url.query_pairs().collect();
    let mut callback = url::Url::parse(&pairs["redirect_uri"]).unwrap();
    callback.query_pairs_mut().extend_pairs([("state", pairs["state"].as_ref()), ("code", "synthetic-code"), ("iss", "https://issuer.example")]);
    callback
}
async fn send_callback(login: &LoginResponse, mutate: Option<fn(&mut url::Url)>) {
    let mut callback = callback_url(login);
    if let Some(mutate) = mutate {
        mutate(&mut callback);
    }
    let host = format!("127.0.0.1:{}", callback.port().unwrap());
    let mut stream = TcpStream::connect(&host).await.unwrap();
    stream.write_all(format!("GET {}?{} HTTP/1.1\r\nHost: {host}\r\n\r\n", callback.path(), callback.query().unwrap()).as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    for forbidden in ["synthetic-code", "state=", "issuer.example"] {
        assert!(!response.contains(forbidden));
    }
}
async fn finish(service: &OAuthService) {
    let tasks = std::mem::take(&mut service.inner.lock().await.tasks);
    for (_, task) in tasks {
        tokio::time::timeout(Duration::from_secs(3), task).await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn manual_login_pkce_ephemeral_injection_and_logout_keep_redaction() {
    let mut f = Fixture::new().await;
    assert_eq!(f.service.status(&f.record).await.status, "not_connected");
    f.connect("synthetic-access-one", Some("synthetic-refresh-one"), 3600).await;
    assert!(!f.store.read(|state| state.mcp_servers_for(&f.record.bot_id).next().unwrap().enabled), "login never enables servers");
    f.enable().await;
    let before = f.store.read(|state| state.mcp_json(&f.record.bot_id).unwrap());
    let (prepared, failed) = f.service.prepare(&f.record.bot_id, f.resolved()).await;
    assert!(failed.is_empty());
    let McpServerConfig::Http { headers, .. } = &prepared[0].config else { panic!() };
    assert_eq!(headers.last().unwrap().value, "Bearer synthetic-access-one");
    assert_eq!(before, f.store.read(|state| state.mcp_json(&f.record.bot_id).unwrap()));
    let keys = f.store.read(|state| state.redaction_keys());
    assert!(keys.contains(&"synthetic-access-one".into()) && keys.contains(&"synthetic-refresh-one".into()));
    {
        let requests = f.fake.requests.lock().unwrap();
        let pairs: HashMap<_, _> = requests[0].1.iter().cloned().collect();
        assert_eq!(pairs["grant_type"], "authorization_code");
        assert_eq!(pairs["code_verifier"].len(), 43);
        assert_eq!(pairs["resource"], "https://mcp.example");
    }
    let logged_out = f.service.logout(&f.record).await.unwrap();
    assert_eq!(logged_out.status, "not_connected");
    assert!(logged_out.message.unwrap().contains("unsupported or unavailable"));
    assert!(!f.service.manages(&f.record).await);
    assert!(f.store.read(|state| state.redaction_keys()).contains(&"synthetic-access-one".into()));
    let state = std::fs::read(f._root.path().join("state.json")).unwrap();
    assert!(!String::from_utf8(state).unwrap().contains("synthetic-access"));
    f.service.shutdown().await;
}

#[tokio::test]
async fn public_dcr_is_accepted_and_confidential_registration_is_rejected() {
    let f = Fixture::new().await;
    f.fake.token("synthetic-dcr-access", None, 3600);
    let login = f.service.login(f.record.clone(), LoginInput::default()).await.unwrap();
    send_callback(&login, None).await;
    finish(&f.service).await;
    assert_eq!(f.service.status(&f.record).await.status, "connected");
    f.service.logout(&f.record).await.unwrap();
    f.fake.confidential.store(true, Ordering::SeqCst);
    let error = f.service.login(f.record.clone(), LoginInput::default()).await.err().unwrap();
    assert!(!error.message.contains("synthetic-client-secret"));
    assert_eq!(error.status, 400);
    assert_eq!(f.service.status(&f.record).await.status, "error");
    f.service.shutdown().await;
}

#[tokio::test]
async fn manual_id_requires_advertised_issuer_binding_but_dcr_unique_redirect_can_proceed() {
    let f = Fixture::new().await;
    f.fake.metadata.lock().unwrap().require_iss = false;
    let error = f.service.login(f.record.clone(), LoginInput { client_id: Some("synthetic-manual-client".into()), resource: None }).await.err().unwrap();
    assert_eq!(error.status, 400);
    assert_eq!(error.message, Error::UnsupportedIssuerBinding.message());
    assert_eq!(f.fake.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.service.status(&f.record).await.status, "error");
    let login = f.service.login(f.record.clone(), LoginInput::default()).await.unwrap();
    assert!(callback_url(&login).path().starts_with("/mcp-oauth/callback/"));
    assert_eq!(f.fake.calls.load(Ordering::SeqCst), 1, "public DCR is not weakened or disabled by manual-client policy");
    f.service.shutdown().await;
}

#[tokio::test]
async fn duplicate_bad_state_and_wrong_issuer_do_not_consume_attempt() {
    let f = Fixture::new().await;
    f.fake.token("synthetic-access", None, 3600);
    let login = f.login().await;
    send_callback(
        &login,
        Some(|url| {
            url.query_pairs_mut().append_pair("state", "duplicate");
        }),
    )
    .await;
    send_callback(
        &login,
        Some(|url| {
            url.query_pairs_mut().append_pair("iss", "https://wrong.example");
        }),
    )
    .await;
    assert_eq!(f.fake.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.service.status(&f.record).await.status, "awaiting_login");
    send_callback(&login, None).await;
    finish(&f.service).await;
    assert_eq!(f.fake.calls.load(Ordering::SeqCst), 1);
    let callback = callback_url(&login);
    assert!(TcpStream::connect(("127.0.0.1", callback.port().unwrap())).await.is_err());
    f.service.shutdown().await;
}

#[tokio::test]
async fn logout_cancel_replacement_and_shutdown_close_loopback_listeners() {
    let f = Fixture::new().await;
    let first = f.login().await;
    let first_url = callback_url(&first);
    let second = f.login().await;
    tokio::task::yield_now().await;
    assert!(TcpStream::connect(("127.0.0.1", first_url.port().unwrap())).await.is_err());
    let second_url = callback_url(&second);
    f.service.logout(&f.record).await.unwrap();
    finish(&f.service).await;
    assert!(TcpStream::connect(("127.0.0.1", second_url.port().unwrap())).await.is_err());
    let third = f.login().await;
    let third_url = callback_url(&third);
    f.service.shutdown().await;
    assert!(TcpStream::connect(("127.0.0.1", third_url.port().unwrap())).await.is_err());
    assert_eq!(f.service.status(&f.record).await.status, "unavailable");
    assert!(f.service.login(f.record.clone(), LoginInput::default()).await.is_err());
}

#[tokio::test]
async fn stale_callback_config_and_deleted_record_never_receive_or_inject_tokens() {
    for deleted in [false, true] {
        let mut f = Fixture::new().await;
        let login = f.login().await;
        f.store
            .mutate(|state| {
                state.set_mcp_servers(
                    &f.record.bot_id,
                    if deleted { r#"{"mcpServers":{}}"# } else { r#"{"mcpServers":{"remote":{"url":"https://mcp.example/other"}}}"# },
                    true,
                )
            })
            .await
            .unwrap();
        send_callback(&login, None).await;
        finish(&f.service).await;
        assert_eq!(f.fake.calls.load(Ordering::SeqCst), 0);
        if !deleted {
            f.record = f.store.read(|state| state.mcp_servers_for(&f.record.bot_id).next().unwrap().clone());
            f.enable().await;
            let (prepared, failures) = f.service.prepare(&f.record.bot_id, f.resolved()).await;
            assert!(prepared.is_empty());
            assert_eq!(failures[0].message, STARTUP_MESSAGE);
        }
        f.service.shutdown().await;
    }
}

#[tokio::test]
async fn refresh_rotation_is_pre_turn_frozen_and_relogin_errors_remove_active_tokens() {
    let mut f = Fixture::new().await;
    f.enable().await;
    f.connect("synthetic-access-old", Some("synthetic-refresh-old"), 30).await;
    f.fake.token("synthetic-access-new", Some("synthetic-refresh-new"), 3600);
    let (prepared, failed) = f.service.prepare(&f.record.bot_id, f.resolved()).await;
    assert!(failed.is_empty());
    let McpServerConfig::Http { headers, .. } = &prepared[0].config else { panic!() };
    assert_eq!(headers[0].value, "Bearer synthetic-access-new");
    let keys = f.store.read(|state| state.redaction_keys());
    for value in ["synthetic-access-old", "synthetic-refresh-old", "synthetic-access-new", "synthetic-refresh-new"] {
        assert!(keys.contains(&value.into()));
    }
    {
        let requests = f.fake.requests.lock().unwrap();
        let pairs: HashMap<_, _> = requests[1].1.iter().cloned().collect();
        assert_eq!(pairs["grant_type"], "refresh_token");
        assert_eq!(pairs["refresh_token"], "synthetic-refresh-old");
    }
    f.service.report_unauthorized_token(&f.record.bot_id, &f.record.name, "synthetic-access-old").await;
    assert_eq!(f.service.status(&f.record).await.status, "connected", "delayed old-lease401 must not invalidate the rotated credential");
    f.now.fetch_add(3600, Ordering::SeqCst);
    f.fake.replies.lock().unwrap().push_back(Ok(reply(400, json!({"error":"invalid_grant","error_description":"private-canary"}))));
    let (prepared, failed) = f.service.prepare(&f.record.bot_id, f.resolved()).await;
    assert!(prepared.is_empty() && failed.len() == 1);
    let status = f.service.status(&f.record).await;
    assert_eq!(status.status, "login_required");
    assert!(!serde_json::to_string(&status).unwrap().contains("canary"));
    f.service.shutdown().await;
}

#[tokio::test]
async fn changed_refresh_issuer_never_receives_refresh_secret() {
    let mut f = Fixture::new().await;
    f.enable().await;
    f.connect("synthetic-access", Some("synthetic-refresh"), 10).await;
    f.fake.metadata.lock().unwrap().token = "https://other.example/token".into();
    let (prepared, failed) = f.service.prepare(&f.record.bot_id, f.resolved()).await;
    assert!(prepared.is_empty() && failed.len() == 1);
    assert_eq!(f.fake.calls.load(Ordering::SeqCst), 1, "only initial authorization-code exchange was sent");
    assert_eq!(f.service.status(&f.record).await.status, "login_required");
    f.service.shutdown().await;
}

#[tokio::test]
async fn explicit_unauthorized_evidence_requires_relogin_and_redaction_survives_reopen() {
    let mut f = Fixture::new().await;
    f.enable().await;
    f.connect("synthetic-retired-access", Some("synthetic-retired-refresh"), 3600).await;
    f.service.report_unauthorized_token(&f.record.bot_id, &f.record.name, "synthetic-retired-access").await;
    assert_eq!(f.service.status(&f.record).await.status, "login_required");
    let (prepared, failed) = f.service.prepare(&f.record.bot_id, f.resolved()).await;
    assert!(prepared.is_empty() && failed.len() == 1);
    f.service.logout(&f.record).await.unwrap();
    f.service.shutdown().await;
    let store = f.store.clone();
    let record = f.record.clone();
    let fake = f.fake.clone();
    let service = f.service;
    drop(service);
    let reopened = OAuthService::open_with(store.clone(), fake).await.unwrap();
    assert_eq!(reopened.status(&record).await.status, "not_connected");
    let keys = store.read(|state| state.redaction_keys());
    assert!(keys.contains(&"synthetic-retired-access".into()) && keys.contains(&"synthetic-retired-refresh".into()));
    reopened.shutdown().await;
}

#[tokio::test]
async fn unbound_manual_static_and_stdio_are_unchanged_and_manual_auth_is_never_overridden() {
    let mut f = Fixture::new().await;
    f.enable().await;
    let servers = vec![
        ResolvedMcpServer { name: "stdio".into(), config: McpServerConfig::Stdio { command: "synthetic-command".into(), args: vec![], env: vec![] } },
        ResolvedMcpServer {
            name: "manual".into(),
            config: McpServerConfig::Http {
                url: "http://127.0.0.1:12345".into(),
                headers: vec![McpPair { name: "Authorization".into(), value: "Bearer synthetic-manual".into() }],
            },
        },
    ];
    let (prepared, failed) = f.service.prepare(&f.record.bot_id, servers.clone()).await;
    assert_eq!(prepared, servers);
    assert!(failed.is_empty());
    let mut manual = f.record.clone();
    if let McpServerConfig::Http { headers, .. } = &mut manual.config {
        headers.push(McpPair { name: "aUtHoRiZaTiOn".into(), value: "synthetic-canary".into() });
    }
    let error = f.service.login(manual, LoginInput::default()).await.err().unwrap();
    assert_eq!(error.message, UNSUPPORTED);
    assert_eq!(f.fake.calls.load(Ordering::SeqCst), 0);
    f.service.shutdown().await;
}

#[test]
fn resource_scope_requires_same_origin_segment_containment_and_exact_query_identity() {
    for (resource, transport) in [
        ("https://mcp.example", "https://mcp.example/mcp"),
        ("https://mcp.example/tools", "https://mcp.example/tools/v1"),
        ("https://mcp.example/tools/", "https://mcp.example/tools/v1"),
        ("https://mcp.example/tools//", "https://mcp.example/tools//v1"),
        ("https://mcp.example/tools//", "https://mcp.example/tools//"),
        ("https://mcp.example/tools?version=1", "https://mcp.example/tools?version=1"),
    ] {
        assert!(resource_contains_transport(resource, transport).is_ok());
    }
    for (resource, transport) in [
        ("https://other.example", "https://mcp.example/mcp"),
        ("https://mcp.example/tools", "https://mcp.example/toolsmith"),
        ("https://mcp.example/tools//", "https://mcp.example/tools/v1"),
        ("https://mcp.example/tools//", "https://mcp.example/tools/"),
        ("https://mcp.example/tools/", "https://mcp.example/tools"),
        ("https://mcp.example/tools?version=1", "https://mcp.example/tools?version=2"),
        ("https://mcp.example/tools", "https://mcp.example/tools%2fv1"),
        ("https://mcp.example", "https://mcp.example/a%255cb"),
    ] {
        assert_eq!(resource_contains_transport(resource, transport).unwrap_err().message, RESOURCE);
    }
    assert!(serde_json::from_str::<LoginInput>(r#"{"clientId":"x","secret":"forbidden"}"#).is_err());
    assert!(serde_json::from_str::<LoginInput>(r#"{"clientId":"x","clientId":"y"}"#).is_err());
}
