//! Private synthetic fixtures for the integrated runtime test. Loaded only as a `cfg(test)` child
//! of the relay, so neither fake OAuth networking nor fake upstream transport exists in production.
//! This is not acceptance against a real OAuth server, account, metadata endpoint, or registration.

use super::*;

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

use grok_bot_model::McpServerRecord;
use grok_bot_store::Store;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroize::Zeroizing;

use crate::mcp_oauth::{LoginInput, OAuthService, flow};

// Deliberately overlap ordinary tool-name suffixes (`search_tool`, `todo_write`). Native engine
// function names overwrite, rather than append: withholding their suffix would corrupt tool calls.
pub(crate) const ACCESS: &str = "tool-secret-credential";
pub(crate) const REFRESH: &str = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJzeW50aGV0aWMifQ.synthetic-signature";
pub(crate) const REMOTE: &str = "https://runtime-oauth.example/mcp";
const ISSUER: &str = "https://runtime-issuer.example";
const CLIENT: &str = "synthetic-runtime-public-client";
const CODE: &str = "synthetic-runtime-authorization-code";
const WAIT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct OAuthNetwork {
    verifier: Mutex<Option<String>>,
    exchanges: AtomicUsize,
}

impl flow::Network for OAuthNetwork {
    fn discover<'a>(&'a self, resource: &'a str) -> BoxFuture<'a, Result<flow::Metadata, flow::Error>> {
        Box::pin(async move {
            assert!(resource == REMOTE, "only the synthetic resource may be discovered");
            Ok(flow::Metadata {
                issuer: ISSUER.into(),
                authorization: format!("{ISSUER}/authorize"),
                token: format!("{ISSUER}/token"),
                registration: None,
                revocation: None,
                require_iss: true,
                public_auth: Some(true),
            })
        })
    }

    fn post<'a>(&'a self, endpoint: &'a str, body: flow::Body) -> BoxFuture<'a, Result<flow::Reply, flow::Error>> {
        Box::pin(async move {
            assert!(endpoint == format!("{ISSUER}/token"));
            let flow::Body::Form(pairs) = body else { panic!("the synthetic manual client does not register") };
            let pairs: HashMap<_, _> = pairs.into_iter().collect();
            assert!(pairs.get("grant_type").is_some_and(|value| value == "authorization_code"));
            assert!(pairs.get("client_id").is_some_and(|value| value == CLIENT));
            assert!(pairs.get("resource").is_some_and(|value| value == REMOTE));
            assert!(pairs.get("code").is_some_and(|value| value == CODE));
            let redirect = Url::parse(pairs.get("redirect_uri").unwrap()).unwrap();
            assert!(redirect.scheme() == "http" && redirect.host_str() == Some("127.0.0.1"));
            assert!(redirect.path() == "/mcp-oauth/callback" && redirect.port().is_some());
            let verifier = pairs.get("code_verifier").unwrap();
            assert!(verifier.len() == 43);
            *self.verifier.lock().unwrap() = Some(verifier.clone());
            self.exchanges.fetch_add(1, Ordering::SeqCst);
            Ok(flow::Reply {
                status: 200,
                json: true,
                body: Zeroizing::new(
                    serde_json::to_vec(&json!({
                        "access_token": ACCESS, "refresh_token": REFRESH,
                        "token_type": "Bearer", "expires_in": 3600,
                    }))
                    .unwrap(),
                ),
            })
        })
    }
}

/// Perform the real service login and real TCP loopback callback, not a pre-seeded bearer/vault.
/// Only the private discovery/token-request network is fake; PKCE, issuer/state checking, encrypted
/// commit, redaction registration and the public connected status are the application's real code.
pub(crate) async fn connected_service(store: Arc<Store>, record: &McpServerRecord) -> Arc<OAuthService> {
    let network = Arc::new(OAuthNetwork::default());
    let service = OAuthService::open_for_test(store, network.clone()).await.unwrap();
    let login = service.login(record.clone(), LoginInput { client_id: Some(CLIENT.into()), resource: None }).await.unwrap();
    let authorization = Url::parse(&login.authorization_url).unwrap();
    assert!(authorization.origin().ascii_serialization() == ISSUER);
    let pairs: HashMap<_, _> = authorization.query_pairs().into_owned().collect();
    assert!(pairs.get("code_challenge_method").is_some_and(|value| value == "S256"));
    let mut callback = Url::parse(&pairs["redirect_uri"]).unwrap();
    callback.query_pairs_mut().extend_pairs([("state", pairs["state"].as_str()), ("code", CODE), ("iss", ISSUER)]);
    let host = format!("127.0.0.1:{}", callback.port().unwrap());
    let mut socket = TcpStream::connect(&host).await.unwrap();
    socket.write_all(format!("GET {}?{} HTTP/1.1\r\nHost: {host}\r\n\r\n", callback.path(), callback.query().unwrap()).as_bytes()).await.unwrap();
    let mut response = String::new();
    tokio::time::timeout(WAIT, socket.read_to_string(&mut response)).await.unwrap().unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!([ACCESS, REFRESH, CODE, pairs["state"].as_str()].iter().all(|secret| !response.contains(secret)));
    tokio::time::timeout(WAIT, async {
        loop {
            match service.status(record).await.status {
                "connected" => break,
                "awaiting_login" => tokio::time::sleep(Duration::from_millis(5)).await,
                _ => panic!("synthetic loopback OAuth login did not connect"),
            }
        }
    })
    .await
    .unwrap();
    assert!(network.exchanges.load(Ordering::SeqCst) == 1);
    let verifier = network.verifier.lock().unwrap();
    assert!(pairs["code_challenge"] == flow::pkce_challenge(verifier.as_ref().unwrap()));
    service
}

#[derive(Default)]
pub(crate) struct Upstream {
    authorization: Mutex<Vec<String>>,
    methods: Mutex<Vec<String>>,
    calls: Mutex<Vec<Value>>,
    hold_next: AtomicBool,
    unauthorized_next: AtomicBool,
    holding: watch::Sender<bool>,
    dropped_forwards: Arc<AtomicUsize>,
}

impl Upstream {
    pub(crate) async fn start(self: &Arc<Self>, remote: &str, headers: Vec<McpPair>, keys: Vec<String>, on_unauthorized: Unauthorized) -> HttpResult<Lease> {
        let upstream = self.clone();
        let fetch: TestFetch = Arc::new(move |request| {
            let upstream = upstream.clone();
            Box::pin(async move { upstream.fetch(request).await })
        });
        // Same real listener/authentication/forwarding/JSON+SSE redaction/cancellation as production.
        start_inner(remote, headers, keys, on_unauthorized, Some(fetch)).await
    }

    async fn fetch(self: &Arc<Self>, request: UpstreamRequest) -> Result<UpstreamResponse, Fault> {
        assert!(request.url.as_str() == REMOTE, "the remote endpoint remains frozen");
        let authorization = request.headers.get(header::AUTHORIZATION).unwrap().to_str().unwrap().to_owned();
        assert!(authorization == format!("Bearer {ACCESS}"), "upstream sees the exact OAuth access token, never the local lease");
        assert!(!authorization.contains(REFRESH));
        self.authorization.lock().unwrap().push(authorization);
        if self.unauthorized_next.swap(false, Ordering::SeqCst) {
            return Ok(reply(StatusCode::UNAUTHORIZED, "application/json", vec![json!({"error": ACCESS}).to_string().into_bytes()]));
        }
        if request.method != Method::POST {
            return Ok(reply(StatusCode::METHOD_NOT_ALLOWED, "application/json", vec![]));
        }
        let request: Value = serde_json::from_slice(&request.body).unwrap();
        let method = request["method"].as_str().unwrap();
        self.methods.lock().unwrap().push(method.into());
        let Some(id) = request.get("id").cloned() else {
            return Ok(reply(StatusCode::ACCEPTED, "application/json", vec![]));
        };
        let result = match method {
            "initialize" => json!({
                "protocolVersion": request["params"]["protocolVersion"],
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": format!("synthetic-oauth {ACCESS}"), "version": "1.0.0"},
            }),
            "ping" => json!({}),
            "tools/list" => json!({"tools": [{
                "name": "lookup", "description": format!("Look up a synthetic fixture. Echo: {ACCESS}"),
                "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"], "additionalProperties": false},
            }]}),
            "tools/call" => {
                assert!(request["params"]["name"] == "lookup");
                self.calls.lock().unwrap().push(request["params"].clone());
                if self.hold_next.swap(false, Ordering::SeqCst) {
                    let _guard = DroppedForward(self.dropped_forwards.clone());
                    self.holding.send_replace(true);
                    futures::future::pending::<()>().await;
                    unreachable!();
                }
                json!({"content": [{"type": "text", "text": format!("oauth-tool-ok:{} token={ACCESS}", request["params"]["arguments"]["query"].as_str().unwrap())}], "isError": false})
            }
            _ => {
                return Ok(reply(
                    StatusCode::OK,
                    "application/json",
                    vec![
                        json!({
                            "jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"},
                        })
                        .to_string()
                        .into_bytes(),
                    ],
                ));
            }
        };
        let bytes = escaped_json(json!({"jsonrpc": "2.0", "id": id, "result": result}));
        if method == "tools/call" {
            let mut event = b"event: message\ndata: ".to_vec();
            event.extend_from_slice(&bytes);
            event.extend_from_slice(b"\n\n");
            // The response is real MCP-over-SSE, with an escaped token split across byte chunks.
            Ok(reply(StatusCode::OK, "text/event-stream", event.chunks(17).map(<[u8]>::to_vec).collect()))
        } else {
            Ok(reply(StatusCode::OK, "application/json", vec![bytes]))
        }
    }

    pub(crate) fn assert_not_called(&self) {
        assert!(self.calls.lock().unwrap().is_empty(), "the real upstream tool must wait for approval");
    }

    pub(crate) fn assert_used(&self) {
        let methods = self.methods.lock().unwrap();
        for expected in ["initialize", "notifications/initialized", "tools/list", "tools/call"] {
            assert!(methods.iter().any(|method| method == expected), "the real engine must drive the MCP lifecycle");
        }
        assert!(!self.authorization.lock().unwrap().is_empty());
        let calls = self.calls.lock().unwrap();
        assert!(calls.len() == 1 && calls[0]["arguments"]["query"] == "alpha", "the approved synthetic tool really ran once");
    }

    pub(crate) fn hold_next_call(&self) {
        self.holding.send_replace(false);
        self.hold_next.store(true, Ordering::SeqCst);
    }

    pub(crate) async fn wait_holding(&self) {
        let mut receiver = self.holding.subscribe();
        tokio::time::timeout(WAIT, receiver.wait_for(|holding| *holding)).await.unwrap().unwrap();
    }

    pub(crate) fn assert_forward_dropped(&self) {
        assert!(self.dropped_forwards.load(Ordering::SeqCst) == 1, "cancellation drops the live upstream forward");
    }

    pub(crate) fn unauthorized_next(&self) {
        self.unauthorized_next.store(true, Ordering::SeqCst);
    }
}

struct DroppedForward(Arc<AtomicUsize>);
impl Drop for DroppedForward {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn escaped_json(value: Value) -> Vec<u8> {
    // Ordinary string replacement alone cannot sanitize this encoded token; JSON parsing is needed.
    serde_json::to_string(&value).unwrap().replace(ACCESS, &format!("\\u{:04x}{}", ACCESS.as_bytes()[0], &ACCESS[1..])).into_bytes()
}

fn reply(status: StatusCode, content_type: &'static str, chunks: Vec<Vec<u8>>) -> UpstreamResponse {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    UpstreamResponse { status, headers, body: futures::stream::iter(chunks.into_iter().map(Ok)).boxed() }
}
