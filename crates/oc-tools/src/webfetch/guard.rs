//! SSRF guard: scheme/credential checks, DNS resolution once per hop, address
//! vetting and DNS pinning.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use tokio::net::lookup_host;
use url::{Host, Url};

use crate::ToolError;

/// A request target whose addresses have been checked and are pinned into the
/// HTTP client, so the connection cannot be redirected by a second DNS answer.
#[derive(Debug)]
pub(super) struct Target {
    pub(super) url: Url,
    /// Lowercase DNS name to pin; `None` for literal IP hosts.
    domain: Option<String>,
    port: u16,
    /// Validated addresses; at least one. Deduplicated and sorted.
    addresses: Vec<IpAddr>,
}

/// Validate scheme, credentials, and host, resolving DNS once and returning
/// the vetted addresses for pinning.
pub(super) async fn validate_target(url: &Url, display: &str) -> Result<Target, ToolError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ToolError::new(format!(
            "URL must use http:// or https://: {display}"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ToolError::new(format!(
            "URL must not include credentials: {display}"
        )));
    }
    let host = url
        .host()
        .ok_or_else(|| ToolError::new(format!("URL has no host: {display}")))?;
    let port = url.port_or_known_default().unwrap_or(80);
    let (domain, mut addresses) = match host {
        Host::Ipv4(address) => (None, vec![IpAddr::V4(address)]),
        Host::Ipv6(address) => (None, vec![IpAddr::V6(address)]),
        Host::Domain(domain) => {
            let normalized = domain.trim_end_matches('.').to_ascii_lowercase();
            if normalized == "localhost" || normalized.ends_with(".localhost") {
                return Err(blocked(display, "localhost"));
            }
            let mut addresses = Vec::new();
            let resolved = lookup_host((normalized.as_str(), port))
                .await
                .map_err(|error| {
                    ToolError::new(format!("Unable to resolve host for {display}: {error}"))
                })?;
            for address in resolved {
                addresses.push(address.ip());
            }
            (Some(normalized), addresses)
        }
    };
    if addresses.is_empty() {
        return Err(ToolError::new(format!(
            "Unable to resolve host for {display}: no addresses"
        )));
    }
    addresses.sort_unstable();
    addresses.dedup();
    for address in &addresses {
        check_ip(*address, display)?;
    }
    Ok(Target {
        url: url.clone(),
        domain,
        port,
        addresses,
    })
}

/// DNS pins to install on the client builder: the lowercase domain and every
/// vetted address at the request port.
pub(super) fn dns_pins(target: &Target) -> Option<(&str, Vec<SocketAddr>)> {
    let domain = target.domain.as_deref()?;
    let pins = target
        .addresses
        .iter()
        .map(|address| SocketAddr::new(*address, target.port))
        .collect();
    Some((domain, pins))
}

/// Reject a connection that landed on an address other than a vetted one.
pub(super) fn verify_remote(
    target: &Target,
    response: &reqwest::Response,
) -> Result<(), ToolError> {
    let Some(remote) = response.remote_addr() else {
        return Ok(());
    };
    if target.addresses.contains(&remote.ip()) {
        return Ok(());
    }
    Err(ToolError::new(format!(
        "Refusing to read {}: connected to unexpected address {remote}",
        target.url
    )))
}

fn check_ip(ip: IpAddr, display: &str) -> Result<(), ToolError> {
    if is_blocked_ip(ip) {
        return Err(blocked(display, &ip.to_string()));
    }
    Ok(())
}

fn blocked(display: &str, host: &str) -> ToolError {
    ToolError::new(format!(
        "Refusing to fetch {display}: host `{host}` is blocked (loopback, private, link-local, multicast, metadata, or otherwise non-public)"
    ))
}

/// Whether an address must never be reached by `webfetch`.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(address) => is_blocked_v4(address),
        IpAddr::V6(address) => is_blocked_v6(address),
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    let [first, second, third, _] = ip.octets();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        // 0.0.0.0/8 "this network"
        || first == 0
        // 100.64.0.0/10 carrier-grade NAT
        || (first == 100 && (64..=127).contains(&second))
        // 192.0.0.0/24 IETF protocol assignments
        || (first == 192 && second == 0 && third == 0)
        // 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24 documentation
        || (first == 192 && second == 0 && third == 2)
        || (first == 198 && second == 51 && third == 100)
        || (first == 203 && second == 0 && third == 113)
        // 192.88.99.0/24 6to4 relay anycast
        || (first == 192 && second == 88 && third == 99)
        // 198.18.0.0/15 benchmarking
        || (first == 198 && (second == 18 || second == 19))
        // 240.0.0.0/4 reserved
        || first >= 240
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return true;
    }
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_blocked_v4(mapped);
    }
    let segments = ip.segments();
    // IPv4-compatible `::a.b.c.d`
    if segments[..6].iter().all(|segment| *segment == 0) {
        return is_blocked_v4(embedded_ipv4(segments[6], segments[7]));
    }
    // fc00::/7 unique local
    if (segments[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    // fec0::/10 deprecated site-local
    if (segments[0] & 0xffc0) == 0xfec0 {
        return true;
    }
    // fe80::/10 link-local
    if (segments[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // 2002::/16 6to4 and 2001::/32 Teredo tunnel arbitrary IPv4 payloads.
    if segments[0] == 0x2002 || (segments[0] == 0x2001 && segments[1] == 0) {
        return true;
    }
    // 2001:db8::/32 documentation
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return true;
    }
    // 64:ff9b::/96 NAT64 embeds an IPv4 address.
    if segments[0] == 0x0064
        && segments[1] == 0xff9b
        && segments[2..6].iter().all(|segment| *segment == 0)
    {
        return is_blocked_v4(embedded_ipv4(segments[6], segments[7]));
    }
    // 100::/64 discard-only
    segments[0] == 0x0100 && segments[1..4].iter().all(|segment| *segment == 0)
}

fn embedded_ipv4(high: u16, low: u16) -> Ipv4Addr {
    let [first, second] = high.to_be_bytes();
    let [third, fourth] = low.to_be_bytes();
    Ipv4Addr::new(first, second, third, fourth)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use super::super::fetch::build_client;

    fn make_target(domain: Option<&str>, addresses: Vec<IpAddr>, port: u16) -> Target {
        Target {
            url: Url::parse("http://example.com/").expect("test url"),
            domain: domain.map(str::to_string),
            port,
            addresses,
        }
    }

    #[test]
    fn dns_pins_cover_every_vetted_address() {
        let target = make_target(
            Some("example.com"),
            vec![
                IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
                IpAddr::V4(Ipv4Addr::new(93, 184, 216, 35)),
            ],
            8080,
        );
        let (domain, pins) = dns_pins(&target).expect("domain target should be pinned");
        assert_eq!(domain, "example.com");
        assert_eq!(pins.len(), 2);
        assert!(pins.contains(&SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
            8080
        )));
        assert!(pins.contains(&SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(93, 184, 216, 35)),
            8080
        )));
        // The builder path consumes those pins.
        assert!(build_client(Duration::from_secs(1), &target).is_ok());
    }

    #[test]
    fn literal_targets_need_no_dns_pin() {
        let target = make_target(None, vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))], 80);
        assert!(dns_pins(&target).is_none());
        assert!(build_client(Duration::from_secs(1), &target).is_ok());
    }

    #[test]
    fn deprecated_site_local_ipv6_is_blocked() {
        assert!(is_blocked_ip("fec0::1".parse().expect("ip")));
        assert!(is_blocked_ip("fef0::1".parse().expect("ip")));
        assert!(!is_blocked_ip("2606:4700::1111".parse().expect("ip")));
    }

    #[tokio::test]
    async fn validate_target_blocks_localhost_and_literals_without_dns() {
        let url = Url::parse("http://localhost:8080/").expect("url");
        let error = validate_target(&url, url.as_str()).await.unwrap_err();
        assert!(error.message.contains("blocked"), "{}", error.message);

        for text in ["http://169.254.169.254/", "http://[fec0::1]/"] {
            let url = Url::parse(text).expect("url");
            let error = validate_target(&url, text).await.unwrap_err();
            assert!(error.message.contains("blocked"), "{}", error.message);
        }
    }
}
