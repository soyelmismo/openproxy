//! Shared proxy scheme, endpoint and credential encoding rules.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use std::net::{IpAddr, Ipv6Addr};

const USERINFO: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// The SOCKS connector already sends hostnames to the proxy for remote DNS.
pub fn canonical_scheme(scheme: &str) -> Option<&'static str> {
    match scheme.to_ascii_lowercase().as_str() {
        "http" => Some("http"),
        "https" => Some("https"),
        "socks4" | "socks4a" => Some("socks4"),
        "socks5" | "socks5h" => Some("socks5"),
        _ => None,
    }
}

pub fn canonical_host(host: &str) -> Option<String> {
    let host = host.trim();
    if let Some(inner) = host.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return inner.parse::<Ipv6Addr>().ok().map(|ip| ip.to_string());
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(ip.to_string());
    }
    let name = host.strip_suffix('.').unwrap_or(host);
    if name.len() > 253
        || !name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

pub fn authority(host: &str, port: impl std::fmt::Display) -> String {
    if host.parse::<Ipv6Addr>().is_ok() {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

pub fn encode_userinfo(raw: &str) -> String {
    utf8_percent_encode(raw, USERINFO).to_string()
}

pub fn decode_userinfo(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    if bytes.iter().enumerate().any(|(i, b)| {
        *b == b'%'
            && !bytes
                .get(i + 1..i + 3)
                .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit))
    }) {
        return None;
    }
    let decoded = percent_decode_str(raw).decode_utf8().ok()?;
    (!decoded.chars().any(char::is_control)).then(|| decoded.into_owned())
}

pub fn valid_auth(scheme: &str, user: Option<&str>, pass: Option<&str>) -> bool {
    const MAX_BASIC_CREDENTIAL_BYTES: usize = 4096;

    match (scheme, user, pass) {
        (_, None, None) => true,
        ("socks4", Some(user), None) => {
            (1..=255).contains(&user.len()) && !user.chars().any(char::is_control)
        }
        ("socks5", Some(user), Some(pass)) => {
            (1..=255).contains(&user.len())
                && (1..=255).contains(&pass.len())
                && !user.chars().chain(pass.chars()).any(char::is_control)
        }
        ("http" | "https", Some(user), Some(pass)) => {
            !user.is_empty()
                && user.len().saturating_add(1).saturating_add(pass.len())
                    <= MAX_BASIC_CREDENTIAL_BYTES
                && !user.contains(':')
                && !user.chars().chain(pass.chars()).any(char::is_control)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::valid_auth;

    #[test]
    fn proxy_credentials_are_bounded_in_bytes() {
        let max_socks = format!("{}a", "é".repeat(127));
        let oversized_socks = "é".repeat(128);
        assert!(valid_auth("socks4", Some(&max_socks), None));
        assert!(!valid_auth("socks4", Some(&oversized_socks), None));
        assert!(valid_auth("socks5", Some("u"), Some(&max_socks)));
        assert!(!valid_auth("socks5", Some("u"), Some(&oversized_socks)));

        let max_basic = "é".repeat(2047);
        let oversized_basic = "é".repeat(2048);
        for scheme in ["http", "https"] {
            assert!(valid_auth(scheme, Some("u"), Some(&max_basic)));
            assert!(!valid_auth(scheme, Some("u"), Some(&oversized_basic)));
        }
    }
}
