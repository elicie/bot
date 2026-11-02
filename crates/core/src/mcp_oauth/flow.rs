//! Private OAuth protocol machinery. Production has no injectable HTTP/local-network exception.
//! Request/response errors are fixed diagnostics; external bodies, URLs and secrets are never errors.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures::{FutureExt, StreamExt, future::BoxFuture, stream};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;
use zeroize::Zeroizing;

use super::{DiscoveryError, Fetch, HttpsUrl, MAX_METADATA_BYTES, Response, fetch, is_global_ip};

pub(super) const MAX_BODY: usize = 64 * 1024;
pub(super) const MAX_SECRET: usize = 8192;
pub(super) const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(180);
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_DNS: usize = 64;
const MAX_CALLBACK: usize = 16 * 1024;
const MAX_CALLBACK_REQUESTS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Error {
    Failed,
    Invalid,
    Relogin,
    UnsupportedClient,
    UnsupportedIssuerBinding,
    ResourceMismatch,
    Callback,
    Cancelled,
    Timeout,
}

impl Error {
    pub(super) fn message(self) -> &'static str {
        match self {
            Self::Failed => "OAuth could not complete a safe request. Try logging in again.",
            Self::Invalid => "OAuth returned an invalid or oversized response. Try logging in again.",
            Self::Relogin => "OAuth authorization has expired or was revoked. Log in again.",
            Self::UnsupportedClient => "OAuth requires a public client: enter a public client ID or use supported public registration.",
            Self::UnsupportedIssuerBinding => {
                "Manual OAuth clients require advertised RFC9207 issuer-bound authorization responses. Use supported public registration instead."
            }
            Self::ResourceMismatch => {
                "OAuth discovery did not match this resource. Enter its exact advertised resource identifier without changing the MCP transport URL."
            }
            Self::Callback => "OAuth login did not receive a valid bound callback. Log in again.",
            Self::Cancelled => "OAuth login was cancelled. Log in again when ready.",
            Self::Timeout => "OAuth login exceeded its time limit. Log in again.",
        }
    }
}

/// Metadata, never credentials. Stored only inside the encrypted service sidecar when bound.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Metadata {
    pub issuer: String,
    pub authorization: String,
    pub token: String,
    pub registration: Option<String>,
    pub revocation: Option<String>,
    pub require_iss: bool,
    pub public_auth: Option<bool>,
}

impl Metadata {
    pub(super) fn valid(&self) -> bool {
        HttpsUrl::parse(&self.issuer, true).is_ok()
            && [&self.authorization, &self.token].into_iter().all(|value| HttpsUrl::parse(value, false).is_ok())
            && [&self.registration, &self.revocation].into_iter().flatten().all(|value| HttpsUrl::parse(value, false).is_ok())
    }
}

pub(super) enum Body {
    Form(Vec<(String, String)>),
    Json(Vec<u8>),
}

pub(super) struct Reply {
    pub status: u16,
    pub json: bool,
    pub body: Zeroizing<Vec<u8>>,
}

/// Private protocol seam. Only the module's deterministic tests can supply a fake implementation.
pub(super) trait Network: Send + Sync {
    fn discover<'a>(&'a self, resource: &'a str) -> BoxFuture<'a, Result<Metadata, Error>>;
    fn post<'a>(&'a self, endpoint: &'a str, body: Body) -> BoxFuture<'a, Result<Reply, Error>>;
}

pub(super) struct Production;

impl Network for Production {
    fn discover<'a>(&'a self, resource: &'a str) -> BoxFuture<'a, Result<Metadata, Error>> {
        async move {
            // Reuse strict discovery unchanged. Capture its already-bounded anonymous documents so
            // optional RFC9207/revocation fields come from the *same* validated issuer response,
            // not an extra request or an unadvertised authorization server.
            let capture = Capture(Mutex::new(Vec::new()));
            let found = super::discover_with(resource, &capture, super::Limits::PRODUCTION)
                .await
                .map_err(|error| if error == DiscoveryError::ResourceMismatch { Error::ResourceMismatch } else { Error::Failed })?;
            let selected = &found.authorization_server;
            let documents = capture.0.into_inner().map_err(|_| Error::Failed)?;
            for body in documents {
                if super::validate_authorization_server(&body, &selected.issuer).as_ref() != Ok(selected) {
                    continue;
                }
                let extra: ExtraMetadata = json(&body)?;
                let revocation =
                    extra.revocation_endpoint.map(|endpoint| HttpsUrl::parse(&endpoint, false).map(|_| endpoint).map_err(|_| Error::Invalid)).transpose()?;
                let public_auth = selected.token_endpoint_auth_methods_supported.as_ref().map(|methods| methods.iter().any(|method| method == "none"));
                let can_revoke = extra
                    .revocation_endpoint_auth_methods_supported
                    .as_ref()
                    .map_or(public_auth == Some(true), |methods| methods.len() <= 128 && methods.iter().any(|method| method == "none"));
                return Ok(Metadata {
                    issuer: selected.issuer.as_str().into(),
                    authorization: selected.authorization_endpoint.as_str().into(),
                    token: selected.token_endpoint.as_str().into(),
                    registration: selected.registration_endpoint.as_ref().map(|url| url.as_str().into()),
                    revocation: if can_revoke { revocation } else { None },
                    require_iss: extra.authorization_response_iss_parameter_supported,
                    public_auth,
                });
            }
            Err(Error::Invalid)
        }
        .boxed()
    }

    fn post<'a>(&'a self, endpoint: &'a str, body: Body) -> BoxFuture<'a, Result<Reply, Error>> {
        async move {
            tokio::time::timeout(HTTP_TIMEOUT, async {
                let url = HttpsUrl::parse(endpoint, false).map_err(|_| Error::Invalid)?;
                let client = public_client(url.as_url()).await?;
                let request = client.post(url.as_url().clone()).header(ACCEPT, "application/json");
                let response = match body {
                    Body::Form(pairs) => {
                        let mut encoded = Zeroizing::new(String::new());
                        encoded.push_str(
                            &url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs.iter().map(|(key, value)| (key, value))).finish(),
                        );
                        request.header(CONTENT_TYPE, "application/x-www-form-urlencoded").body(encoded.as_bytes().to_vec()).send().await
                    }
                    Body::Json(bytes) => request.header(CONTENT_TYPE, "application/json").body(bytes).send().await,
                }
                .map_err(|_| Error::Failed)?;
                let status = response.status().as_u16();
                if (300..400).contains(&status) {
                    return Err(Error::Failed);
                }
                let json = response
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value.split(';').next().is_some_and(|kind| kind.trim().eq_ignore_ascii_case("application/json")));
                if response.content_length().is_some_and(|size| size > MAX_BODY as u64) {
                    return Err(Error::Invalid);
                }
                let mut stream = response.bytes_stream();
                let mut body = Zeroizing::new(Vec::new());
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(|_| Error::Failed)?;
                    if chunk.len() > MAX_BODY - body.len() {
                        return Err(Error::Invalid);
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(Reply { status, json, body })
            })
            .await
            .map_err(|_| Error::Timeout)?
        }
        .boxed()
    }
}

struct Capture(Mutex<Vec<Vec<u8>>>);
impl Fetch for Capture {
    fn get<'a>(&'a self, url: &'a Url) -> BoxFuture<'a, Result<Response, DiscoveryError>> {
        async move {
            let mut response = fetch::Production.get(url).await?;
            // Discovery's outer request timeout covers DNS, headers, this read and validation.
            if response.status != 200 || !response.json {
                return Ok(response);
            }
            if response.content_length.is_some_and(|size| size > MAX_METADATA_BYTES as u64) {
                return Err(DiscoveryError::MetadataTooLarge);
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.body.next().await {
                let chunk = chunk?;
                if chunk.len() > MAX_METADATA_BYTES - body.len() {
                    return Err(DiscoveryError::MetadataTooLarge);
                }
                body.extend_from_slice(&chunk);
            }
            let mut documents = self.0.lock().map_err(|_| DiscoveryError::FetchFailed)?;
            if documents.len() >= super::MAX_REQUESTS {
                return Err(DiscoveryError::LimitExceeded);
            }
            documents.push(body.clone());
            response.body = stream::iter([Ok(body)]).boxed();
            Ok(response)
        }
        .boxed()
    }
}

#[derive(Deserialize)]
struct ExtraMetadata {
    #[serde(default)]
    revocation_endpoint: Option<String>,
    #[serde(default)]
    revocation_endpoint_auth_methods_supported: Option<Vec<String>>,
    #[serde(default)]
    authorization_response_iss_parameter_supported: bool,
}

async fn public_client(url: &Url) -> Result<reqwest::Client, Error> {
    let host = url.host_str().ok_or(Error::Invalid)?;
    let port = url.port_or_known_default().ok_or(Error::Invalid)?;
    let addresses = match url.host() {
        Some(url::Host::Ipv4(ip)) => vec![SocketAddr::new(IpAddr::V4(ip), port)],
        Some(url::Host::Ipv6(ip)) => vec![SocketAddr::new(IpAddr::V6(ip), port)],
        Some(url::Host::Domain(_)) => tokio::net::lookup_host((host, port)).await.map_err(|_| Error::Failed)?.take(MAX_DNS + 1).collect(),
        None => return Err(Error::Invalid),
    };
    if !public_addresses(&addresses) {
        return Err(Error::Failed);
    }
    reqwest::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(HTTP_TIMEOUT)
        .connect_timeout(HTTP_TIMEOUT)
        .resolve_to_addrs(host, &addresses)
        .build()
        .map_err(|_| Error::Failed)
}

fn public_addresses(addresses: &[SocketAddr]) -> bool {
    !addresses.is_empty() && addresses.len() <= MAX_DNS && addresses.iter().all(|address| is_global_ip(address.ip()))
}

pub(super) fn random() -> Result<String, Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| Error::Failed)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub(super) fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub(super) fn client_id_valid(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

pub(super) fn secret_valid(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_SECRET && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

pub(super) fn credential_valid(value: &str) -> bool {
    // Like the existing manual vault, reject trivial values which would redact ordinary text and
    // protocol strings globally. Authorization codes use secret_valid independently.
    value.len() >= 8 && secret_valid(value)
}

pub(super) fn bearer_valid(value: &str) -> bool {
    credential_valid(value) && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-._~+/=".contains(&byte))
}

pub(super) async fn register(network: &dyn Network, metadata: &Metadata, redirect: &str) -> Result<String, Error> {
    if metadata.public_auth == Some(false) {
        return Err(Error::UnsupportedClient);
    }
    let endpoint = metadata.registration.as_deref().ok_or(Error::UnsupportedClient)?;
    let request = serde_json::to_vec(&serde_json::json!({
        "client_name": "Grok Bot MCP OAuth",
        "redirect_uris": [redirect],
        "token_endpoint_auth_method": "none",
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"]
    }))
    .map_err(|_| Error::Invalid)?;
    let reply = network.post(endpoint, Body::Json(request)).await?;
    if !matches!(reply.status, 200 | 201) || !reply.json || reply.body.len() > MAX_BODY {
        return Err(Error::UnsupportedClient);
    }
    let registered: Registration = json(&reply.body)?;
    if !client_id_valid(&registered.client_id)
        || registered.token_endpoint_auth_method.as_deref() != Some("none")
        || registered.client_secret.is_some()
        || registered.redirect_uris != [redirect]
        || registered.grant_types.as_ref().is_some_and(|grants| !grants.iter().any(|grant| grant == "authorization_code"))
        || registered.response_types.as_ref().is_some_and(|types| !types.iter().any(|kind| kind == "code"))
    {
        return Err(Error::UnsupportedClient);
    }
    Ok(registered.client_id)
}

#[derive(Deserialize)]
struct Registration {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
    redirect_uris: Vec<String>,
    #[serde(default)]
    grant_types: Option<Vec<String>>,
    #[serde(default)]
    response_types: Option<Vec<String>>,
}

pub(super) fn authorization_url(metadata: &Metadata, client_id: &str, resource: &str, redirect: &str, state: &str, verifier: &str) -> Result<String, Error> {
    let mut url = HttpsUrl::parse(&metadata.authorization, false).map_err(|_| Error::Invalid)?.parsed;
    const RESERVED: &[&str] = &["response_type", "client_id", "resource", "redirect_uri", "state", "code_challenge", "code_challenge_method", "scope"];
    if url.query_pairs().any(|(key, _)| RESERVED.contains(&key.as_ref())) {
        return Err(Error::Invalid);
    }
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", client_id),
        ("resource", resource),
        ("redirect_uri", redirect),
        ("state", state),
        ("code_challenge", &pkce_challenge(verifier)),
        ("code_challenge_method", "S256"),
    ]);
    if url.as_str().len() > 16 * 1024 { Err(Error::Invalid) } else { Ok(url.into()) }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Tokens {
    pub access: String,
    pub refresh: Option<String>,
    pub expires_at: i64,
}

impl Drop for Tokens {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.access.zeroize();
        self.refresh.zeroize();
    }
}

// Keep the security-relevant OAuth request bindings explicit at this single internal call site.
#[allow(clippy::too_many_arguments)]
pub(super) async fn exchange(
    network: &dyn Network,
    metadata: &Metadata,
    client: &str,
    resource: &str,
    redirect: &str,
    verifier: &str,
    code: &str,
    now: i64,
) -> Result<Tokens, Error> {
    let reply = network
        .post(
            &metadata.token,
            Body::Form(vec![
                ("grant_type".into(), "authorization_code".into()),
                ("client_id".into(), client.into()),
                ("resource".into(), resource.into()),
                ("redirect_uri".into(), redirect.into()),
                ("code_verifier".into(), verifier.into()),
                ("code".into(), code.into()),
            ]),
        )
        .await?;
    parse_tokens(reply, now, None)
}

pub(super) async fn refresh(network: &dyn Network, metadata: &Metadata, client: &str, resource: &str, previous: &Tokens, now: i64) -> Result<Tokens, Error> {
    let refresh = previous.refresh.as_ref().ok_or(Error::Relogin)?;
    let reply = network
        .post(
            &metadata.token,
            Body::Form(vec![
                ("grant_type".into(), "refresh_token".into()),
                ("client_id".into(), client.into()),
                ("resource".into(), resource.into()),
                ("refresh_token".into(), refresh.clone()),
            ]),
        )
        .await?;
    parse_tokens(reply, now, previous.refresh.as_deref())
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    error: Option<String>,
}
#[derive(Deserialize)]
struct TokenError {
    error: String,
}

fn parse_tokens(reply: Reply, now: i64, previous_refresh: Option<&str>) -> Result<Tokens, Error> {
    if reply.body.len() > MAX_BODY {
        return Err(Error::Invalid);
    }
    if reply.status == 401 {
        return Err(Error::Relogin);
    }
    if !reply.json {
        return Err(Error::Invalid);
    }
    if reply.status != 200 {
        let error: TokenError = json(&reply.body)?;
        return Err(if matches!(error.error.as_str(), "invalid_grant" | "invalid_client" | "unauthorized_client") || reply.status == 401 {
            Error::Relogin
        } else {
            Error::Failed
        });
    }
    let raw: TokenResponse = json(&reply.body)?;
    if raw.error.is_some()
        || !raw.token_type.eq_ignore_ascii_case("Bearer")
        || !bearer_valid(&raw.access_token)
        || raw.refresh_token.as_deref().is_some_and(|token| !credential_valid(token))
        || raw.scope.as_ref().is_some_and(|scope| scope.len() > 4096 || scope.chars().any(char::is_control))
        || raw.expires_in.is_some_and(|seconds| seconds == 0 || seconds > 31 * 24 * 60 * 60)
    {
        return Err(Error::Invalid);
    }
    // Missing lifetime is not an infinite authorization. Revalidate/refresh after five minutes.
    let seconds = raw.expires_in.unwrap_or(300).min(24 * 60 * 60) as i64;
    Ok(Tokens {
        access: raw.access_token,
        refresh: raw.refresh_token.or_else(|| previous_refresh.map(str::to_owned)),
        expires_at: now.checked_add(seconds).ok_or(Error::Invalid)?,
    })
}

pub(super) async fn revoke(network: &dyn Network, metadata: &Metadata, client: &str, tokens: &Tokens) -> bool {
    let Some(endpoint) = metadata.revocation.as_deref() else { return false };
    let mut revoked = true;
    // Revoke both separately. Revoking a refresh token need not invalidate already-issued access.
    for (token, hint) in
        tokens.refresh.as_ref().map(|token| (token.as_str(), "refresh_token")).into_iter().chain(std::iter::once((tokens.access.as_str(), "access_token")))
    {
        let result = network
            .post(endpoint, Body::Form(vec![("client_id".into(), client.into()), ("token".into(), token.into()), ("token_type_hint".into(), hint.into())]))
            .await;
        revoked &= result.is_ok_and(|reply| reply.status == 200 && reply.body.len() <= MAX_BODY);
    }
    revoked
}

fn json<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, Error> {
    if body.len() > MAX_BODY || body.iter().copied().find(|byte| !byte.is_ascii_whitespace()) != Some(b'{') {
        return Err(Error::Invalid);
    }
    serde_json::from_slice(body).map_err(|_| Error::Invalid)
}

/// An attempt owns its listener, verifier, and state, and never serializes any of them.
pub(super) struct Loopback {
    listener: TcpListener,
    pub redirect: String,
    host: String,
    path: String,
}

impl Loopback {
    pub(super) async fn bind(manual_client: bool) -> Result<Self, Error> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.map_err(|_| Error::Failed)?;
        let address = listener.local_addr().map_err(|_| Error::Failed)?;
        let host = address.to_string();
        // RFC8252 permits a changing loopback port, not an unregistered callback path. Manual
        // public clients pre-register this fixed path; DCR registers its unique path each time.
        let path = if manual_client { "/mcp-oauth/callback".into() } else { format!("/mcp-oauth/callback/{}", random()?) };
        let redirect = format!("http://{host}{path}");
        Ok(Self { listener, redirect, host, path })
    }

    pub(super) async fn code(self, state: &str, metadata: &Metadata) -> Result<Zeroizing<String>, Error> {
        for _ in 0..MAX_CALLBACK_REQUESTS {
            let (mut stream, peer) = self.listener.accept().await.map_err(|_| Error::Callback)?;
            if peer.ip() != IpAddr::V4(std::net::Ipv4Addr::LOCALHOST) {
                continue;
            }
            let result = tokio::time::timeout(Duration::from_secs(2), read_callback(&mut stream, &self.host, &self.path, state, metadata)).await;
            let valid = matches!(&result, Ok(Ok(_)));
            let body = if valid { "OAuth response received. You may close this window." } else { "Invalid OAuth callback." };
            let response = format!(
                "HTTP/1.1 {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; frame-ancestors 'none'\r\n\r\n{}",
                if valid { "200 OK" } else { "400 Bad Request" },
                body.len(),
                body
            );
            let _ = tokio::time::timeout(Duration::from_secs(1), stream.write_all(response.as_bytes())).await;
            if let Ok(Ok(callback)) = result {
                return callback;
            }
        }
        Err(Error::Callback)
    }
}

async fn read_callback(stream: &mut TcpStream, host: &str, path: &str, state: &str, metadata: &Metadata) -> Result<Result<Zeroizing<String>, Error>, Error> {
    let mut bytes = Zeroizing::new(Vec::new());
    let end = loop {
        if let Some(position) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break position + 4;
        }
        if bytes.len() >= MAX_CALLBACK {
            return Err(Error::Callback);
        }
        let mut chunk = [0u8; 1024];
        let read = stream.read(&mut chunk).await.map_err(|_| Error::Callback)?;
        if read == 0 || read > MAX_CALLBACK - bytes.len() {
            return Err(Error::Callback);
        }
        bytes.extend_from_slice(&chunk[..read]);
    };
    if bytes.len() != end {
        return Err(Error::Callback);
    }
    callback(&bytes, host, path, state, metadata)
}

fn callback(bytes: &[u8], host: &str, path: &str, state: &str, metadata: &Metadata) -> Result<Result<Zeroizing<String>, Error>, Error> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = httparse::Request::new(&mut headers);
    if !request.parse(bytes).map_err(|_| Error::Callback)?.is_complete() || request.method != Some("GET") || !matches!(request.version, Some(0 | 1)) {
        return Err(Error::Callback);
    }
    let mut seen_host = false;
    for header in request.headers {
        if header.name.eq_ignore_ascii_case("host") {
            if seen_host || header.value != host.as_bytes() {
                return Err(Error::Callback);
            }
            seen_host = true;
        }
        if header.name.eq_ignore_ascii_case("transfer-encoding") || (header.name.eq_ignore_ascii_case("content-length") && header.value != b"0") {
            return Err(Error::Callback);
        }
    }
    if !seen_host {
        return Err(Error::Callback);
    }
    let target = request.path.ok_or(Error::Callback)?;
    let (actual_path, query) = target.split_once('?').ok_or(Error::Callback)?;
    if actual_path != path || query.is_empty() || query.contains('#') || !valid_encoding(query) {
        return Err(Error::Callback);
    }
    let mut fields = HashMap::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if !matches!(key.as_ref(), "state" | "code" | "iss" | "error" | "error_description" | "error_uri" | "scope" | "session_state")
            || fields.insert(key.into_owned(), value.into_owned()).is_some()
        {
            return Err(Error::Callback);
        }
    }
    let actual_state = fields.get("state").ok_or(Error::Callback)?;
    #[allow(deprecated)]
    if ring::constant_time::verify_slices_are_equal(actual_state.as_bytes(), state.as_bytes()).is_err() {
        return Err(Error::Callback);
    }
    match fields.get("iss") {
        Some(issuer) if issuer != &metadata.issuer => return Err(Error::Callback),
        None if metadata.require_iss => return Err(Error::Callback),
        _ => {}
    }
    // Without RFC9207, the attempt's exclusive loopback listener/redirect URI + random state bind
    // this response to exactly one issuer/token endpoint; neither comes from callback parameters.
    match (fields.remove("code"), fields.remove("error")) {
        (Some(code), None) if secret_valid(&code) && code.len() <= 4096 => Ok(Ok(Zeroizing::new(code))),
        (None, Some(error)) if !error.is_empty() && error.len() <= 256 => Ok(Err(Error::Relogin)),
        _ => Err(Error::Callback),
    }
}

fn valid_encoding(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.iter().enumerate().all(|(index, byte)| {
        !byte.is_ascii_control()
            && *byte != b' '
            && (*byte != b'%' || (index + 2 < bytes.len() && bytes[index + 1].is_ascii_hexdigit() && bytes[index + 2].is_ascii_hexdigit()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn metadata() -> Metadata {
        Metadata {
            issuer: "https://issuer.example".into(),
            authorization: "https://issuer.example/authorize".into(),
            token: "https://issuer.example/token".into(),
            registration: None,
            revocation: None,
            require_iss: true,
            public_auth: Some(true),
        }
    }
    fn request(query: &str) -> Vec<u8> {
        format!("GET /callback?{query} HTTP/1.1\r\nHost: 127.0.0.1:12345\r\n\r\n").into_bytes()
    }
    fn reply(status: u16, body: serde_json::Value) -> Reply {
        Reply { status, json: true, body: Zeroizing::new(serde_json::to_vec(&body).unwrap()) }
    }

    #[test]
    fn pkce_matches_rfc7636_and_randoms_have_256_bits() {
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        let first = random().unwrap();
        assert_eq!(first.len(), 43);
        assert_ne!(first, random().unwrap());
        assert_eq!(URL_SAFE_NO_PAD.decode(first).unwrap().len(), 32);
    }

    #[test]
    fn callback_rejects_wrong_host_path_duplicates_state_issuer_and_ambiguous_results() {
        let good = "state=synthetic-state&code=synthetic-code&iss=https%3A%2F%2Fissuer.example";
        assert_eq!(callback(&request(good), "127.0.0.1:12345", "/callback", "synthetic-state", &metadata()).unwrap().unwrap().as_str(), "synthetic-code");
        for query in [
            good.replace("synthetic-state", "wrong-state"),
            good.replace("issuer.example", "other.example"),
            format!("{good}&state=synthetic-state"),
            format!("{good}&%63ode=duplicate"),
            format!("{good}&error=denied"),
            "state=synthetic-state&code=synthetic-code".into(),
            format!("{good}&unknown=x"),
            format!("{good}&scope=%zz"),
        ] {
            assert!(callback(&request(&query), "127.0.0.1:12345", "/callback", "synthetic-state", &metadata()).is_err());
        }
        for changed in [
            String::from_utf8(request(good)).unwrap().replace("127.0.0.1", "localhost"),
            String::from_utf8(request(good)).unwrap().replace("/callback?", "/other?"),
            String::from_utf8(request(good)).unwrap().replace("GET ", "POST "),
            String::from_utf8(request(good)).unwrap().replace("\r\n\r\n", "\r\nHost: 127.0.0.1:12345\r\n\r\n"),
        ] {
            assert!(callback(changed.as_bytes(), "127.0.0.1:12345", "/callback", "synthetic-state", &metadata()).is_err());
        }
    }

    #[test]
    fn token_bounds_type_errors_and_rotation_are_sanitized() {
        let parsed = parse_tokens(
            reply(200, json!({"access_token":"access-canary", "token_type":"bearer", "refresh_token":"rotated-canary", "expires_in":3600})),
            100,
            Some("old-canary"),
        )
        .unwrap();
        assert_eq!(parsed.access, "access-canary");
        assert_eq!(parsed.refresh.as_deref(), Some("rotated-canary"));
        assert_eq!(parsed.expires_at, 3700);
        let unchanged = parse_tokens(reply(200, json!({"access_token":"new-canary", "token_type":"Bearer"})), 100, Some("old-canary")).unwrap();
        assert_eq!(unchanged.refresh.as_deref(), Some("old-canary"));
        for body in [
            json!({"access_token":"leak\r\ncanary","token_type":"Bearer"}),
            json!({"access_token":"access-canary","token_type":"MAC"}),
            json!({"access_token":"t","token_type":"Bearer","expires_in":3600}),
            json!({"access_token":"access-canary","refresh_token":"r","token_type":"Bearer","expires_in":3600}),
            json!({"access_token":"canary","token_type":"Bearer","expires_in":0}),
            json!({"access_token":"x".repeat(MAX_SECRET+1),"token_type":"Bearer"}),
        ] {
            assert_eq!(parse_tokens(reply(200, body), 100, None).err(), Some(Error::Invalid));
        }
        for (code, expected) in [("invalid_grant", Error::Relogin), ("invalid_client", Error::Relogin), ("private-canary-error", Error::Failed)] {
            let error = parse_tokens(reply(400, json!({"error": code, "error_description":"private-canary"})), 100, None).err().unwrap();
            assert_eq!(error, expected);
            assert!(!error.message().contains("canary"));
        }
        assert_eq!(parse_tokens(Reply { status: 200, json: true, body: Zeroizing::new(vec![b' '; MAX_BODY + 1]) }, 0, None).err(), Some(Error::Invalid));
    }

    #[test]
    fn public_network_policy_and_authorization_parameter_binding() {
        assert!(!public_addresses(&[]));
        assert!(!public_addresses(&["1.1.1.1:443".parse().unwrap(), "127.0.0.1:443".parse().unwrap()]));
        assert!(public_addresses(&["1.1.1.1:443".parse().unwrap()]));
        let url = authorization_url(&metadata(), "public-id", "https://mcp.example/tools", "http://127.0.0.1:12345/callback", "state", "verifier").unwrap();
        let url = Url::parse(&url).unwrap();
        let pairs: HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(pairs["resource"], "https://mcp.example/tools");
        assert_eq!(pairs["code_challenge_method"], "S256");
        let mut bad = metadata();
        bad.authorization.push_str("?state=shadow");
        assert_eq!(authorization_url(&bad, "id", "r", "d", "s", "v").err(), Some(Error::Invalid));
    }

    #[tokio::test]
    async fn manual_clients_use_registered_stable_path_while_dcr_paths_are_unique() {
        let manual1 = Loopback::bind(true).await.unwrap();
        let manual2 = Loopback::bind(true).await.unwrap();
        assert_eq!(manual1.path, "/mcp-oauth/callback");
        assert_eq!(manual1.path, manual2.path);
        assert_ne!(manual1.host, manual2.host, "two live attempts get distinct ephemeral ports");
        let dcr1 = Loopback::bind(false).await.unwrap();
        let dcr2 = Loopback::bind(false).await.unwrap();
        assert_ne!(dcr1.path, dcr2.path);
        assert!(dcr1.path.starts_with("/mcp-oauth/callback/"));
    }

    #[tokio::test]
    async fn callback_listener_is_loopback_only_and_closes_after_consumption_or_drop() {
        let callback = Loopback::bind(false).await.unwrap();
        let url = Url::parse(&callback.redirect).unwrap();
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        let address = callback.host.clone();
        let path = callback.path.clone();
        let task = tokio::spawn(async move { callback.code("state", &metadata()).await });
        let mut stream = TcpStream::connect(&address).await.unwrap();
        stream
            .write_all(format!("GET {path}?state=state&code=synthetic-code&iss=https%3A%2F%2Fissuer.example HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(!response.contains("synthetic-code") && !response.contains("state="));
        assert_eq!(task.await.unwrap().unwrap().as_str(), "synthetic-code");
        assert!(TcpStream::connect(&address).await.is_err(), "replay listener is closed");
        let callback = Loopback::bind(false).await.unwrap();
        let address = callback.host.clone();
        drop(callback);
        assert!(TcpStream::connect(&address).await.is_err());
    }
}
