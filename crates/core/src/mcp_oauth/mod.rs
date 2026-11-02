//! Remote MCP OAuth: strict discovery, encrypted credential lifecycle, and frozen transport leases.
//!
//! The argument to [`discover`] is the exact resource identifier, not a transport URL from which
//! an origin should be inferred. The discovery function is credential-free; [`OAuthService`]
//! owns public-client registration, PKCE/loopback login and encrypted token storage. The runtime
//! exposes only local per-turn leases to the engine. `WWW-Authenticate` challenge discovery and
//! signed metadata remain outside the supported initial profile.
//!
//! Resource and issuer identities use exact decoded JSON string equality, without URL
//! normalization (RFC 9728 §3.3/§6 and RFC 8414 §3.3/§4). Candidate fallback is allowed only for
//! absent metadata (404/410), never for a returned but invalid document. Published URLs are
//! validated, not fetched. Any later endpoint request must apply its own DNS/pinning policy.

use std::fmt;
use std::time::Duration;

use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde::Deserialize;
use url::{Host, Url};

use crate::computer::network::is_global_ip;

mod fetch;
mod flow;
pub(crate) mod relay;
pub mod service;
mod vault;
pub use service::{LoginInput, LoginResponse, OAuthService, OAuthStatus};
#[cfg(test)]
mod tests;

pub const MAX_URL_BYTES: usize = 4096;
pub const MAX_METADATA_BYTES: usize = 64 * 1024;
pub const MAX_AUTHORIZATION_SERVERS: usize = 4;
/// Two resource locations, followed by at most three locations for each advertised issuer.
pub const MAX_REQUESTS: usize = 2 + 3 * MAX_AUTHORIZATION_SERVERS;
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
pub const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_METADATA_VALUES: usize = 128;
const MAX_VALUE_BYTES: usize = 512;

/// Fixed, bounded diagnostics. No variant contains a URL, response body, or underlying error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiscoveryError {
    InvalidUrl,
    BlockedAddress,
    MetadataUnavailable,
    Redirect,
    FetchFailed,
    MetadataTooLarge,
    InvalidMetadata,
    ResourceMismatch,
    IssuerMismatch,
    UnsupportedPkce,
    UnsupportedGrant,
    LimitExceeded,
    Timeout,
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidUrl => "OAuth discovery requires a safe HTTPS URL.",
            Self::BlockedAddress => "OAuth discovery requires exclusively public network addresses.",
            Self::MetadataUnavailable => "OAuth discovery metadata is unavailable.",
            Self::Redirect => "OAuth discovery does not follow redirects.",
            Self::FetchFailed => "OAuth discovery could not fetch metadata.",
            Self::MetadataTooLarge => "OAuth discovery metadata exceeds the size limit.",
            Self::InvalidMetadata => "OAuth discovery metadata is invalid.",
            Self::ResourceMismatch => "OAuth discovery resource identity does not match.",
            Self::IssuerMismatch => "OAuth discovery issuer identity does not match.",
            Self::UnsupportedPkce => "OAuth discovery requires advertised S256 PKCE support.",
            Self::UnsupportedGrant => "OAuth discovery requires authorization-code support.",
            Self::LimitExceeded => "OAuth discovery exceeds a safety limit.",
            Self::Timeout => "OAuth discovery exceeded its time limit.",
        })
    }
}

impl std::error::Error for DiscoveryError {}

/// A credential-free HTTPS URL, preserving its original spelling for identity comparisons.
/// This proves URL syntax/policy, not that the host will resolve publicly at a later time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpsUrl {
    original: String,
    parsed: Url,
}

impl HttpsUrl {
    pub fn as_str(&self) -> &str {
        &self.original
    }

    pub fn as_url(&self) -> &Url {
        &self.parsed
    }

    fn parse(input: &str, issuer: bool) -> Result<Self, DiscoveryError> {
        // URL parsers otherwise accept whitespace stripping, backslash normalization, and
        // abbreviated authorities. Reject those forms before parsing, including empty userinfo.
        if input.len() > MAX_URL_BYTES
            || !input.get(..8).is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
            || input.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\')
            || input[8..].split(['/', '?', '#']).next().is_none_or(|authority| authority.is_empty() || authority.contains('@'))
        {
            return Err(DiscoveryError::InvalidUrl);
        }
        let bytes = input.as_bytes();
        for (index, byte) in bytes.iter().enumerate() {
            if *byte == b'%' && (index + 2 >= bytes.len() || !bytes[index + 1].is_ascii_hexdigit() || !bytes[index + 2].is_ascii_hexdigit()) {
                return Err(DiscoveryError::InvalidUrl);
            }
        }
        let parsed = Url::parse(input).map_err(|_| DiscoveryError::InvalidUrl)?;
        if parsed.scheme() != "https"
            || parsed.host().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.fragment().is_some()
            || (issuer && parsed.query().is_some())
            || parsed.port() == Some(0)
        {
            return Err(DiscoveryError::InvalidUrl);
        }
        match parsed.host() {
            Some(Host::Ipv4(ip)) if !is_global_ip(ip.into()) => return Err(DiscoveryError::BlockedAddress),
            Some(Host::Ipv6(ip)) if !is_global_ip(ip.into()) => return Err(DiscoveryError::BlockedAddress),
            _ => {}
        }
        Ok(Self { original: input.to_owned(), parsed })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceMetadata {
    pub resource: HttpsUrl,
    pub authorization_servers: Vec<HttpsUrl>,
    pub scopes_supported: Vec<String>,
}

/// Only the fields needed by a later authorization-code client are retained and their URLs checked.
/// Unknown fields are ignored as required by RFC 8414; they are not fetched or exposed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizationServerMetadata {
    pub issuer: HttpsUrl,
    pub authorization_endpoint: HttpsUrl,
    pub token_endpoint: HttpsUrl,
    pub registration_endpoint: Option<HttpsUrl>,
    pub scopes_supported: Vec<String>,
    /// Absence does not establish support for unauthenticated public clients. Registration and
    /// client-authentication policy remain the responsibility of the later login implementation.
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovery {
    pub resource: ResourceMetadata,
    pub authorization_server: AuthorizationServerMetadata,
}

/// Discover a validated resource/authorization-server pair without credentials or persistent state.
///
/// Tries RFC 9728's path-aware resource location, then the same origin's root location. Every
/// returned document must name `resource_url` exactly, including its query or trailing slash.
/// Advertised issuers are tried in order, with RFC 8414 OAuth, RFC 8414 OIDC, then legacy OIDC
/// locations. An issuer is skipped only when all its metadata locations return 404/410.
///
/// All requests are anonymous, HTTPS-only, redirect-free, size/time bounded, and pinned to DNS
/// answers checked with the managed computer's public-address policy. Nothing is inferred when
/// metadata is absent, and an invalid response terminates discovery instead of downgrading it.
pub async fn discover(resource_url: &str) -> Result<Discovery, DiscoveryError> {
    discover_with(resource_url, &fetch::Production, Limits::PRODUCTION).await
}

struct Response {
    status: u16,
    json: bool,
    content_length: Option<u64>,
    body: BoxStream<'static, Result<Vec<u8>, DiscoveryError>>,
}

/// Private, in-memory test seam. Production never exposes a custom client, resolver, or HTTP/local
/// exception to callers.
trait Fetch: Send + Sync {
    fn get<'a>(&'a self, url: &'a Url) -> BoxFuture<'a, Result<Response, DiscoveryError>>;
}

#[derive(Clone, Copy)]
struct Limits {
    request_timeout: Duration,
    total_timeout: Duration,
    requests: usize,
}

impl Limits {
    const PRODUCTION: Self = Self { request_timeout: REQUEST_TIMEOUT, total_timeout: DISCOVERY_TIMEOUT, requests: MAX_REQUESTS };
}

async fn discover_with(resource_url: &str, fetch: &impl Fetch, limits: Limits) -> Result<Discovery, DiscoveryError> {
    tokio::time::timeout(limits.total_timeout, async {
        let expected = HttpsUrl::parse(resource_url, false)?;
        let mut requests = 0;
        let mut document = None;
        for candidate in resource_locations(&expected.parsed) {
            if let Some(body) = read_document(fetch, &candidate, &mut requests, limits).await? {
                document = Some(body);
                break;
            }
        }
        let document = document.ok_or(DiscoveryError::MetadataUnavailable)?;
        let raw: ResourceDocument = parse_metadata(&document)?;
        // Do not inspect or follow any advertised authorization server until identity is bound.
        if raw.resource != resource_url {
            return Err(DiscoveryError::ResourceMismatch);
        }
        if raw.authorization_servers.is_empty() {
            return Err(DiscoveryError::MetadataUnavailable);
        }
        if raw.authorization_servers.len() > MAX_AUTHORIZATION_SERVERS {
            return Err(DiscoveryError::LimitExceeded);
        }
        let issuers = raw.authorization_servers.iter().map(|issuer| HttpsUrl::parse(issuer, true)).collect::<Result<Vec<_>, _>>()?;
        validate_values(&raw.scopes_supported)?;
        let resource = ResourceMetadata { resource: expected, authorization_servers: issuers, scopes_supported: raw.scopes_supported };
        for issuer in &resource.authorization_servers {
            for candidate in issuer_locations(&issuer.parsed) {
                if let Some(body) = read_document(fetch, &candidate, &mut requests, limits).await? {
                    let authorization_server = validate_authorization_server(&body, issuer)?;
                    return Ok(Discovery { resource, authorization_server });
                }
            }
        }
        Err(DiscoveryError::MetadataUnavailable)
    })
    .await
    .map_err(|_| DiscoveryError::Timeout)?
}

async fn read_document(fetch: &impl Fetch, url: &Url, requests: &mut usize, limits: Limits) -> Result<Option<Vec<u8>>, DiscoveryError> {
    if *requests >= limits.requests {
        return Err(DiscoveryError::LimitExceeded);
    }
    *requests += 1;
    tokio::time::timeout(limits.request_timeout, async {
        let mut response = fetch.get(url).await?;
        match response.status {
            404 | 410 => return Ok(None),
            300..=399 => return Err(DiscoveryError::Redirect),
            200 => {}
            _ => return Err(DiscoveryError::FetchFailed),
        }
        if response.content_length.is_some_and(|size| size > MAX_METADATA_BYTES as u64) {
            return Err(DiscoveryError::MetadataTooLarge);
        }
        if !response.json {
            return Err(DiscoveryError::InvalidMetadata);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.body.next().await {
            let chunk = chunk?;
            if chunk.len() > MAX_METADATA_BYTES - body.len() {
                return Err(DiscoveryError::MetadataTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Some(body))
    })
    .await
    .map_err(|_| DiscoveryError::Timeout)?
}

fn inserted_location(url: &Url, suffix: &str) -> Url {
    let mut result = url.clone();
    let path = if url.path() == "/" { "" } else { url.path() };
    result.set_path(&format!("/.well-known/{suffix}{path}"));
    result
}

fn resource_locations(resource: &Url) -> Vec<Url> {
    let path_aware = inserted_location(resource, "oauth-protected-resource");
    let mut root = resource.clone();
    root.set_path("/.well-known/oauth-protected-resource");
    root.set_query(None);
    let mut result = vec![path_aware];
    if result[0] != root {
        result.push(root);
    }
    result
}

fn issuer_locations(issuer: &Url) -> Vec<Url> {
    // RFC 8414 removes a terminating slash for location construction only. The original issuer
    // remains unchanged for exact metadata identity comparison.
    let mut location = issuer.clone();
    location.set_path(issuer.path().trim_end_matches('/'));
    let mut result = vec![inserted_location(&location, "oauth-authorization-server"), inserted_location(&location, "openid-configuration")];
    let mut oidc = issuer.clone();
    oidc.set_path(&format!("{}/.well-known/openid-configuration", issuer.path().trim_end_matches('/')));
    if !result.contains(&oidc) {
        result.push(oidc);
    }
    result
}

#[derive(Deserialize)]
struct ResourceDocument {
    resource: String,
    #[serde(default)]
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

#[derive(Deserialize)]
struct AuthorizationServerDocument {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default, deserialize_with = "present")]
    registration_endpoint: Option<String>,
    response_types_supported: Vec<String>,
    #[serde(default, deserialize_with = "present")]
    grant_types_supported: Option<Vec<String>>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
    #[serde(default, deserialize_with = "present")]
    token_endpoint_auth_methods_supported: Option<Vec<String>>,
}

// The omission defaults in RFC 8414 do not apply to explicit null values.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn parse_metadata<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, DiscoveryError> {
    // A named Serde struct can also deserialize a positional JSON array. Require an object, then
    // deserialize the original bytes so duplicate known fields are still rejected by Serde.
    if body.iter().copied().find(|byte| !byte.is_ascii_whitespace()) != Some(b'{') {
        return Err(DiscoveryError::InvalidMetadata);
    }
    serde_json::from_slice(body).map_err(|_| DiscoveryError::InvalidMetadata)
}

fn validate_authorization_server(body: &[u8], issuer: &HttpsUrl) -> Result<AuthorizationServerMetadata, DiscoveryError> {
    let raw: AuthorizationServerDocument = parse_metadata(body)?;
    if raw.issuer != issuer.original {
        return Err(DiscoveryError::IssuerMismatch);
    }
    for values in [&raw.response_types_supported, &raw.code_challenge_methods_supported, &raw.scopes_supported]
        .into_iter()
        .chain(raw.grant_types_supported.iter())
        .chain(raw.token_endpoint_auth_methods_supported.iter())
    {
        validate_values(values)?;
    }
    if !raw.code_challenge_methods_supported.iter().any(|method| method == "S256") {
        return Err(DiscoveryError::UnsupportedPkce);
    }
    if !raw.response_types_supported.iter().any(|response| response == "code")
        || raw.grant_types_supported.as_ref().is_some_and(|grants| !grants.iter().any(|grant| grant == "authorization_code"))
    {
        return Err(DiscoveryError::UnsupportedGrant);
    }
    Ok(AuthorizationServerMetadata {
        issuer: issuer.clone(),
        authorization_endpoint: HttpsUrl::parse(&raw.authorization_endpoint, false)?,
        token_endpoint: HttpsUrl::parse(&raw.token_endpoint, false)?,
        registration_endpoint: raw.registration_endpoint.as_deref().map(|endpoint| HttpsUrl::parse(endpoint, false)).transpose()?,
        scopes_supported: raw.scopes_supported,
        token_endpoint_auth_methods_supported: raw.token_endpoint_auth_methods_supported,
    })
}

fn validate_values(values: &[String]) -> Result<(), DiscoveryError> {
    if values.len() > MAX_METADATA_VALUES || values.iter().any(|value| value.is_empty() || value.len() > MAX_VALUE_BYTES || value.chars().any(char::is_control))
    {
        return Err(DiscoveryError::LimitExceeded);
    }
    Ok(())
}
