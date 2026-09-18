//! Provider registry CRUD and domain utilities.

use std::net::IpAddr;

use crate::error::Result;
use crate::ids::ProviderId;
pub use openproxy_db::providers::{
    NewProvider, UpdateProviderParams, create, delete, delete_provider_favicon, get,
    get_auth_types, get_provider_favicon, list, list_active, set_active, set_provider_favicon,
    update, update_current_proxy,
};
pub use openproxy_types::providers::*;

pub use crate::seed::{builtin_provider_ids, is_builtin};

/// Extract clean domain/host from a base_url string.
pub fn extract_domain(base_url: &str) -> Option<String> {
    let trimmed = base_url.trim();
    let stripped = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    let host_part = stripped.split_once('/').map_or(stripped, |(h, _)| h);
    let host = if host_part.starts_with('[') {
        host_part
            .strip_prefix('[')
            .and_then(|s| s.split_once(']'))
            .map_or(host_part, |(h, _)| h)
            .trim()
    } else {
        host_part
            .split_once(':')
            .map_or(host_part, |(h, _)| h)
            .trim()
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Returns true if the host is a loopback, local, unspecified, or private address.
pub fn is_loopback_or_private_host(host: &str) -> bool {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    if host.is_empty()
        || host.eq_ignore_ascii_case("localhost")
        || host.rsplit_once('.').is_some_and(|(_, tld)| {
            tld.eq_ignore_ascii_case("local") || tld.eq_ignore_ascii_case("localhost")
        })
    {
        return true;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(ipv4) => {
                ipv4.is_loopback()
                    || ipv4.is_private()
                    || ipv4.is_link_local()
                    || ipv4.is_unspecified()
                    || ipv4.is_broadcast()
                    || ipv4.is_documentation()
                    || (ipv4.octets()[0] == 100 && (ipv4.octets()[1] & 0xC0) == 64)
            }
            IpAddr::V6(ipv6) => {
                if let Some(ipv4) = ipv6.to_ipv4_mapped() {
                    return ipv4.is_loopback()
                        || ipv4.is_private()
                        || ipv4.is_link_local()
                        || ipv4.is_unspecified()
                        || ipv4.is_broadcast()
                        || ipv4.is_documentation()
                        || (ipv4.octets()[0] == 100 && (ipv4.octets()[1] & 0xC0) == 64);
                }
                ipv6.is_loopback()
                    || ipv6.is_unspecified()
                    || ((ipv6.segments()[0] & 0xfe00) == 0xfc00)
                    || ((ipv6.segments()[0] & 0xffc0) == 0xfe80)
            }
        };
    }
    false
}

fn is_compound_tld(second_to_last: &str, last: &str) -> bool {
    matches!(second_to_last, "co" | "com" | "org" | "net" | "gov" | "edu") && last.len() == 2
}

/// Extract apex/root domain from host (e.g. "api.fireworks.ai" -> "fireworks.ai").
pub fn extract_apex_domain(host: &str) -> String {
    if host.parse::<IpAddr>().is_ok() {
        return host.to_string();
    }
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() <= 2 {
        return host.to_string();
    }
    let second_to_last = parts[parts.len() - 2];
    let last = parts[parts.len() - 1];
    if is_compound_tld(second_to_last, last) && parts.len() >= 3 {
        parts[parts.len() - 3..].join(".")
    } else {
        parts[parts.len() - 2..].join(".")
    }
}

/// Maximum allowed favicon size (64 KiB) to prevent memory bloat and DoS.
pub const MAX_FAVICON_BYTES: usize = 64 * 1024;

/// Inspects binary bytes and returns the detected image MIME type if valid.
/// Rejects non-images, HTML documents, and payloads exceeding 64 KiB.
pub fn validate_favicon_bytes(bytes: &[u8]) -> Option<&'static str> {
    if bytes.is_empty() || bytes.len() > MAX_FAVICON_BYTES {
        return None;
    }

    // Reject HTML documents (case-insensitive check on ASCII prefix)
    let check_len = bytes.len().min(128);
    let prefix = &bytes[..check_len];
    if prefix
        .windows(9)
        .any(|w| w.eq_ignore_ascii_case(b"<!doctype"))
        || prefix.windows(5).any(|w| w.eq_ignore_ascii_case(b"<html"))
        || prefix
            .windows(9)
            .any(|w| w.eq_ignore_ascii_case(b"text/html"))
    {
        return None;
    }

    // Magic byte checks:
    // PNG: \x89PNG
    if bytes.starts_with(b"\x89PNG") {
        return Some("image/png");
    }

    // ICO: 00 00 01 00 (icon) or 00 00 02 00 (cursor)
    if bytes.starts_with(b"\x00\x00\x01\x00") || bytes.starts_with(b"\x00\x00\x02\x00") {
        return Some("image/x-icon");
    }

    // GIF: GIF8
    if bytes.starts_with(b"GIF8") {
        return Some("image/gif");
    }

    // JPEG: \xff\xd8\xff
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some("image/jpeg");
    }

    // WEBP: RIFF....WEBP
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }

    None
}

/// Fetch raw favicon bytes and detected MIME type from multiple fallback sources.
pub async fn fetch_favicon_raw(
    base_url: &str,
    upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
) -> Option<(&'static str, Vec<u8>)> {
    let host = extract_domain(base_url)?;
    if is_loopback_or_private_host(&host) {
        return None;
    }
    let apex = extract_apex_domain(&host);

    let mut domains = vec![host.clone()];
    if apex != host {
        domains.push(apex);
    }

    async fn try_fetch_raw(
        upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
        url: &str,
    ) -> Option<(&'static str, Vec<u8>)> {
        let bytes = openproxy_adapters::adapters::upstream_get_bytes(upstream_client, url, &[])
            .await
            .ok()?;
        let mime = validate_favicon_bytes(&bytes)?;
        Some((mime, bytes.to_vec()))
    }

    for domain in domains {
        if let Some(res) = try_fetch_raw(
            upstream_client,
            &format!("https://www.google.com/s2/favicons?domain={domain}&sz=64"),
        )
        .await
        {
            return Some(res);
        }

        if let Some(res) = try_fetch_raw(
            upstream_client,
            &format!("https://icons.duckduckgo.com/ip3/{domain}.ico"),
        )
        .await
        {
            return Some(res);
        }

        if let Some(res) =
            try_fetch_raw(upstream_client, &format!("https://{domain}/favicon.ico")).await
        {
            return Some(res);
        }
    }

    None
}

/// Fetch the favicon for a provider from multiple fallback sources
/// as a data URI (`data:image/png;base64,...`).
pub async fn fetch_favicon_data_uri(
    base_url: &str,
    upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
) -> Option<String> {
    let (mime, bytes) = fetch_favicon_raw(base_url, upstream_client).await?;
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some(format!("data:{mime};base64,{b64}"))
}

/// Fetch the favicon for a provider and store binary bytes directly in the database.
pub async fn fetch_and_cache_favicon(
    db_pool: &std::sync::Arc<openproxy_db::conn::DbPool>,
    id: &ProviderId,
    base_url: &str,
    upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
) -> Result<()> {
    if let Some((mime, bytes)) = fetch_favicon_raw(base_url, upstream_client).await {
        let pool = std::sync::Arc::clone(db_pool);
        let id_clone = id.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.writer();
            set_provider_favicon(&conn, id_clone.as_str(), mime, &bytes)
        })
        .await
        .map_err(|e| openproxy_types::error::CoreError::Internal(format!("join error: {e}")))??;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
