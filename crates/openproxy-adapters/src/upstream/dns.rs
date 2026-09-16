use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::net::{IpAddr, SocketAddr};

fn is_v4_private_or_reserved(v4: std::net::Ipv4Addr) -> bool {
    let octets = v4.octets();
    octets[0] == 0
        || v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2) // 192.0.2.0/24 (TEST-NET-1)
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100) // 198.51.100.0/24 (TEST-NET-2)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113) // 203.0.113.0/24 (TEST-NET-3)
}

fn is_v6_private_or_reserved(v6: &std::net::Ipv6Addr) -> bool {
    if let Some(v4) = v6.to_ipv4_mapped() {
        return is_v4_private_or_reserved(v4);
    }
    v6.is_loopback()
        || v6.is_unique_local()
        || v6.is_unicast_link_local()
        || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 Unique Local
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

pub(crate) fn cache_key(host: &str, port: u16) -> u64 {
    let mut hasher = DefaultHasher::new();
    host.hash(&mut hasher);
    port.hash(&mut hasher);
    hasher.finish()
}

static DNS_CACHE: std::sync::LazyLock<
    dashmap::DashMap<u64, (Vec<SocketAddr>, std::time::Instant)>,
> = std::sync::LazyLock::new(dashmap::DashMap::new);
static SWEEP_STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
const DNS_TTL: std::time::Duration = std::time::Duration::from_mins(5);

fn get_cached_dns(cache_key: u64) -> Option<Vec<SocketAddr>> {
    let entry = DNS_CACHE.get(&cache_key)?;
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

/// Resolve `host:port` to one or more `SocketAddr`s using tokio's
/// async DNS, with a simple in-memory cache (5m TTL) to avoid
/// hitting getaddrinfo on every fresh dial.
pub(crate) async fn resolve_host(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    let allow_private = cfg!(test)
        || cfg!(feature = "ssrf-bypass")
        || std::env::var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS")
            .is_ok_and(|v| v == "true" || v == "1");

    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if !allow_private && is_private_or_reserved(&ip) {
            return Err(io::Error::other(
                "all resolved addresses are private/reserved (SSRF block). Set OPENPROXY_ALLOW_PRIVATE_UPSTREAMS=true to allow.",
            ));
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    let key = cache_key(host, port);
    if let Some(cached) = get_cached_dns(key) {
        return Ok(cached);
    }

    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
    if !allow_private {
        for addr in &addrs {
            if is_private_or_reserved(&addr.ip()) {
                return Err(io::Error::other(
                    "all resolved addresses are private/reserved (SSRF block). Set OPENPROXY_ALLOW_PRIVATE_UPSTREAMS=true to allow.",
                ));
            }
        }
    }
    let now = std::time::Instant::now();
    DNS_CACHE.insert(key, (addrs.clone(), now + DNS_TTL));
    ensure_dns_sweep_started();

    Ok(addrs)
}
