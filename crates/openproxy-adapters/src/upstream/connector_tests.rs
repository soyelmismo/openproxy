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
    assert!(is_private_or_reserved(&IpAddr::V6(Ipv6Addr::UNSPECIFIED))); // unspecified (::)
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

#[tokio::test]
async fn test_dial_phase_fallback_after_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();

    let accept_task = tokio::spawn(async move {
        let _ = listener.accept().await;
    });

    // 192.0.2.1 is TEST-NET-1 (RFC 5737), which blackholes/times out on dial.
    let blackhole_addr: SocketAddr = "192.0.2.1:80".parse().unwrap();
    let addrs = vec![blackhole_addr, local_addr];

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let timeout_config = Duration::from_millis(150);

    let result = dial_phase(addrs, deadline, timeout_config).await;
    assert!(
        result.is_ok(),
        "dial_phase should fall through to second address after timeout on first: {:?}",
        result.err()
    );

    drop(accept_task.await);
}

#[tokio::test]
async fn test_dial_phase_fallback_after_connect_error() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();

    let accept_task = tokio::spawn(async move {
        let _ = listener.accept().await;
    });

    // Pick a port that is guaranteed closed on localhost
    let closed_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_addr = closed_listener.local_addr().unwrap();
    drop(closed_listener);

    let addrs = vec![closed_addr, local_addr];
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let timeout_config = Duration::from_millis(500);

    let result = dial_phase(addrs, deadline, timeout_config).await;
    assert!(
        result.is_ok(),
        "dial_phase should fall through to second address after connect error on first: {:?}",
        result.err()
    );

    drop(accept_task.await);
}

#[tokio::test]
async fn test_dial_phase_all_candidates_timeout() {
    let blackhole_1: SocketAddr = "192.0.2.1:80".parse().unwrap();
    let blackhole_2: SocketAddr = "192.0.2.2:80".parse().unwrap();
    let addrs = vec![blackhole_1, blackhole_2];

    let deadline = std::time::Instant::now() + Duration::from_millis(250);
    let timeout_config = Duration::from_millis(100);

    let result = dial_phase(addrs, deadline, timeout_config).await;
    assert!(result.is_err());
    let err = result.err().unwrap();
    assert_eq!(err.phase, UpstreamPhase::Dial);
    assert!(matches!(err.kind, PhasedErrorKind::Timeout));
}

#[tokio::test]
async fn test_dial_phase_empty_addresses() {
    let addrs = vec![];
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    let timeout_config = Duration::from_millis(100);

    let result = dial_phase(addrs, deadline, timeout_config).await;
    assert!(result.is_err());
    let err = result.err().unwrap();
    assert_eq!(err.phase, UpstreamPhase::Dial);
    match err.kind {
        PhasedErrorKind::Io(io_err) => {
            assert_eq!(io_err.to_string(), "no addresses to dial");
        }
        _ => panic!("expected Io error for empty addresses"),
    }
}

#[cfg(feature = "upstream-hyper")]
#[test]
fn test_production_transport_proxy_pool_segregation() {
    use crate::upstream::client::ProductionTransport;

    let transport = ProductionTransport::new();
    let p1 = "http://proxy1.example.com:8080";
    let p2 = "http://proxy2.example.com:8080";

    assert_eq!(transport.proxy_clients.len(), 0);

    let _c1 = transport.client_for_proxy(Some(p1));
    assert_eq!(transport.proxy_clients.len(), 1);
    assert!(transport.proxy_clients.contains_key(p1));

    let _c2 = transport.client_for_proxy(Some(p2));
    assert_eq!(transport.proxy_clients.len(), 2);
    assert!(transport.proxy_clients.contains_key(p2));

    let _cdirect = transport.client_for_proxy(None);
    assert_eq!(transport.proxy_clients.len(), 2);

    let _c1_again = transport.client_for_proxy(Some(p1));
    assert_eq!(transport.proxy_clients.len(), 2);
}

#[cfg(feature = "upstream-hyper")]
#[test]
fn test_probe_request_preserves_connection_close_header() {
    use crate::upstream::UpstreamRequest;
    use crate::upstream::client::build_hyper_request;

    let mut req = UpstreamRequest::get("http://example.com/generate_204");
    req.headers.insert(
        http::header::CONNECTION,
        http::HeaderValue::from_static("close"),
    );

    let hyper_req = build_hyper_request(req).unwrap();
    assert_eq!(
        hyper_req.headers().get(http::header::CONNECTION).unwrap(),
        "close"
    );
}

#[cfg(feature = "upstream-hyper")]
#[tokio::test]
async fn test_probe_socket_closed_after_response_with_connection_close() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();

    let server_task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let n = socket.read(&mut buf).await.unwrap();
        let req_str = String::from_utf8_lossy(&buf[..n]);
        assert!(
            req_str.to_ascii_lowercase().contains("connection: close"),
            "probe request must contain Connection: close header"
        );

        socket
            .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        socket.flush().await.unwrap();

        let mut eof_buf = [0u8; 16];
        let eof_n = socket.read(&mut eof_buf).await.unwrap();
        assert_eq!(
            eof_n, 0,
            "client socket should be closed after Connection: close response"
        );
    });

    let client = crate::upstream::UpstreamClient::new();
    let mut req =
        crate::upstream::UpstreamRequest::get(format!("http://{local_addr}/generate_204"));
    req.headers.insert(
        http::header::CONNECTION,
        http::HeaderValue::from_static("close"),
    );

    let profile = crate::upstream::TimeoutProfile::Chat;
    let cancel = crate::upstream::CancellationToken::new();
    let res = client.call(req, profile, cancel).await;
    assert!(res.is_ok(), "probe request should succeed: {:?}", res.err());

    server_task.await.unwrap();
}

#[cfg(feature = "upstream-hyper")]
#[tokio::test]
async fn test_adversarial_concurrent_proxy_pool_segregation() {
    use crate::upstream::client::ProductionTransport;
    use crate::upstream::conn_pool::{HostKey, Scheme, UpstreamConnectionPool};
    use std::sync::Arc;
    use tokio::task::JoinSet;

    let transport = Arc::new(ProductionTransport::new());
    let pool = Arc::new(UpstreamConnectionPool::new());
    let mut tasks = JoinSet::new();

    // 1. Concurrently request 10 distinct proxy URLs and direct requests (100 concurrent tasks)
    for i in 0..100 {
        let t = Arc::clone(&transport);
        let p = Arc::clone(&pool);
        tasks.spawn(async move {
            let proxy = if i % 10 == 0 {
                None
            } else {
                Some(format!("http://proxy{}.internal.local:8080", i % 10))
            };

            let _client = t.client_for_proxy(proxy.as_deref());

            let host_key = HostKey::with_proxy(Scheme::Https, "api.openai.com", 443, proxy);
            p.record_dial(host_key.clone());
            host_key
        });
    }

    let mut keys = Vec::new();
    while let Some(res) = tasks.join_next().await {
        keys.push(res.unwrap());
    }
    assert_eq!(keys.len(), 100);

    // Direct requests do not occupy proxy_clients map; the 9 distinct proxy URLs do
    assert_eq!(transport.proxy_clients.len(), 9);
    for i in 1..10 {
        let proxy_url = format!("http://proxy{i}.internal.local:8080");
        assert!(
            transport.proxy_clients.contains_key(&proxy_url),
            "Proxy {proxy_url} must be present in proxy_clients map"
        );
    }

    // 2. High concurrency churn across 150 unique proxies to test eviction limit (128)
    for i in 100..250 {
        let t = Arc::clone(&transport);
        tasks.spawn(async move {
            let proxy_url = format!("http://proxy-churn-{i}.internal.local:8080");
            let _client = t.client_for_proxy(Some(&proxy_url));
            HostKey::with_proxy(Scheme::Https, "api.openai.com", 443, Some(proxy_url))
        });
    }

    while let Some(res) = tasks.join_next().await {
        res.unwrap();
    }

    // After churn past 128 entries, cache cleared and stayed <= 128 without panic or deadlock
    assert!(
        transport.proxy_clients.len() <= 128,
        "proxy_clients length {} must be <= 128",
        transport.proxy_clients.len()
    );
}

#[cfg(feature = "upstream-hyper")]
#[tokio::test]
async fn test_adversarial_probe_socket_never_leaves_warm_connection() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();

    let client_ports = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let client_ports_server = std::sync::Arc::clone(&client_ports);

    let server_task = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, peer_addr) = listener.accept().await.unwrap();
            client_ports_server.lock().await.push(peer_addr.port());

            let mut buf = [0u8; 1024];
            let n = socket.read(&mut buf).await.unwrap();
            let req_str = String::from_utf8_lossy(&buf[..n]);
            assert!(
                req_str.to_ascii_lowercase().contains("connection: close"),
                "Probe request must contain 'connection: close'"
            );

            socket
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            socket.flush().await.unwrap();

            // Verify client immediately closes connection (EOF)
            let mut eof_buf = [0u8; 16];
            let eof_n = socket.read(&mut eof_buf).await.unwrap();
            assert_eq!(
                eof_n, 0,
                "Client must close socket after Connection: close response"
            );
        }
    });

    let client = crate::upstream::UpstreamClient::new();

    // First probe request
    let mut req1 =
        crate::upstream::UpstreamRequest::get(format!("http://{local_addr}/generate_204"));
    req1.headers.insert(
        http::header::CONNECTION,
        http::HeaderValue::from_static("close"),
    );
    let res1 = client
        .call(
            req1,
            crate::upstream::TimeoutProfile::Chat,
            crate::upstream::CancellationToken::new(),
        )
        .await;
    assert!(res1.is_ok(), "First probe request must succeed");

    // Second probe request to the exact same host and path
    let mut req2 =
        crate::upstream::UpstreamRequest::get(format!("http://{local_addr}/generate_204"));
    req2.headers.insert(
        http::header::CONNECTION,
        http::HeaderValue::from_static("close"),
    );
    let res2 = client
        .call(
            req2,
            crate::upstream::TimeoutProfile::Chat,
            crate::upstream::CancellationToken::new(),
        )
        .await;
    assert!(res2.is_ok(), "Second probe request must succeed");

    server_task.await.unwrap();

    let ports = client_ports.lock().await.clone();
    assert_eq!(ports.len(), 2);
    assert_ne!(
        ports[0], ports[1],
        "Probe connection MUST NOT be reused! Ports were identical: {}",
        ports[0]
    );
}

#[tokio::test]
async fn test_adversarial_dial_phase_ipv6_to_ipv4_transparent_fallback() {
    let listener_v4 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_v4_addr = listener_v4.local_addr().unwrap();

    let accept_task = tokio::spawn(async move {
        let _ = listener_v4.accept().await;
    });

    // Unreachable IPv6 documentation address (RFC 3849 2001:db8::1)
    let unreachable_v6: SocketAddr = "[2001:db8::1]:80".parse().unwrap();
    let addrs = vec![unreachable_v6, local_v4_addr];

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let timeout_config = Duration::from_millis(150);

    let result = dial_phase(addrs, deadline, timeout_config).await;
    assert!(
        result.is_ok(),
        "dial_phase must transparently fall back from unreachable IPv6 to reachable IPv4: {:?}",
        result.err()
    );

    drop(accept_task.await);
}
