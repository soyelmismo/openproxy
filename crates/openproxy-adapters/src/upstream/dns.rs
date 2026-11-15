use std::io;
use std::net::{IpAddr, SocketAddr};

fn is_v4_private_or_reserved(v4: std::net::Ipv4Addr) -> bool {
    let octets = v4.octets();
    octets[0] == 0
        || v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast() // 255.255.255.255
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2) // 192.0.2.0/24 (TEST-NET-1)
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100) // 198.51.100.0/24 (TEST-NET-2)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113) // 203.0.113.0/24 (TEST-NET-3)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0) // 192.0.0.0/24 (RFC 6890 IETF Protocol Assignments)
        || (octets[0] == 100 && octets[1] >= 64 && octets[1] <= 127) // 100.64.0.0/10 (RFC 6598 CGNAT)
        || (octets[0] == 198 && octets[1] >= 18 && octets[1] <= 19) // 198.18.0.0/15 (RFC 2544 Benchmarking)
        || octets[0] >= 240 // 240.0.0.0/4 (RFC 1112 Reserved Class E)
}

fn is_v6_private_or_reserved(v6: &std::net::Ipv6Addr) -> bool {
    // IPv4-mapped (::ffff:a.b.c.d) and IPv4-compatible (::a.b.c.d) forms
    // both decode to an IPv4 address that must be checked against the IPv4
    // private/reserved ranges.
    //
    // Security: previously only `to_ipv4_mapped()` was consulted, which
    // returns `None` for the deprecated IPv4-COMPATIBLE form `::a.b.c.d`
    // (RFC 4291 §2.5.5.1). Rust's `Ipv6Addr::from_str` still parses that
    // form, and so a literal `http://[::127.0.0.1]/` or a hostname that
    // resolves to `::169.254.169.254` slipped through `is_private_or_reserved`
    // and reached `dial_phase` (which applies the same check). That opened
    // a real SSRF bypass to loopback / link-local IMDS / RFC 1918 space.
    // `to_ipv4()` converts both forms (mapped and compatible) and is the
    // correct helper here.
    if let Some(v4) = v6.to_ipv4() {
        return is_v4_private_or_reserved(v4);
    }
    v6.is_unspecified()
        || v6.is_loopback()
        || v6.is_unique_local()
        || v6.is_unicast_link_local()
        || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 Unique Local
        || (v6.segments()[0] == 0x2001 && v6.segments()[1] == 0xdb8) // 2001:db8::/32 Documentation
        || (v6.segments()[0] & 0xff00) == 0xff00 // ff00::/8 Multicast
}

/// Returns `true` for private, reserved, loopback, and link-local IP
/// addresses that should never be the target of an upstream HTTP request
/// (SSRF protection).
pub fn is_private_or_reserved(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_v4_private_or_reserved(*v4),
        IpAddr::V6(v6) => is_v6_private_or_reserved(v6),
    }
}

/// Cache key: the literal `host:port` string.
///
/// Security (audit H-5): the key used to be a 64-bit `DefaultHasher`
/// (SipHash-1-3 with the FIXED default keys) digest of `(host, port)`. Any
/// party able to trigger resolutions — e.g. the `/v1/images` fetch path via
/// `resolve_public_host`, which SEEDS this cache — could precompute, offline
/// and against the public binary, a colliding `(attacker_host, port)` pair
/// for a victim host's digest and seed the victim's entry with
/// attacker-controlled addresses: the next dial to the victim (a plaintext
/// `http://` custom provider, carrying its Bearer credential) would connect
/// to the attacker. A literal string key removes collision space entirely.
pub(crate) fn cache_key(host: &str, port: u16) -> String {
    format!("{host}:{port}")
}

static DNS_CACHE: std::sync::LazyLock<
    dashmap::DashMap<String, (Vec<SocketAddr>, std::time::Instant)>,
> = std::sync::LazyLock::new(dashmap::DashMap::new);
static SWEEP_STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
const DNS_TTL: std::time::Duration = std::time::Duration::from_mins(5);

fn get_cached_dns(cache_key: &str) -> Option<Vec<SocketAddr>> {
    let entry = DNS_CACHE.get(cache_key)?;
    if std::time::Instant::now() < entry.1 {
        Some(entry.0.clone())
    } else {
        None
    }
}

fn ensure_dns_sweep_started() {
    SWEEP_STARTED.get_or_init(|| {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_mins(5));
            tick.tick().await; // skip immediate first tick
            loop {
                tick.tick().await;
                let now = std::time::Instant::now();
                let before = DNS_CACHE.len();
                DNS_CACHE.retain(|_, (_, expiry)| *expiry > now);
                let after = DNS_CACHE.len();
                if before != after {
                    tracing::debug!(before, after, evicted = before - after, "DNS cache sweep");
                }
            }
        });
    });
}

/// Resolve `host:port` and require EVERY address to be public, regardless
/// of `OPENPROXY_ALLOW_PRIVATE_UPSTREAMS` (which relaxes the check for LOOPBACK upstreams only
/// for operator-configured provider upstreams, not for user-supplied URLs).
///
/// The addresses are stored in the shared DNS cache, so a subsequent
/// [`UpstreamClient`](super::UpstreamClient) call for the same
/// `host:port` within the cache TTL dials exactly the addresses that were
/// validated here. That closes the resolve-validate-then-resolve-again
/// TOCTOU/DNS-rebinding window a caller would have if it validated with
/// its own `lookup_host` and then let the connector resolve independently.
///
/// Literal IPs are validated directly (they never touch the cache).
pub async fn resolve_public_host(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    let addrs = resolve_host(host, port).await?;
    if addrs.is_empty() {
        return Err(io::Error::other("host resolved to no addresses"));
    }
    if addrs.iter().any(|a| is_private_or_reserved(&a.ip())) {
        return Err(io::Error::other(
            "private or reserved IP addresses are not allowed",
        ));
    }
    Ok(addrs)
}

/// Resolve `host:port` to one or more `SocketAddr`s using tokio's
/// async DNS, with a simple in-memory cache (5m TTL) to avoid
/// hitting getaddrinfo on every fresh dial.
pub(crate) async fn resolve_host(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    // ROOT FIX (audit A6-01): the env var used to disable the whole
    // private/reserved filter in production builds. It now admits LOOPBACK
    // upstreams only (the documented local-backend use case); RFC1918,
    // link-local metadata and every other reserved range stay blocked.
    let allow_all = cfg!(test) || cfg!(feature = "ssrf-bypass");
    let allow_loopback = !allow_all
        && std::env::var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS")
            .is_ok_and(|v| v == "true" || v == "1");
    let addr_allowed =
        |ip: std::net::IpAddr| !is_private_or_reserved(&ip) || (allow_loopback && ip.is_loopback());

    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if !allow_all && !addr_allowed(ip) {
            return Err(io::Error::other(
                "all resolved addresses are private/reserved (SSRF block). OPENPROXY_ALLOW_PRIVATE_UPSTREAMS admits loopback upstreams only.",
            ));
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    let key = cache_key(host, port);
    if let Some(cached) = get_cached_dns(&key) {
        return Ok(cached);
    }

    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
    if !allow_all {
        for addr in &addrs {
            if !addr_allowed(addr.ip()) {
                return Err(io::Error::other(
                    "all resolved addresses are private/reserved (SSRF block). OPENPROXY_ALLOW_PRIVATE_UPSTREAMS admits loopback upstreams only.",
                ));
            }
        }
    }
    let now = std::time::Instant::now();
    DNS_CACHE.insert(key, (addrs.clone(), now + DNS_TTL));
    ensure_dns_sweep_started();

    Ok(addrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_public_host_rejects_private_literals_even_in_test_builds() {
        // `cfg!(test)` relaxes `resolve_host`'s own SSRF gate; the public
        // variant must still refuse private/reserved targets.
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "::1",
            "fd00::1",
        ] {
            let err = resolve_public_host(bad, 80).await.expect_err(bad);
            assert!(
                err.to_string().contains("private or reserved"),
                "{bad}: {err}"
            );
        }
    }

    #[tokio::test]
    async fn resolve_public_host_accepts_public_literals() {
        let addrs = resolve_public_host("1.1.1.1", 443)
            .await
            .expect("public ip");
        assert_eq!(addrs, vec!["1.1.1.1:443".parse::<SocketAddr>().unwrap()]);
    }
}
