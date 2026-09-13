//! URL validation for source queues and discovery fetches.
//!
//! Curator is an administrator-facing local application, so accepting an
//! arbitrary URL must not turn gallery discovery into a shortcut to loopback,
//! link-local, private-LAN, or Tailnet services. The checks are deliberately
//! shared by direct URL search, queued sources, and redirect handling.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use reqwest::redirect::Policy;
use reqwest::Url;

use crate::slug::normalize_url;

pub fn normalize_public_http_url(raw: &str) -> Result<String, String> {
    let normalized = normalize_url(raw);
    if normalized.is_empty() {
        return Err("A URL is required".to_string());
    }
    let url = Url::parse(&normalized).map_err(|_| "Invalid URL".to_string())?;
    validate_public_http_url(&url)?;
    Ok(url.to_string().trim_end_matches('/').to_string())
}

pub fn validate_public_http_url(url: &Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err("Only http and https source URLs are allowed".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Source URLs must not contain credentials".to_string());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "Source URL must include a host".to_string())?
        .trim_start_matches('[')
        .trim_end_matches(']');
    validate_public_host(host)
}

pub fn validate_public_host(host: &str) -> Result<(), String> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty()
        || host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host == "ip6-localhost"
    {
        return Err("Loopback and local-network hostnames are not allowed".to_string());
    }
    if let Ok(address) = host.parse::<IpAddr>() {
        if !is_public_ip(address) {
            return Err(
                "Private, loopback, link-local, and Tailnet addresses are not allowed".to_string(),
            );
        }
    }
    Ok(())
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, _, _] = address.octets();
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_multicast()
        // Tailscale uses this CGNAT range. Treat it as private even though it
        // is not part of RFC1918, so a source can never probe another Tailnet
        // device through Curator.
        || (a == 100 && (64..=127).contains(&b))
        // Benchmark, documentation, and future-use blocks do not belong in a
        // downloader queue either.
        || (a == 198 && (18..=19).contains(&b))
        || (a == 192 && (b == 0 || b == 2 || b == 168))
        || (a == 198 && b == 51)
        || (a == 203 && b == 0)
        || a >= 224)
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    let first = address.octets()[0];
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || address.is_unicast_link_local()
        // fc00::/7 is private/unique local addressing, including many VPNs.
        || (first & 0xfe) == 0xfc)
}

/// Reqwest redirects are checked before following them. This protects the
/// one built-in HTTP discovery adapter from public-to-private redirect tricks.
/// Gallery-dl URLs are additionally constrained at queue creation time.
pub fn public_redirect_policy() -> Policy {
    Policy::custom(|attempt| {
        if validate_public_http_url(attempt.url()).is_ok() {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permits_normal_public_page_urls() {
        assert_eq!(
            normalize_public_http_url("example.com/post/1").unwrap(),
            "https://example.com/post/1"
        );
    }

    #[test]
    fn rejects_private_and_loopback_targets() {
        for value in [
            "http://127.0.0.1:8080/",
            "http://localhost/",
            "http://192.168.1.2/",
            "http://169.254.1.2/",
            "http://100.64.0.1/",
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[fd7a:115c:a1e0::1]/",
        ] {
            assert!(normalize_public_http_url(value).is_err(), "{value}");
        }
    }

    #[test]
    fn rejects_non_http_and_credential_urls() {
        assert!(normalize_public_http_url("file:///C:/secret").is_err());
        assert!(normalize_public_http_url("https://user:pass@example.com/").is_err());
    }
}
