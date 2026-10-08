//! Real client IP resolution for requests behind reverse proxies.
//!
//! When openproxy runs behind a reverse proxy (such as Nginx, HAProxy, or Envoy),
//! the raw TCP socket address (`ConnectInfo`) sees only the proxy's IP (typically
//! loopback `127.0.0.1` or a private gateway).
//!
//! To prevent IP spoofing, proxy forwarding headers (`X-Real-IP`, `X-Forwarded-For`,
//! and RFC 7239 `Forwarded`) are ONLY inspected when the immediate TCP peer address
//! is a trusted proxy. By default, all loopback addresses (`127.0.0.0/8` and `::1`)
//! are trusted. Additional IP addresses or CIDRs can be configured in
//! `server.trusted_proxies`.

use axum::http::HeaderMap;
use std::net::{IpAddr, SocketAddr};

/// Check if an IP address belongs to a CIDR range (e.g. `10.0.0.0/8`, `172.16.0.0/12`)
/// or matches an exact IP string.
pub fn is_ip_in_cidr(ip: IpAddr, cidr: &str) -> bool {
    let cidr = cidr.trim();
    let Some((net_str, prefix_str)) = cidr.split_once('/') else {
        return cidr.parse::<IpAddr>() == Ok(ip);
    };

    let Ok(prefix_len) = prefix_str.parse::<u8>() else {
        return false;
    };

    match (ip, net_str.parse::<IpAddr>()) {
        (IpAddr::V4(ip4), Ok(IpAddr::V4(net4))) if prefix_len <= 32 => {
            let mask = if prefix_len == 0 {
                0
            } else {
                !0u32 << (32 - prefix_len)
            };
            (u32::from(ip4) & mask) == (u32::from(net4) & mask)
        }
        (IpAddr::V6(ip6), Ok(IpAddr::V6(net6))) if prefix_len <= 128 => {
            let mask = if prefix_len == 0 {
                0
            } else {
                !0u128 << (128 - prefix_len)
            };
            (u128::from(ip6) & mask) == (u128::from(net6) & mask)
        }
        _ => false,
    }
}

/// Check if a peer IP address is trusted to forward client IP headers.
///
/// Security: loopback addresses (`127.0.0.0/8` and `::1`) are NOT trusted
/// by default. An operator who runs openproxy behind a reverse proxy on
/// the same host (the common nginx-on-127.0.0.1 deployment) MUST list the
/// proxy's IP in `server.trusted_proxies` (e.g. `["127.0.0.1", "::1"]`).
/// This closes the per-IP rate-limit / admin-auth-throttle bypass in which
/// any local user could spoof `X-Forwarded-For` / `X-Real-IP` and evade
/// per-IP buckets.
pub fn is_trusted_proxy(ip: IpAddr, trusted_proxies: &[String]) -> bool {
    trusted_proxies.iter().any(|entry| is_ip_in_cidr(ip, entry))
}

/// Parse an IP string which may be a bare IP (`192.0.2.1`), bracketed IPv6
/// (`[2001:db8::1]`), or include a port (`192.0.2.1:8080`, `[2001:db8::1]:443`).
pub fn parse_ip_or_bracketed(val: &str) -> Option<IpAddr> {
    let val = val.trim().trim_matches('"');
    if val.is_empty() {
        return None;
    }
    // Bracketed IPv6: [2001:db8::1] or [2001:db8::1]:8080
    if let Some(rest) = val.strip_prefix('[')
        && let Some((ipv6_str, _)) = rest.split_once(']')
    {
        return ipv6_str.parse::<IpAddr>().ok();
    }
    // SocketAddr: e.g. 192.0.2.1:8080
    if let Ok(socket) = val.parse::<SocketAddr>() {
        return Some(socket.ip());
    }
    // Bare IpAddr
    val.parse::<IpAddr>().ok()
}

/// Resolve the real client IP address from request headers and TCP peer address.
///
/// If `peer_addr` is trusted (loopback or in `trusted_proxies`):
/// 1. `X-Real-IP` header (single value overwritten by the proxy).
/// 2. **Rightmost-untrusted** IP in `X-Forwarded-For`: each proxy appends
///    the address it received the request from, so the rightmost entry NOT
///    belonging to a trusted proxy is the client. The leftmost entry is
///    fully client-controlled and must never be used (OP-08: spoofing of
///    audit-log IPs and per-IP rate-limit buckets).
/// 3. Last `for=` entry in RFC 7239 `Forwarded` (same rightmost logic).
/// 4. Fallback: `peer_addr` IP.
///
/// If `peer_addr` is not trusted, returns `peer_addr` IP directly (anti-spoofing).
pub fn resolve_client_ip(
    headers: &HeaderMap,
    peer_addr: Option<&SocketAddr>,
    trusted_proxies: &[String],
) -> Option<IpAddr> {
    let peer_ip = peer_addr.map(|a| a.ip());
    let is_trusted = peer_ip.is_some_and(|ip| is_trusted_proxy(ip, trusted_proxies));

    if is_trusted {
        // 1. X-Real-IP: a single value your proxy overwrites on every hop.
        if let Some(real_ip) = headers
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_ip_or_bracketed)
        {
            return Some(real_ip);
        }

        // 2. X-Forwarded-For, rightmost-untrusted (see doc comment).
        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            let entries: Vec<Option<IpAddr>> = xff.split(',').map(parse_ip_or_bracketed).collect();
            if let Some(ip) = rightmost_untrusted(&entries, trusted_proxies) {
                return Some(ip);
            }
        }

        // 3. RFC 7239 Forwarded: for=192.0.2.60;proto=http;by=203.0.113.43 —
        //    multiple comma-separated sections may each carry a for=; the
        //    rightmost non-trusted one wins, mirroring XFF semantics.
        if let Some(forwarded) = headers.get("forwarded").and_then(|v| v.to_str().ok()) {
            let entries: Vec<Option<IpAddr>> = forwarded
                .split(',')
                .filter_map(|section| {
                    section.split(';').find_map(|part| {
                        part.trim()
                            .strip_prefix("for=")
                            .and_then(parse_ip_or_bracketed)
                    })
                })
                .map(Some)
                .collect();
            if let Some(ip) = rightmost_untrusted(&entries, trusted_proxies) {
                return Some(ip);
            }
        }
    }

    peer_ip
}

/// Walk `entries` right-to-left and return the first address that is NOT a
/// trusted proxy; `None` when every entry is trusted or nothing parses.
fn rightmost_untrusted(entries: &[Option<IpAddr>], trusted_proxies: &[String]) -> Option<IpAddr> {
    entries
        .iter()
        .rev()
        .find_map(|entry| entry.filter(|ip| !is_trusted_proxy(*ip, trusted_proxies)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn test_is_ip_in_cidr() {
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50));
        assert!(is_ip_in_cidr(ip, "192.168.1.0/24"));
        assert!(is_ip_in_cidr(ip, "192.168.0.0/16"));
        assert!(is_ip_in_cidr(ip, "192.168.1.50"));
        assert!(!is_ip_in_cidr(ip, "192.168.2.0/24"));
        assert!(!is_ip_in_cidr(ip, "10.0.0.0/8"));

        let ipv6 = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
        assert!(is_ip_in_cidr(ipv6, "2001:db8::/32"));
        assert!(is_ip_in_cidr(ipv6, "2001:db8::1"));
        assert!(!is_ip_in_cidr(ipv6, "2001:db9::/32"));
    }

    #[test]
    fn test_is_ip_in_cidr_edge_cases() {
        let ip4 = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50));
        let ip6 = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));

        // Prefix length 0 matches all addresses of the same family
        assert!(is_ip_in_cidr(ip4, "0.0.0.0/0"));
        assert!(is_ip_in_cidr(ip6, "::/0"));

        // Invalid prefix length range (>32 for IPv4, >128 for IPv6)
        assert!(!is_ip_in_cidr(ip4, "192.168.1.0/33"));
        assert!(!is_ip_in_cidr(ip6, "2001:db8::/129"));

        // Non-numeric prefix length
        assert!(!is_ip_in_cidr(ip4, "192.168.1.0/abc"));

        // IP version mismatch
        assert!(!is_ip_in_cidr(ip4, "2001:db8::/32"));
        assert!(!is_ip_in_cidr(ip6, "192.168.1.0/24"));
    }

    #[test]
    fn test_is_trusted_proxy_loopback() {
        let local_v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let local_v4_alias = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 253));
        let local_v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        let public_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1));

        // Security: loopback is NOT trusted by default. Operators who want
        // X-Forwarded-For / X-Real-IP honored must explicitly list the
        // proxy's IP in `server.trusted_proxies`.
        let empty_trusted: Vec<String> = Vec::new();
        assert!(!is_trusted_proxy(local_v4, &empty_trusted));
        assert!(!is_trusted_proxy(local_v4_alias, &empty_trusted));
        assert!(!is_trusted_proxy(local_v6, &empty_trusted));
        assert!(!is_trusted_proxy(public_ip, &empty_trusted));

        // Explicit opt-in: list loopback (or any CIDR) and it is trusted.
        let loopback_trusted = vec!["127.0.0.0/8".to_string(), "::1".to_string()];
        assert!(is_trusted_proxy(local_v4, &loopback_trusted));
        assert!(is_trusted_proxy(local_v4_alias, &loopback_trusted));
        assert!(is_trusted_proxy(local_v6, &loopback_trusted));
        assert!(!is_trusted_proxy(public_ip, &loopback_trusted));

        let custom_trusted = vec!["203.0.113.0/24".to_string()];
        assert!(is_trusted_proxy(public_ip, &custom_trusted));
    }

    #[test]
    fn test_parse_ip_or_bracketed() {
        assert_eq!(
            parse_ip_or_bracketed("192.0.2.1"),
            Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)))
        );
        assert_eq!(
            parse_ip_or_bracketed("192.0.2.1:8080"),
            Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)))
        );
        assert_eq!(
            parse_ip_or_bracketed("\"192.0.2.1\""),
            Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)))
        );
        assert_eq!(
            parse_ip_or_bracketed("[2001:db8::1]"),
            Some(IpAddr::V6("2001:db8::1".parse().unwrap()))
        );
        assert_eq!(
            parse_ip_or_bracketed("[2001:db8::1]:443"),
            Some(IpAddr::V6("2001:db8::1".parse().unwrap()))
        );
        assert_eq!(parse_ip_or_bracketed("invalid"), None);
        assert_eq!(parse_ip_or_bracketed(""), None);
    }

    #[test]
    fn test_resolve_client_ip_behind_nginx() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "198.51.100.42".parse().unwrap());
        headers.insert(
            "x-forwarded-for",
            "198.51.100.42, 127.0.0.1".parse().unwrap(),
        );

        let peer = "127.0.0.253:8787".parse::<SocketAddr>().unwrap();
        // Security: nginx-on-loopback deployment requires explicit opt-in.
        let trusted = vec!["127.0.0.0/8".to_string()];

        let resolved = resolve_client_ip(&headers, Some(&peer), &trusted);
        assert_eq!(resolved, Some(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 42))));
    }

    #[test]
    fn test_resolve_client_ip_anti_spoofing() {
        // Direct request from untrusted IP pretending to be another IP
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "1.1.1.1".parse().unwrap());
        headers.insert("x-forwarded-for", "1.1.1.1".parse().unwrap());

        let untrusted_peer = "203.0.113.50:12345".parse::<SocketAddr>().unwrap();
        let trusted: Vec<String> = Vec::new();

        let resolved = resolve_client_ip(&headers, Some(&untrusted_peer), &trusted);
        // Must ignore spoofed headers and return the untrusted peer's actual IP
        assert_eq!(resolved, Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 50))));
    }

    /// OP-08: a client behind a trusted proxy spoofs a leftmost XFF entry
    /// (`X-Forwarded-For: 1.2.3.4`); the proxy appends the real client IP.
    /// The rightmost-untrusted entry must win — the leftmost is dead simple
    /// for the client to forge.
    #[test]
    fn test_resolve_client_ip_xff_spoofed_leftmost_ignored() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4, 198.51.100.42".parse().unwrap());
        let peer = "127.0.0.1:8787".parse::<SocketAddr>().unwrap();
        // Explicit opt-in: 127.0.0.0/8 is trusted.
        let trusted = vec!["127.0.0.0/8".to_string()];

        let resolved = resolve_client_ip(&headers, Some(&peer), &trusted);
        assert_eq!(resolved, Some(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 42))));
    }

    /// OP-08: multi-hop chain — trusted proxies are skipped from the right
    /// until the first untrusted address.
    #[test]
    fn test_resolve_client_ip_xff_multihop_skips_trusted() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "1.2.3.4, 198.51.100.42, 10.0.0.7".parse().unwrap(),
        );
        let peer = "10.0.0.9:8787".parse::<SocketAddr>().unwrap();
        let trusted = vec!["10.0.0.0/8".to_string()];

        let resolved = resolve_client_ip(&headers, Some(&peer), &trusted);
        assert_eq!(resolved, Some(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 42))));
    }

    /// OP-08: every XFF entry is a trusted proxy → fall back to the peer.
    #[test]
    fn test_resolve_client_ip_xff_all_trusted_falls_back_to_peer() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "127.0.0.2, 127.0.0.3".parse().unwrap());
        let peer = "127.0.0.1:8787".parse::<SocketAddr>().unwrap();
        let trusted = vec!["127.0.0.0/8".to_string()];

        let resolved = resolve_client_ip(&headers, Some(&peer), &trusted);
        assert_eq!(resolved, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }

    /// OP-08: RFC 7239 Forwarded uses the rightmost for= entry as well.
    #[test]
    fn test_resolve_client_ip_forwarded_rightmost() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "forwarded",
            "for=1.2.3.4, for=198.51.100.99".parse().unwrap(),
        );
        let peer = "127.0.0.1:8787".parse::<SocketAddr>().unwrap();
        let trusted = vec!["127.0.0.0/8".to_string()];

        let resolved = resolve_client_ip(&headers, Some(&peer), &trusted);
        assert_eq!(resolved, Some(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 99))));
    }
}
