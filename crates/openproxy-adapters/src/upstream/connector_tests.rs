use super::super::dns::cache_key;
use super::*;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[test]
fn test_is_private_or_reserved() {
    // IPv4 private/reserved
    assert!(is_private_or_reserved(&IpAddr::V4(Ipv4Addr::LOCALHOST))); // loopback
    assert!(is_private_or_reserved(&IpAddr::V4(Ipv4Addr::new(
        10, 0, 0, 1
    )))); // private
    assert!(is_private_or_reserved(&IpAddr::V4(Ipv4Addr::new(
        192, 168, 1, 1
    )))); // private
    assert!(is_private_or_reserved(&IpAddr::V4(Ipv4Addr::new(
        169, 254, 0, 1
    )))); // link-local
    assert!(is_private_or_reserved(&IpAddr::V4(Ipv4Addr::UNSPECIFIED))); // zero

    // IPv4 public
    assert!(!is_private_or_reserved(&IpAddr::V4(Ipv4Addr::new(
        8, 8, 8, 8
    ))));

    // IPv6 private/reserved
    assert!(is_private_or_reserved(&IpAddr::V6(Ipv6Addr::LOCALHOST))); // loopback

    // Documentation networks
    assert!(is_private_or_reserved(&IpAddr::V4(Ipv4Addr::new(
        192, 0, 2, 1
    ))));
    assert!(is_private_or_reserved(&IpAddr::V4(Ipv4Addr::new(
        198, 51, 100, 1
    ))));
    assert!(is_private_or_reserved(&IpAddr::V4(Ipv4Addr::new(
        203, 0, 113, 1
    ))));

    // Unique Local IPv6 (fc00::/7)
    assert!(is_private_or_reserved(&IpAddr::V6(Ipv6Addr::new(
        0xfc00, 0, 0, 0, 0, 0, 0, 1
    ))));
    assert!(is_private_or_reserved(&IpAddr::V6(Ipv6Addr::new(
        0xfd00, 0, 0, 0, 0, 0, 0, 1
    ))));

    // IPv4-mapped IPv6
    assert!(is_private_or_reserved(&IpAddr::V6(Ipv6Addr::new(
        0, 0, 0, 0, 0, 0xffff, 0x7f00, 0x0001
    ))));
    assert!(!is_private_or_reserved(&IpAddr::V6(Ipv6Addr::new(
        0, 0, 0, 0, 0, 0xffff, 0x0808, 0x0808
    ))));

    // IPv6 public
    assert!(!is_private_or_reserved(&IpAddr::V6(Ipv6Addr::new(
        0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888
    ))));
}

#[test]
fn test_parse_authority() {
    let uri: http::Uri = "https://api.openai.com/v1/chat/completions"
        .parse()
        .unwrap();
    assert_eq!(parse_authority(&uri).unwrap(), ("api.openai.com", 443));

    let uri_http: http::Uri = "http://example.com/foo".parse().unwrap();
    assert_eq!(parse_authority(&uri_http).unwrap(), ("example.com", 80));

    let uri_port: http::Uri = "http://example.com:8080/foo".parse().unwrap();
    assert_eq!(parse_authority(&uri_port).unwrap(), ("example.com", 8080));

    let uri_ipv6: http::Uri = "http://[::1]:9000/foo".parse().unwrap();
    assert_eq!(parse_authority(&uri_ipv6).unwrap(), ("::1", 9000));
}

#[test]
fn test_cache_key() {
    let k1 = cache_key("api.openai.com", 443);
    let k2 = cache_key("api.openai.com", 443);
    let k3 = cache_key("api.openai.com", 80);
    let k4 = cache_key("api.anthropic.com", 443);
    assert_eq!(k1, k2);
    assert_ne!(k1, k3);
    assert_ne!(k1, k4);
}
