//! Real TCP tests select the cooperative transport directly, never via global env.

use super::*;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::upstream::{
    CancellationToken, ResolvedTimeouts, TimeoutProfile, UpstreamClient, UpstreamRequest,
};

fn profile() -> TimeoutProfile {
    TimeoutProfile::Custom(ResolvedTimeouts {
        dns_ms: 1000,
        dial_ms: 1000,
        tls_ms: 1000,
        write_ms: 100,
        headers_ms: 200,
        body_chunk_ms: 100,
        total_ms: 2000,
    })
}

async fn read_request(tcp: &mut TcpStream) -> Vec<u8> {
    let mut raw = Vec::new();
    let boundary = loop {
        let mut buf = [0; 8192];
        let n = tcp.read(&mut buf).await.unwrap();
        assert!(n > 0);
        raw.extend_from_slice(&buf[..n]);
        if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let length = String::from_utf8_lossy(&raw[..boundary])
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|value| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while raw.len() < boundary + length {
        let mut buf = [0; 8192];
        let n = tcp.read(&mut buf).await.unwrap();
        assert!(n > 0);
        raw.extend_from_slice(&buf[..n]);
    }
    raw
}

#[tokio::test]
async fn delayed_chunked_body_drives_connection_without_a_background_task() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        let request = read_request(&mut tcp).await;
        let text = String::from_utf8_lossy(&request);
        assert!(text.contains("GET /test?q=1 HTTP/1.1"));
        assert!(text.contains(&format!("host: {addr}")));
        tcp.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
            .await
            .unwrap();
        for chunk in [b"hello".as_slice(), b"world"] {
            tokio::time::sleep(Duration::from_millis(20)).await;
            tcp.write_all(b"5\r\n").await.unwrap();
            tcp.write_all(chunk).await.unwrap();
            tcp.write_all(b"\r\n").await.unwrap();
        }
        tcp.write_all(b"0\r\n\r\n").await.unwrap();
    });
    let response = UpstreamClient::for_test_cooperative()
        .call(
            UpstreamRequest::get(format!("http://{addr}/test?q=1")),
            profile(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.collect().await.unwrap(),
        Bytes::from_static(b"helloworld")
    );
    server.await.unwrap();
}

#[tokio::test]
async fn drop_closes_connection_instead_of_leaving_a_driver_task() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        read_request(&mut tcp).await;
        tcp.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nchunk\r\n")
            .await
            .unwrap();
        let mut buf = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), tcp.read(&mut buf))
                .await
                .expect("drop must close socket")
                .unwrap(),
            0
        );
    });
    let mut response = UpstreamClient::for_test_cooperative()
        .call(
            UpstreamRequest::get(format!("http://{addr}/")),
            profile(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(response.body.next_chunk().await.unwrap().is_some());
    drop(response);
    server.await.unwrap();
}

#[tokio::test]
async fn idle_chunk_timeout_and_body_cancel_preserve_phase() {
    for cancel_body in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            read_request(&mut tcp).await;
            tcp.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\npart1\r\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let cancel = CancellationToken::new();
        let mut response = UpstreamClient::for_test_cooperative()
            .call(
                UpstreamRequest::get(format!("http://{addr}/")),
                profile(),
                cancel.clone(),
            )
            .await
            .unwrap();
        assert!(response.body.next_chunk().await.unwrap().is_some());
        response.body.note_content_chunk();
        if cancel_body {
            cancel.cancel();
        }
        let error = response.body.next_chunk().await.unwrap_err();
        if cancel_body {
            assert!(matches!(error, UpstreamError::Cancel), "{error:?}");
        } else {
            assert!(
                matches!(error, UpstreamError::Timeout(UpstreamPhase::Body)),
                "{error:?}"
            );
        }
        drop(response);
        server.abort();
    }
}

#[tokio::test]
async fn blocked_upload_remains_write_timeout_in_both_modes() {
    for streaming in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_tcp, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(3)).await;
        });
        let mut req = UpstreamRequest::post_json(
            format!("http://{addr}/"),
            Bytes::from(vec![b'x'; 32 * 1024 * 1024]),
        );
        req.is_streaming = streaming;
        let error = UpstreamClient::for_test_cooperative()
            .call(req, profile(), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            matches!(error, UpstreamError::Timeout(UpstreamPhase::Write)),
            "{error:?}"
        );
        server.abort();
    }
}

#[tokio::test]
async fn flushed_upload_arms_headers_but_unary_uses_total() {
    for streaming in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            read_request(&mut tcp).await;
            tokio::time::sleep(Duration::from_secs(3)).await;
        });
        let mut req =
            UpstreamRequest::post_json(format!("http://{addr}/"), Bytes::from_static(b"{}"));
        req.is_streaming = streaming;
        let mut timeouts = profile().resolve();
        timeouts.total_ms = 500;
        let error = UpstreamClient::for_test_cooperative()
            .call(
                req,
                TimeoutProfile::Custom(timeouts),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let expected = if streaming {
            UpstreamPhase::Headers
        } else {
            UpstreamPhase::Total
        };
        assert!(
            matches!(error, UpstreamError::Timeout(phase) if phase == expected),
            "{error:?}"
        );
        server.abort();
    }
}

#[tokio::test]
async fn early_response_during_upload_survives() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        let mut buf = [0; 4096];
        assert!(tcp.read(&mut buf).await.unwrap() > 0);
        tcp.write_all(
            b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    });
    let response = UpstreamClient::for_test_cooperative()
        .call(
            UpstreamRequest::post_json(
                format!("http://{addr}/"),
                Bytes::from(vec![b'x'; 32 * 1024 * 1024]),
            ),
            profile(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.status, http::StatusCode::PAYLOAD_TOO_LARGE);
    assert!(response.collect().await.unwrap().is_empty());
    server.await.unwrap();
}

#[tokio::test]
async fn gzip_decoding_and_nonstreaming_bounds_are_preserved() {
    let payload = b"compressed cooperative response";
    let gzip = crate::upstream::compression_tests::gzip_container(payload, 0);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        read_request(&mut tcp).await;
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
            gzip.len()
        );
        tcp.write_all(head.as_bytes()).await.unwrap();
        tcp.write_all(&gzip).await.unwrap();
    });
    let response = UpstreamClient::for_test_cooperative()
        .call(
            UpstreamRequest::get(format!("http://{addr}/")),
            profile(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        !response
            .headers
            .contains_key(http::header::CONTENT_ENCODING)
    );
    assert_eq!(response.collect().await.unwrap().as_ref(), payload);
    server.await.unwrap();

    // The limit wraps TransportBody too, not only legacy Incoming.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        read_request(&mut tcp).await;
        tcp.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 34603008\r\n\r\n")
            .await
            .unwrap();
        let block = vec![b'x'; 1024 * 1024];
        for _ in 0..33 {
            if tcp.write_all(&block).await.is_err() {
                break;
            }
        }
    });
    let mut req = UpstreamRequest::get(format!("http://{addr}/"));
    req.is_streaming = false;
    let response = UpstreamClient::for_test_cooperative()
        .call(req, TimeoutProfile::Chat, CancellationToken::new())
        .await
        .unwrap();
    let error = response.collect().await.unwrap_err();
    assert!(error.to_string().contains("length limit"), "{error}");
    server.await.unwrap();
}

#[test]
fn tls_proxy_upgrade_and_connect_use_legacy() {
    let transport = CooperativeH1Transport::new();
    let build = |url| super::super::client::build_hyper_request(UpstreamRequest::get(url)).unwrap();
    assert!(transport.uses_cooperative_connection(&build("http://example.com/"), None));
    let mut http10 = build("http://example.com/");
    *http10.version_mut() = http::Version::HTTP_10;
    assert!(transport.uses_cooperative_connection(&http10, None));
    assert!(!transport.uses_cooperative_connection(&build("https://example.com/"), None));
    assert!(
        !transport
            .uses_cooperative_connection(&build("http://example.com/"), Some("http://proxy/"))
    );
    let mut req = build("http://example.com/");
    req.headers_mut().insert(
        http::header::UPGRADE,
        http::HeaderValue::from_static("websocket"),
    );
    assert!(!transport.uses_cooperative_connection(&req, None));
    req.headers_mut().clear();
    *req.method_mut() = http::Method::CONNECT;
    assert!(!transport.uses_cooperative_connection(&req, None));
}

async fn respond(tcp: &mut TcpStream, body: &str) {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    tcp.write_all(head.as_bytes()).await.unwrap();
}

/// The listener accepts exactly one connection. A second dial would never be
/// answered and the request would fail on its headers deadline.
#[tokio::test]
async fn sequential_requests_reuse_one_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        for body in ["first", "second", "third"] {
            read_request(&mut tcp).await;
            respond(&mut tcp, body).await;
        }
    });
    let client = UpstreamClient::for_test_cooperative();
    for body in ["first", "second", "third"] {
        let response = client
            .call(
                UpstreamRequest::get(format!("http://{addr}/")),
                profile(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(response.collect().await.unwrap().as_ref(), body.as_bytes());
    }
    server.await.unwrap();
}

/// A client that stops reading after the content it needs (SSE `[DONE]`)
/// still recovers the connection when the terminator already arrived.
#[tokio::test]
async fn drop_after_terminator_arrived_keeps_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        read_request(&mut tcp).await;
        tcp.write_all(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n5\r\nworld\r\n0\r\n\r\n",
        )
        .await
        .unwrap();
        read_request(&mut tcp).await;
        respond(&mut tcp, "again").await;
    });
    let client = UpstreamClient::for_test_cooperative();
    let mut response = client
        .call(
            UpstreamRequest::get(format!("http://{addr}/")),
            profile(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(response.body.next_chunk().await.unwrap().is_some());
    tokio::time::sleep(Duration::from_millis(20)).await;
    drop(response);
    let response = client
        .call(
            UpstreamRequest::get(format!("http://{addr}/")),
            profile(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.collect().await.unwrap().as_ref(), b"again");
    server.await.unwrap();
}

/// An idle connection the peer closed is never used for the next request.
#[tokio::test]
async fn peer_closed_idle_connection_is_replaced() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for body in ["first", "second"] {
            let (mut tcp, _) = listener.accept().await.unwrap();
            read_request(&mut tcp).await;
            respond(&mut tcp, body).await;
            tcp.shutdown().await.unwrap();
        }
    });
    let client = UpstreamClient::for_test_cooperative();
    for body in ["first", "second"] {
        let response = client
            .call(
                UpstreamRequest::post_json(format!("http://{addr}/"), Bytes::from_static(b"{}")),
                profile(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(response.collect().await.unwrap().as_ref(), body.as_bytes());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    server.await.unwrap();
}
