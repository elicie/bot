//! Anonymous metadata HTTP transport. Resolved addresses are checked and installed into the
//! client's resolver before connecting; the original URL still determines TLS/SNI verification.

use std::net::{IpAddr, SocketAddr};

use reqwest::header::{ACCEPT, CONTENT_TYPE};

use super::{BoxFuture, DiscoveryError, Fetch, REQUEST_TIMEOUT, Response, StreamExt, Url, is_global_ip};

const MAX_DNS_ANSWERS: usize = 64;

pub(super) struct Production;

impl Fetch for Production {
    fn get<'a>(&'a self, url: &'a Url) -> BoxFuture<'a, Result<Response, DiscoveryError>> {
        Box::pin(async move {
            let host = url.host_str().ok_or(DiscoveryError::InvalidUrl)?;
            let port = url.port_or_known_default().ok_or(DiscoveryError::InvalidUrl)?;
            let addresses = match url.host() {
                Some(url::Host::Ipv4(ip)) => vec![SocketAddr::new(IpAddr::V4(ip), port)],
                Some(url::Host::Ipv6(ip)) => vec![SocketAddr::new(IpAddr::V6(ip), port)],
                Some(url::Host::Domain(_)) => {
                    tokio::net::lookup_host((host, port)).await.map_err(|_| DiscoveryError::FetchFailed)?.take(MAX_DNS_ANSWERS + 1).collect()
                }
                None => return Err(DiscoveryError::InvalidUrl),
            };
            validate_addresses(&addresses)?;
            let client = reqwest::Client::builder()
                .https_only(true)
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(REQUEST_TIMEOUT)
                .connect_timeout(REQUEST_TIMEOUT)
                .resolve_to_addrs(host, &addresses)
                .build()
                .map_err(|_| DiscoveryError::FetchFailed)?;
            // No caller-provided client/headers, ambient proxy, cookie store, or credentials.
            let response = client.get(url.clone()).header(ACCEPT, "application/json").send().await.map_err(request_error)?;
            let status = response.status().as_u16();
            let json = response.headers().get(CONTENT_TYPE).and_then(|value| value.to_str().ok()).is_some_and(is_json);
            let content_length = response.content_length();
            let body = response.bytes_stream().map(|chunk| chunk.map(|bytes| bytes.to_vec()).map_err(request_error)).boxed();
            Ok(Response { status, json, content_length, body })
        })
    }
}

fn request_error(error: reqwest::Error) -> DiscoveryError {
    if error.is_timeout() { DiscoveryError::Timeout } else { DiscoveryError::FetchFailed }
}

fn is_json(content_type: &str) -> bool {
    content_type.split(';').next().is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
}

fn validate_addresses(addresses: &[SocketAddr]) -> Result<(), DiscoveryError> {
    if addresses.len() > MAX_DNS_ANSWERS {
        return Err(DiscoveryError::LimitExceeded);
    }
    if addresses.is_empty() || addresses.iter().any(|address| !is_global_ip(address.ip())) {
        return Err(DiscoveryError::BlockedAddress);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_dns_answer_must_be_public() {
        let public: SocketAddr = "1.1.1.1:443".parse().unwrap();
        let public_v6: SocketAddr = "[2606:4700:4700::1111]:443".parse().unwrap();
        assert_eq!(validate_addresses(&[public, public_v6]), Ok(()));
        assert_eq!(validate_addresses(&[]), Err(DiscoveryError::BlockedAddress));
        for address in ["127.0.0.1:443", "10.0.0.1:443", "169.254.169.254:443", "[::1]:443", "[::ffff:127.0.0.1]:443", "[2001:db8::1]:443"] {
            let blocked = address.parse().unwrap();
            assert_eq!(validate_addresses(&[blocked]), Err(DiscoveryError::BlockedAddress));
            assert_eq!(validate_addresses(&[public, blocked]), Err(DiscoveryError::BlockedAddress));
            assert_eq!(validate_addresses(&[blocked, public]), Err(DiscoveryError::BlockedAddress));
        }
        assert_eq!(validate_addresses(&vec![public; MAX_DNS_ANSWERS + 1]), Err(DiscoveryError::LimitExceeded));
    }

    #[test]
    fn metadata_media_type_is_json_not_html_or_arbitrary_suffix() {
        for value in ["application/json", "Application/JSON; charset=utf-8", "application/json ; charset=UTF-8"] {
            assert!(is_json(value));
        }
        for value in ["text/html", "application/jsonp", "text/json", "application/problem+json", ""] {
            assert!(!is_json(value));
        }
    }
}
