use std::collections::VecDeque;
use std::sync::Mutex;

use futures::stream;
use serde_json::{Value, json};

use super::*;

const RESOURCE: &str = "https://mcp.example/tools";
const RESOURCE_METADATA: &str = "https://mcp.example/.well-known/oauth-protected-resource/tools";
const ISSUER: &str = "https://auth.example/tenant";
const AUTH_METADATA: &str = "https://auth.example/.well-known/oauth-authorization-server/tenant";

/// Metadata only: this does not log in, register a client, obtain tokens, or call MCP tools.
#[tokio::test]
#[ignore = "opt-in anonymous HTTPS discovery against the live Notion service"]
async fn live_notion_metadata() {
    let metadata = discover("https://mcp.notion.com").await.unwrap();
    assert_eq!(metadata.resource.resource.as_str(), "https://mcp.notion.com");
    assert_eq!(metadata.authorization_server.issuer.as_str(), "https://mcp.notion.com");
}

struct Fake {
    responses: Mutex<VecDeque<(String, Response)>>,
    visited: Mutex<Vec<String>>,
}

impl Fake {
    fn new(responses: Vec<(&str, Response)>) -> Self {
        Self { responses: Mutex::new(responses.into_iter().map(|(url, response)| (url.to_owned(), response)).collect()), visited: Mutex::default() }
    }

    fn visited(&self) -> Vec<String> {
        self.visited.lock().unwrap().clone()
    }
}

impl Fetch for Fake {
    fn get<'a>(&'a self, url: &'a Url) -> BoxFuture<'a, Result<Response, DiscoveryError>> {
        Box::pin(async move {
            self.visited.lock().unwrap().push(url.to_string());
            let (expected, response) = self.responses.lock().unwrap().pop_front().expect("unexpected metadata request");
            assert_eq!(url.as_str(), expected);
            Ok(response)
        })
    }
}

fn response(status: u16, body: Vec<u8>) -> Response {
    Response { status, json: true, content_length: None, body: stream::iter([Ok(body)]).boxed() }
}

fn document(body: Value) -> Response {
    response(200, serde_json::to_vec(&body).unwrap())
}

fn absent(status: u16) -> Response {
    response(status, Vec::new())
}

fn resource(resource: &str, issuers: &[&str]) -> Value {
    json!({"resource": resource, "authorization_servers": issuers, "scopes_supported": ["tools:read"]})
}

fn authorization(issuer: &str) -> Value {
    json!({
        "issuer": issuer,
        "authorization_endpoint": "https://auth.example/authorize",
        "token_endpoint": "https://auth.example/token",
        "registration_endpoint": "https://auth.example/register",
        "response_types_supported": ["code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": ["tools:read"]
    })
}

async fn discover_fake(resource: &str, fake: &Fake) -> Result<Discovery, DiscoveryError> {
    discover_with(resource, fake, Limits::PRODUCTION).await
}

#[tokio::test]
async fn path_aware_discovery_binds_exact_identities_and_retains_required_metadata() {
    let resource_url = "https://mcp.example/tools?version=2";
    let issuer = "https://auth.example/tenant/";
    let fake = Fake::new(vec![
        ("https://mcp.example/.well-known/oauth-protected-resource/tools?version=2", document(resource(resource_url, &[issuer]))),
        (AUTH_METADATA, document(authorization(issuer))),
    ]);
    let discovered = discover_fake(resource_url, &fake).await.unwrap();
    assert_eq!(discovered.resource.resource.as_str(), resource_url);
    assert_eq!(discovered.authorization_server.issuer.as_str(), issuer);
    assert_eq!(discovered.authorization_server.authorization_endpoint.as_str(), "https://auth.example/authorize");
    assert_eq!(discovered.authorization_server.token_endpoint.as_str(), "https://auth.example/token");
    assert_eq!(discovered.authorization_server.registration_endpoint.unwrap().as_str(), "https://auth.example/register");
    assert_eq!(discovered.resource.scopes_supported, ["tools:read"]);
    assert_eq!(discovered.authorization_server.token_endpoint_auth_methods_supported, Some(vec!["none".into()]));
    assert_eq!(fake.visited().len(), 2, "published endpoints must not be fetched");
}

#[tokio::test]
async fn root_resource_fallback_is_only_for_absence_and_does_not_change_identity() {
    for status in [404, 410] {
        let fake = Fake::new(vec![
            (RESOURCE_METADATA, absent(status)),
            ("https://mcp.example/.well-known/oauth-protected-resource", document(resource(RESOURCE, &[ISSUER]))),
            (AUTH_METADATA, document(authorization(ISSUER))),
        ]);
        assert_eq!(discover_fake(RESOURCE, &fake).await.unwrap().resource.resource.as_str(), RESOURCE);
        let fake = Fake::new(vec![
            (RESOURCE_METADATA, absent(status)),
            ("https://mcp.example/.well-known/oauth-protected-resource", document(resource("https://mcp.example", &[ISSUER]))),
        ]);
        assert_eq!(discover_fake(RESOURCE, &fake).await, Err(DiscoveryError::ResourceMismatch));
        assert_eq!(fake.visited().len(), 2);
    }
}

#[tokio::test]
async fn wrong_resource_is_rejected_before_authorization_server_is_followed() {
    for identity in ["https://mcp.example/other", "https://mcp.example/tools/", "https://MCP.example/tools", "https://mcp.example:443/tools"] {
        let fake = Fake::new(vec![(RESOURCE_METADATA, document(resource(identity, &["https://untrusted.example/sensitive-canary"])))]);
        let error = discover_fake(RESOURCE, &fake).await.unwrap_err();
        assert_eq!(error, DiscoveryError::ResourceMismatch);
        assert_eq!(fake.visited(), [RESOURCE_METADATA]);
        assert!(!format!("{error:?}: {error}").contains("canary"));
    }
}

#[tokio::test]
async fn malformed_or_unrelated_metadata_is_not_a_reason_to_guess_another_endpoint() {
    for body in [
        b"not-json".to_vec(),
        b"{}".to_vec(),
        b"[]".to_vec(),
        br#"{"resource":"https://mcp.example/tools","resource":"other","authorization_servers":[]}"#.to_vec(),
    ] {
        let fake = Fake::new(vec![(RESOURCE_METADATA, response(200, body))]);
        assert_eq!(discover_fake(RESOURCE, &fake).await, Err(DiscoveryError::InvalidMetadata));
        assert_eq!(fake.visited().len(), 1);
    }
    for status in [201, 204, 301, 302, 307, 308, 400, 401, 429, 500] {
        let fake = Fake::new(vec![(RESOURCE_METADATA, response(status, b"private-upstream-error-canary".to_vec()))]);
        let error = discover_fake(RESOURCE, &fake).await.unwrap_err();
        assert_eq!(error, if (300..=399).contains(&status) { DiscoveryError::Redirect } else { DiscoveryError::FetchFailed });
        assert_eq!(fake.visited().len(), 1);
        assert!(!error.to_string().contains("canary"));
    }
    let mut html = response(200, b"<html>private-canary</html>".to_vec());
    html.json = false;
    let fake = Fake::new(vec![(RESOURCE_METADATA, html)]);
    assert_eq!(discover_fake(RESOURCE, &fake).await, Err(DiscoveryError::InvalidMetadata));
}

#[tokio::test]
async fn empty_authorization_server_list_means_profile_unavailable_not_endpoint_guessing() {
    for body in [resource(RESOURCE, &[]), json!({"resource": RESOURCE})] {
        let fake = Fake::new(vec![(RESOURCE_METADATA, document(body))]);
        assert_eq!(discover_fake(RESOURCE, &fake).await, Err(DiscoveryError::MetadataUnavailable));
        assert_eq!(fake.visited().len(), 1);
    }
}

#[tokio::test]
async fn issuer_candidates_follow_rfc8414_then_oidc_without_guessing_tokens() {
    let fake = Fake::new(vec![
        (RESOURCE_METADATA, document(resource(RESOURCE, &[ISSUER]))),
        (AUTH_METADATA, absent(404)),
        ("https://auth.example/.well-known/openid-configuration/tenant", absent(410)),
        ("https://auth.example/tenant/.well-known/openid-configuration", document(authorization(ISSUER))),
    ]);
    assert_eq!(discover_fake(RESOURCE, &fake).await.unwrap().authorization_server.issuer.as_str(), ISSUER);
    assert_eq!(fake.visited().len(), 4);
}

#[tokio::test]
async fn tries_next_advertised_issuer_only_when_previous_metadata_is_absent() {
    let fake = Fake::new(vec![
        (RESOURCE_METADATA, document(resource(RESOURCE, &["https://missing.example", ISSUER]))),
        ("https://missing.example/.well-known/oauth-authorization-server", absent(404)),
        ("https://missing.example/.well-known/openid-configuration", absent(404)),
        (AUTH_METADATA, document(authorization(ISSUER))),
    ]);
    assert_eq!(discover_fake(RESOURCE, &fake).await.unwrap().authorization_server.issuer.as_str(), ISSUER);
    let fake = Fake::new(vec![
        (RESOURCE_METADATA, document(resource(RESOURCE, &[ISSUER, "https://other.example"]))),
        (AUTH_METADATA, document(authorization("https://different.example"))),
    ]);
    assert_eq!(discover_fake(RESOURCE, &fake).await, Err(DiscoveryError::IssuerMismatch));
    assert_eq!(fake.visited().len(), 2);
}

#[tokio::test]
async fn issuer_and_resource_strings_are_not_canonicalized_before_comparison() {
    for issuer in ["https://AUTH.example/tenant", "https://auth.example/tenant/", "https://auth.example:443/tenant"] {
        let fake = Fake::new(vec![(RESOURCE_METADATA, document(resource(RESOURCE, &[ISSUER]))), (AUTH_METADATA, document(authorization(issuer)))]);
        assert_eq!(discover_fake(RESOURCE, &fake).await, Err(DiscoveryError::IssuerMismatch));
    }
    let escaped = br#"{"resource":"https:\/\/mcp.example\/tools","authorization_servers":["https://auth.example/tenant"]}"#.to_vec();
    let fake = Fake::new(vec![(RESOURCE_METADATA, response(200, escaped)), (AUTH_METADATA, document(authorization(ISSUER)))]);
    assert!(discover_fake(RESOURCE, &fake).await.is_ok(), "JSON escaping is decoded before equality");
}

#[tokio::test]
async fn insecure_urls_are_rejected_without_fetching_them() {
    let invalid = [
        "http://mcp.example/tools",
        "ftp://mcp.example/tools",
        "https://user:password-canary@mcp.example/tools",
        "https://@mcp.example/tools",
        "https:///mcp.example/tools",
        "https://mcp.example/tools#",
        "https://mcp.example/tools#token-canary",
        "https://mcp.example:0/tools",
        " https://mcp.example/tools",
        "https://mcp.example/\\tools",
        "https://mcp.example/tools\n",
        "https://mcp.example/%zz",
    ];
    for url in invalid {
        let fake = Fake::new(vec![]);
        assert_eq!(discover_fake(url, &fake).await, Err(DiscoveryError::InvalidUrl));
        assert!(fake.visited().is_empty());
    }
    for url in ["https://127.0.0.1/mcp", "https://10.0.0.1/mcp", "https://[::1]/mcp", "https://[::ffff:127.0.0.1]/mcp"] {
        let fake = Fake::new(vec![]);
        assert_eq!(discover_fake(url, &fake).await, Err(DiscoveryError::BlockedAddress));
        assert!(fake.visited().is_empty());
    }
    for issuer in ["http://auth.example", "https://auth.example/?", "https://auth.example/?tenant=1", "https://auth.example/#"] {
        let fake = Fake::new(vec![(RESOURCE_METADATA, document(resource(RESOURCE, &[issuer])))]);
        assert_eq!(discover_fake(RESOURCE, &fake).await, Err(DiscoveryError::InvalidUrl));
        assert_eq!(fake.visited().len(), 1);
    }
}

#[tokio::test]
async fn required_endpoints_must_be_safe_https_but_unused_extensions_are_ignored() {
    for field in ["authorization_endpoint", "token_endpoint", "registration_endpoint"] {
        for endpoint in ["http://auth.example/sensitive-canary", "https://user:password-canary@auth.example/token", "https://auth.example/token#fragment"] {
            let mut metadata = authorization(ISSUER);
            metadata[field] = json!(endpoint);
            let fake = Fake::new(vec![(RESOURCE_METADATA, document(resource(RESOURCE, &[ISSUER]))), (AUTH_METADATA, document(metadata))]);
            let error = discover_fake(RESOURCE, &fake).await.unwrap_err();
            assert_eq!(error, DiscoveryError::InvalidUrl);
            assert!(!error.to_string().contains("canary"));
        }
    }
    let mut metadata = authorization(ISSUER);
    metadata["unused_extension_uri"] = json!("http://unused.example/documentation");
    metadata["other_extension_endpoint"] = json!({"any": "unknown structure"});
    metadata["signed_metadata"] = json!("ignored-not-consumed");
    let fake = Fake::new(vec![(RESOURCE_METADATA, document(resource(RESOURCE, &[ISSUER]))), (AUTH_METADATA, document(metadata))]);
    assert!(discover_fake(RESOURCE, &fake).await.is_ok());
}

#[tokio::test]
async fn optional_metadata_defaults_only_when_omitted_not_when_null() {
    for field in ["grant_types_supported", "registration_endpoint", "token_endpoint_auth_methods_supported"] {
        let mut metadata = authorization(ISSUER);
        metadata.as_object_mut().unwrap().remove(field);
        let fake = Fake::new(vec![(RESOURCE_METADATA, document(resource(RESOURCE, &[ISSUER]))), (AUTH_METADATA, document(metadata.clone()))]);
        assert!(discover_fake(RESOURCE, &fake).await.is_ok(), "omitted {field}");
        metadata[field] = Value::Null;
        let fake = Fake::new(vec![(RESOURCE_METADATA, document(resource(RESOURCE, &[ISSUER]))), (AUTH_METADATA, document(metadata))]);
        assert_eq!(discover_fake(RESOURCE, &fake).await, Err(DiscoveryError::InvalidMetadata));
        assert_eq!(fake.visited().len(), 2, "null is not a reason to try another location");
    }
    let positional_resource = json!([RESOURCE, [ISSUER], ["tools:read"]]);
    let fake = Fake::new(vec![(RESOURCE_METADATA, document(positional_resource))]);
    assert_eq!(discover_fake(RESOURCE, &fake).await, Err(DiscoveryError::InvalidMetadata));
    let positional_authorization = json!([
        ISSUER,
        "https://auth.example/authorize",
        "https://auth.example/token",
        "https://auth.example/register",
        ["code"],
        null,
        ["S256"],
        [],
        ["none"]
    ]);
    let fake = Fake::new(vec![(RESOURCE_METADATA, document(resource(RESOURCE, &[ISSUER]))), (AUTH_METADATA, document(positional_authorization))]);
    assert_eq!(discover_fake(RESOURCE, &fake).await, Err(DiscoveryError::InvalidMetadata));
}

#[tokio::test]
async fn requires_explicit_s256_and_authorization_code_support() {
    for supported in [json!([]), json!(["plain"]), json!(["s256"]), Value::Null] {
        let mut metadata = authorization(ISSUER);
        if supported.is_null() {
            metadata.as_object_mut().unwrap().remove("code_challenge_methods_supported");
        } else {
            metadata["code_challenge_methods_supported"] = supported;
        }
        let fake = Fake::new(vec![(RESOURCE_METADATA, document(resource(RESOURCE, &[ISSUER]))), (AUTH_METADATA, document(metadata))]);
        assert_eq!(discover_fake(RESOURCE, &fake).await, Err(DiscoveryError::UnsupportedPkce));
    }
    for (field, supported) in [("response_types_supported", json!(["token"])), ("grant_types_supported", json!(["client_credentials"]))] {
        let mut metadata = authorization(ISSUER);
        metadata[field] = supported;
        let fake = Fake::new(vec![(RESOURCE_METADATA, document(resource(RESOURCE, &[ISSUER]))), (AUTH_METADATA, document(metadata))]);
        assert_eq!(discover_fake(RESOURCE, &fake).await, Err(DiscoveryError::UnsupportedGrant));
    }
}

#[tokio::test]
async fn limits_body_size_for_declared_and_chunked_or_misleading_length_responses() {
    let mut declared = response(200, Vec::new());
    declared.content_length = Some(MAX_METADATA_BYTES as u64 + 1);
    let mut chunked = response(200, Vec::new());
    chunked.body = stream::iter([Ok(vec![b' '; MAX_METADATA_BYTES]), Ok(vec![b' '])]).boxed();
    chunked.content_length = Some(1);
    for oversized in [declared, chunked, response(200, vec![b' '; MAX_METADATA_BYTES + 1])] {
        let fake = Fake::new(vec![(RESOURCE_METADATA, oversized)]);
        assert_eq!(discover_fake(RESOURCE, &fake).await, Err(DiscoveryError::MetadataTooLarge));
    }
    let mut body = serde_json::to_vec(&resource(RESOURCE, &[ISSUER])).unwrap();
    body.resize(MAX_METADATA_BYTES, b' ');
    let fake = Fake::new(vec![(RESOURCE_METADATA, response(200, body)), (AUTH_METADATA, document(authorization(ISSUER)))]);
    assert!(discover_fake(RESOURCE, &fake).await.is_ok(), "exact body bound is allowed");
}

#[tokio::test]
async fn limits_servers_values_url_and_candidate_request_count() {
    let issuers = ["https://a.example"; MAX_AUTHORIZATION_SERVERS + 1];
    let fake = Fake::new(vec![(RESOURCE_METADATA, document(resource(RESOURCE, &issuers)))]);
    assert_eq!(discover_fake(RESOURCE, &fake).await, Err(DiscoveryError::LimitExceeded));
    let mut body = resource(RESOURCE, &[ISSUER]);
    body["scopes_supported"] = json!(["x".repeat(MAX_VALUE_BYTES + 1)]);
    let fake = Fake::new(vec![(RESOURCE_METADATA, document(body))]);
    assert_eq!(discover_fake(RESOURCE, &fake).await, Err(DiscoveryError::LimitExceeded));
    let fake = Fake::new(vec![(RESOURCE_METADATA, document(resource(RESOURCE, &[ISSUER])))]);
    let limits = Limits { requests: 1, ..Limits::PRODUCTION };
    assert_eq!(discover_with(RESOURCE, &fake, limits).await, Err(DiscoveryError::LimitExceeded));
    assert_eq!(fake.visited().len(), 1);
    let url = format!("https://mcp.example/{}", "x".repeat(MAX_URL_BYTES));
    let fake = Fake::new(vec![]);
    assert_eq!(discover_fake(&url, &fake).await, Err(DiscoveryError::InvalidUrl));
    assert!(fake.visited().is_empty());
}

struct NeverFetch;
impl Fetch for NeverFetch {
    fn get<'a>(&'a self, _: &'a Url) -> BoxFuture<'a, Result<Response, DiscoveryError>> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn request_and_whole_discovery_deadlines_cover_fetch_and_streaming_body() {
    for limits in
        [Limits { request_timeout: Duration::from_millis(1), ..Limits::PRODUCTION }, Limits { total_timeout: Duration::from_millis(1), ..Limits::PRODUCTION }]
    {
        assert_eq!(discover_with(RESOURCE, &NeverFetch, limits).await, Err(DiscoveryError::Timeout));
        let mut stalled_body = response(200, Vec::new());
        stalled_body.body = stream::pending().boxed();
        let fake = Fake::new(vec![(RESOURCE_METADATA, stalled_body)]);
        assert_eq!(discover_with(RESOURCE, &fake, limits).await, Err(DiscoveryError::Timeout));
    }
}

#[test]
fn location_construction_preserves_resource_path_query_but_strips_issuer_trailing_slash() {
    let resource = Url::parse("https://mcp.example/a%2Fb/?version=2").unwrap();
    let urls = resource_locations(&resource).into_iter().map(|url| url.to_string()).collect::<Vec<_>>();
    assert_eq!(urls, ["https://mcp.example/.well-known/oauth-protected-resource/a%2Fb/?version=2", "https://mcp.example/.well-known/oauth-protected-resource"]);
    let issuer = Url::parse("https://auth.example/tenant/").unwrap();
    let urls = issuer_locations(&issuer).into_iter().map(|url| url.to_string()).collect::<Vec<_>>();
    assert_eq!(
        urls,
        [
            "https://auth.example/.well-known/oauth-authorization-server/tenant",
            "https://auth.example/.well-known/openid-configuration/tenant",
            "https://auth.example/tenant/.well-known/openid-configuration"
        ]
    );
    assert_eq!(resource_locations(&Url::parse("https://mcp.example").unwrap()).len(), 1);
    assert_eq!(issuer_locations(&Url::parse("https://auth.example").unwrap()).len(), 2);
}
