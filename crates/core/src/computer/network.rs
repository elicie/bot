//! Network policy of the managed computer. Port of `ComputerNetworkPolicy`,
//! `isGlobalComputerAddress`, `computerUrl`, and `computerOrigin` in `server/computer-network.ts`.
//!
//! Only user-allowlisted public HTTP(S) origins are reachable. Every DNS answer must be a global
//! unicast address (one private answer denies the whole name, which defeats DNS rebinding through
//! mixed answers), every request resolves again instead of trusting an earlier lookup, and the
//! proxy connects to the validated address it was given, never re-resolving the name.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;

use futures::future::BoxFuture;
use grok_bot_model::{HttpError, HttpResult};
use url::{Host, Url};

use super::{test_mode, utf16_len};

/// Name resolution used by the policy; tests inject answers.
pub type Lookup = Arc<dyn Fn(String) -> BoxFuture<'static, std::io::Result<Vec<IpAddr>>> + Send + Sync>;

const IPV4_DENIED: [(u32, u32); 15] = [
    (0x0000_0000, 8),
    (0x0a00_0000, 8),
    (0x6440_0000, 10),
    (0x7f00_0000, 8),
    (0xa9fe_0000, 16),
    (0xac10_0000, 12),
    (0xc000_0000, 24),
    (0xc000_0200, 24),
    (0xc058_6300, 24),
    (0xc0a8_0000, 16),
    (0xc612_0000, 15),
    (0xc633_6400, 24),
    (0xcb00_7100, 24),
    (0xe000_0000, 4),
    (0xf000_0000, 4),
];

/// A global unicast address: IPv4 outside the special-purpose ranges, IPv6 inside 2000::/3 minus
/// the IETF protocol, documentation, 6to4, and 3fff::/20 ranges. Mapped, NAT64, ULA, link-local,
/// multicast, loopback, and zone-scoped addresses are never global.
pub fn is_global_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            let value = u32::from(v4);
            !IPV4_DENIED.iter().any(|(network, bits)| value >> (32 - bits) == network >> (32 - bits))
        }
        IpAddr::V6(v6) => {
            let value = u128::from(v6);
            let in_range = |network: u128, bits: u32| value >> (128 - bits) == network >> (128 - bits);
            in_range(0x2000 << 112, 3)
                && !in_range(0x2001 << 112, 23)
                && !in_range(0x2001_0db8 << 96, 32)
                && !in_range(0x2002 << 112, 16)
                && !in_range(0x3fff << 112, 20)
        }
    }
}

/// `isGlobalComputerAddress`: the textual form must be an unambiguous IP literal.
pub fn is_global_address(address: &str) -> bool {
    !address.contains('%') && address.parse::<IpAddr>().is_ok_and(is_global_ip)
}

/// `computerUrl`: an HTTP(S) URL without credentials, whitespace, controls, or backslashes.
pub fn computer_url(input: &str) -> HttpResult<Url> {
    if utf16_len(input) > 4096 || input.chars().any(|c| c <= '\u{20}' || c == '\u{7f}' || c == '\\') {
        return Err(HttpError::bad_request("Use an HTTP or HTTPS URL without credentials."));
    }
    let url = Url::parse(input).map_err(|_| HttpError::bad_request("Use a valid HTTP or HTTPS URL."))?;
    if !matches!(url.scheme(), "http" | "https") || !url.username().is_empty() || url.password().is_some() {
        return Err(HttpError::forbidden("Only HTTP and HTTPS URLs without credentials are allowed."));
    }
    Ok(url)
}

pub(crate) fn origin_of(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// `computerOrigin`: an allowlist entry is a bare origin.
pub fn computer_origin(input: &str) -> HttpResult<String> {
    let url = computer_url(input)?;
    if url.path() != "/" || url.query().is_some_and(|q| !q.is_empty()) || url.fragment().is_some_and(|f| !f.is_empty()) {
        return Err(HttpError::bad_request("Allowlist entries must be origins without a path, query, or fragment."));
    }
    Ok(origin_of(&url))
}

/// A validated destination: connect to `address:port`, never to the name again.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub url: Url,
    pub address: IpAddr,
    pub port: u16,
}

/// What the proxy asks before connecting. [`NetworkPolicy`] is the production implementation.
pub trait Destination: Send + Sync {
    fn resolve<'a>(&'a self, input: &'a str) -> BoxFuture<'a, HttpResult<Resolved>>;
}

pub struct NetworkPolicy {
    pub origins: Vec<String>,
    allowed: HashSet<String>,
    fixtures: HashSet<String>,
    lookup: Lookup,
}

fn system_lookup() -> Lookup {
    Arc::new(|host: String| {
        Box::pin(async move {
            let answers = tokio::net::lookup_host((host.as_str(), 0)).await?;
            Ok(answers.map(|answer| answer.ip()).collect())
        })
    })
}

impl NetworkPolicy {
    /// `fixtures` are loopback test origins (available only in [`test_mode`]); `lookup` replaces
    /// the system resolver.
    pub fn new(origins: &[String], fixtures: &[String], lookup: Option<Lookup>) -> HttpResult<Self> {
        if origins.is_empty() || origins.len() > 16 {
            return Err(HttpError::bad_request("Select between one and sixteen allowed public origins."));
        }
        let mut list = Vec::new();
        for origin in origins {
            let origin = computer_origin(origin)?;
            if !list.contains(&origin) {
                list.push(origin);
            }
        }
        let mut fixture_set = HashSet::new();
        for fixture in fixtures {
            fixture_set.insert(computer_origin(fixture)?);
        }
        if !fixture_set.is_empty() && !test_mode() {
            return Err(HttpError::forbidden("Local fixture origins are available only to tests."));
        }
        for fixture in &fixture_set {
            let url = Url::parse(fixture).map_err(|_| HttpError::bad_request("Fixture origins must be explicit loopback HTTP ports."))?;
            let loopback = matches!(url.host(), Some(Host::Ipv4(ip)) if ip.is_loopback() && ip.octets() == [127, 0, 0, 1])
                || matches!(url.host(), Some(Host::Ipv6(ip)) if ip.is_loopback());
            if url.scheme() != "http" || !loopback || url.port().is_none() {
                return Err(HttpError::bad_request("Fixture origins must be explicit loopback HTTP ports."));
            }
        }
        Ok(Self { allowed: list.iter().cloned().collect(), origins: list, fixtures: fixture_set, lookup: lookup.unwrap_or_else(system_lookup) })
    }

    pub fn allows(&self, input: &str) -> bool {
        computer_url(input).is_ok_and(|url| self.allowed.contains(&origin_of(&url)))
    }

    pub async fn resolve(&self, input: &str) -> HttpResult<Resolved> {
        let url = computer_url(input)?;
        let origin = origin_of(&url);
        if !self.allowed.contains(&origin) {
            return Err(HttpError::forbidden("The destination origin is not allowed."));
        }
        let answers = match url.host() {
            Some(Host::Ipv4(ip)) => vec![IpAddr::V4(ip)],
            Some(Host::Ipv6(ip)) => vec![IpAddr::V6(ip)],
            Some(Host::Domain(name)) => {
                (self.lookup)(name.to_owned()).await.map_err(|_| HttpError::forbidden("The destination could not be resolved safely."))?
            }
            None => return Err(HttpError::forbidden("The destination could not be resolved safely.")),
        };
        let fixture = self.fixtures.contains(&origin);
        let permitted = |address: &IpAddr| (fixture && matches!(address.to_string().as_str(), "127.0.0.1" | "::1")) || is_global_ip(*address);
        if answers.is_empty() || !answers.iter().all(permitted) {
            return Err(HttpError::forbidden("The destination resolves to a non-public address."));
        }
        let port = url.port_or_known_default().unwrap_or(80);
        Ok(Resolved { address: answers[0], port, url })
    }
}

impl Destination for NetworkPolicy {
    fn resolve<'a>(&'a self, input: &'a str) -> BoxFuture<'a, HttpResult<Resolved>> {
        Box::pin(NetworkPolicy::resolve(self, input))
    }
}
