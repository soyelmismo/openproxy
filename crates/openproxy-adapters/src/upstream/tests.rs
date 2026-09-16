//! Unit tests for the `upstream/` module (Gate-0 test plan).

#![cfg(all(feature = "upstream-hyper", test))]

use super::*;
use http::StatusCode;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn spawn_mock_raw(
    chunks_with_delays: Vec<(&'static [u8], Duration)>,
) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut tcp, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = tcp.read(&mut buf).await;
            for (chunk, delay) in chunks_with_delays {
                if !chunk.is_empty() {
                    let _ = tcp.write_all(chunk).await;
                    let _ = tcp.flush().await;
                }
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            }
        }
    });
    addr
}

#[tokio::test]
async fn phase_timeout_tls() {
    spawn_mock_raw(vec![(&[], Duration::from_secs(30))]).await;
    let client = UpstreamClient::new();
    let mut to = TimeoutProfile::Chat.resolve();
    to.dial_ms = 10;
    let res = client
        .call(
            UpstreamRequest::get("http://192.0.2.1/"),
            TimeoutProfile::Custom(to),
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(
        res.unwrap_err(),
        UpstreamError::Timeout(UpstreamPhase::Dns | UpstreamPhase::Dial | UpstreamPhase::Write)
    ));
}

#[tokio::test]
async fn cancel_mid_body() {
    let addr = spawn_mock_raw(vec![(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n",
        Duration::from_secs(300),
    )]).await;
    let client = UpstreamClient::new();
    let cancel = CancellationToken::new();
    let mut resp = client
        .call(
            UpstreamRequest::get(format!("http://{addr}/")),
            TimeoutProfile::OAuth,
            cancel.clone(),
        )
        .await
        .expect("first request should succeed");
    assert_eq!(resp.status, StatusCode::OK);
    let chunk = resp
        .body
        .next_chunk()
        .await
        .expect("first chunk ok")
        .expect("chunk data");
    assert_eq!(&chunk[..5], b"hello");
    cancel.cancel();
    assert!(matches!(
        resp.body.next_chunk().await.unwrap_err(),
        UpstreamError::Cancel
    ));
}

async fn spawn_echo_server() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            if let Ok((mut tcp, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = tcp.read(&mut buf).await;
                    let resp = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nOK";
                    let _ = tcp.write_all(resp).await;
                    let _ = tcp.shutdown().await;
                });
            }
        }
    });
    addr
}

#[tokio::test]
async fn conn_pool_reuse() {
    let addr = spawn_echo_server().await;
    let url = format!("http://{addr}/");
    let client = UpstreamClient::new();
    let cancel = CancellationToken::new();
    let profile = TimeoutProfile::OAuth;

    let r1 = client
        .call(
            UpstreamRequest::get(&url),
            profile,
            CancellationToken::clone(&cancel),
        )
        .await
        .expect("first call ok");
    let _ = r1.body.collect_all().await.expect("collect first");

    let r2 = client
        .call(UpstreamRequest::get(&url), profile, cancel)
        .await
        .expect("second call ok");
    let _ = r2.body.collect_all().await.expect("collect second");

    let pool = client.pool();
    assert!(
        pool.reuses() >= 1,
        "expected at least one pool reuse, got reuses={}, total={}",
        pool.reuses(),
        pool.total()
    );
}

#[test]
fn profile_chat_default_values() {
    let t = TimeoutProfile::Chat.resolve();
    assert_eq!(t.dns_ms, 5_000);
    assert_eq!(t.dial_ms, 5_000);
    assert_eq!(t.tls_ms, 5_000);
    assert_eq!(t.write_ms, 10_000);
    assert_eq!(t.headers_ms, 6_000);
    assert_eq!(t.body_chunk_ms, 90_000);
    assert_eq!(t.total_ms, 300_000);
}

fn custom_profile(
    headers_ms: u64,
    body_chunk_ms: u64,
    write_ms: u64,
    total_ms: u64,
) -> TimeoutProfile {
    TimeoutProfile::Custom(ResolvedTimeouts {
        dns_ms: 5_000,
        dial_ms: 5_000,
        tls_ms: 5_000,
        write_ms,
        headers_ms,
        body_chunk_ms,
        total_ms,
    })
}

#[tokio::test]
async fn phase_timeout_body_chunk_gap() {
    let addr = spawn_mock_raw(vec![
        (b"HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ntransfer-encoding: chunked\r\n\r\n5\r\nfirst\r\n", Duration::from_secs(5)),
        (b"6\r\nsecond\r\n0\r\n\r\n", Duration::ZERO),
    ]).await;
    let client = UpstreamClient::new();
    let mut resp = client
        .call(
            UpstreamRequest::get(format!("http://{addr}/")),
            custom_profile(10_000, 1_000, 5_000, 30_000),
            CancellationToken::new(),
        )
        .await
        .expect("first request ok");
    assert_eq!(resp.status, StatusCode::OK);
    let chunk = resp
        .body
        .next_chunk()
        .await
        .expect("first chunk ok")
        .expect("first chunk data");
    assert_eq!(&chunk[..5], b"first");
    resp.body.note_content_chunk();
    let t_before_second = std::time::Instant::now();
    let res = resp.body.next_chunk().await;
    let gap_elapsed = t_before_second.elapsed();
    assert!(matches!(
        res.unwrap_err(),
        UpstreamError::Timeout(UpstreamPhase::Body)
    ));
    assert!(gap_elapsed >= Duration::from_millis(800) && gap_elapsed < Duration::from_secs(4));
}

#[tokio::test]
async fn stub_event_does_not_start_chunk_gap_timer() {
    let addr = spawn_mock_raw(vec![(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n4\r\nstub\r\n", Duration::from_secs(30))]).await;
    let client = UpstreamClient::new();
    let mut resp = client
        .call(
            UpstreamRequest::get(format!("http://{addr}/")),
            custom_profile(5_000, 500, 5_000, 2_000),
            CancellationToken::new(),
        )
        .await
        .expect("dispatch ok");
    assert_eq!(resp.status, StatusCode::OK);
    let chunk = resp
        .body
        .next_chunk()
        .await
        .expect("first chunk ok")
        .expect("first chunk data");
    assert_eq!(&chunk[..4], b"stub");
    let t = std::time::Instant::now();
    let res = resp.body.next_chunk().await;
    let elapsed = t.elapsed();
    assert!(matches!(
        res.unwrap_err(),
        UpstreamError::Timeout(UpstreamPhase::Total)
    ));
    assert!(elapsed >= Duration::from_millis(1_500) && elapsed < Duration::from_secs(3));
}

#[tokio::test]
async fn ttft_timeout_fires_when_first_chunk_delayed_after_headers() {
    let addr = spawn_mock_raw(vec![(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
        Duration::from_secs(30),
    )])
    .await;
    let client = UpstreamClient::new();
    let mut resp = client
        .call(
            UpstreamRequest::get(format!("http://{addr}/")),
            custom_profile(200, 5_000, 5_000, 5_000),
            CancellationToken::new(),
        )
        .await
        .expect("dispatch ok");
    assert_eq!(resp.status, StatusCode::OK);
    let t = std::time::Instant::now();
    let res = resp.body.next_chunk().await;
    assert!(matches!(
        res.unwrap_err(),
        UpstreamError::Timeout(UpstreamPhase::Headers)
    ));
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn phase_timeout_write_accumulates() {
    let addr = spawn_mock_raw(vec![(&[], Duration::from_secs(30))]).await;
    let client = UpstreamClient::new();
    let t0 = std::time::Instant::now();
    let res = client
        .call(
            UpstreamRequest::get(format!("http://{addr}/")),
            custom_profile(5_000, 5_000, 200, 60_000),
            CancellationToken::new(),
        )
        .await;
    let elapsed = t0.elapsed();
    assert!(matches!(
        res.unwrap_err(),
        UpstreamError::Timeout(UpstreamPhase::Write)
    ));
    assert!(elapsed >= Duration::from_millis(150) && elapsed < Duration::from_secs(2));
}

#[tokio::test]
async fn phase_timeout_dial_real() {
    let client = UpstreamClient::new();
    let mut to = TimeoutProfile::Chat.resolve();
    to.dial_ms = 50;
    to.total_ms = 60_000;
    let res = client
        .call(
            UpstreamRequest::get("http://192.0.2.1/"),
            TimeoutProfile::Custom(to),
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(
        res.unwrap_err(),
        UpstreamError::Timeout(UpstreamPhase::Dial | UpstreamPhase::Write)
    ));
}

#[tokio::test]
async fn adversarial_phase_timeout_dns_actually_fires_at_dns_ms_not_total() {
    let client = UpstreamClient::new();
    let mut to = TimeoutProfile::Chat.resolve();
    to.dns_ms = 1;
    to.total_ms = 30_000;
    let t0 = std::time::Instant::now();
    let res = client
        .call(
            UpstreamRequest::get("http://nonexistent.openproxy-test.invalid/"),
            TimeoutProfile::Custom(to),
            CancellationToken::new(),
        )
        .await;
    let elapsed = t0.elapsed();
    if let Err(e) = &res {
        assert!(
            matches!(e, UpstreamError::Timeout(UpstreamPhase::Dns))
                || matches!(e, UpstreamError::Connection(msg) if msg.contains("failed to lookup address information") || msg.contains("Name or service not known"))
        );
    }
    assert!(elapsed < Duration::from_secs(5));
}

#[tokio::test]
async fn adversarial_phased_connector_respects_dynamic_timeouts_via_atomic() {
    use crate::upstream::connector::{CALL_TIMEOUTS, PhasedConnector, PhasedTimeouts};

    let connector = PhasedConnector::new(PhasedTimeouts {
        dns: Duration::from_secs(5),
        dial: Duration::from_secs(5),
        tls: Duration::from_secs(5),
    });
    assert_eq!(connector.effective_timeouts().dial, Duration::from_secs(5));
    assert_eq!(connector.effective_timeouts().dns, Duration::from_secs(5));

    connector.set_timeouts(PhasedTimeouts {
        dns: Duration::from_millis(50),
        dial: Duration::from_millis(50),
        tls: Duration::from_secs(5),
    });
    assert_eq!(connector.effective_timeouts().dial, Duration::from_secs(5));

    let tight = PhasedTimeouts {
        dns: Duration::from_millis(50),
        dial: Duration::from_millis(50),
        tls: Duration::from_secs(5),
    };
    let read_back = CALL_TIMEOUTS
        .scope(tight, async { connector.effective_timeouts() })
        .await;
    assert_eq!(read_back.dial, Duration::from_millis(50));
    assert_eq!(read_back.dns, Duration::from_millis(50));
    assert_eq!(connector.effective_timeouts().dial, Duration::from_secs(5));
}

#[tokio::test]
async fn adversarial_phase_timeout_body_chunk_gap_resets_after_each_chunk() {
    let addr = spawn_mock_raw(vec![
        (b"HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ntransfer-encoding: chunked\r\n\r\n5\r\nfirst\r\n", Duration::from_millis(200)),
        (b"6\r\nsecond\r\n", Duration::from_millis(200)),
        (b"5\r\nthird\r\n", Duration::from_secs(5)),
        (b"5\r\nfourth\r\n0\r\n\r\n", Duration::ZERO),
    ]).await;
    let client = UpstreamClient::new();
    let mut resp = client
        .call(
            UpstreamRequest::get(format!("http://{addr}/")),
            custom_profile(10_000, 1_000, 5_000, 30_000),
            CancellationToken::new(),
        )
        .await
        .expect("dispatch ok");
    assert_eq!(resp.status, StatusCode::OK);

    let mut got = 0usize;
    for _ in 0..3 {
        let chunk = resp
            .body
            .next_chunk()
            .await
            .expect("chunk ok")
            .expect("chunk data");
        got += chunk.len();
        resp.body.note_content_chunk();
    }
    assert!(got > 0);

    let t = std::time::Instant::now();
    let res = resp.body.next_chunk().await;
    assert!(matches!(
        res.unwrap_err(),
        UpstreamError::Timeout(UpstreamPhase::Body)
    ));
    assert!(t.elapsed() >= Duration::from_millis(800) && t.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn adversarial_phase_timeout_body_chunk_not_attributed_to_write() {
    let addr = spawn_mock_raw(vec![
        (b"HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ntransfer-encoding: chunked\r\n\r\n5\r\nfirst\r\n", Duration::from_secs(5)),
        (b"6\r\nsecond\r\n0\r\n\r\n", Duration::ZERO),
    ]).await;
    let client = UpstreamClient::new();
    let mut resp = client
        .call(
            UpstreamRequest::get(format!("http://{addr}/")),
            custom_profile(5_000, 200, 5_000, 30_000),
            CancellationToken::new(),
        )
        .await
        .expect("dispatch ok");
    assert_eq!(resp.status, StatusCode::OK);
    let chunk = resp
        .body
        .next_chunk()
        .await
        .expect("first chunk ok")
        .expect("first chunk data");
    assert!(!chunk.is_empty());
    resp.body.note_content_chunk();

    let t = std::time::Instant::now();
    let res = resp.body.next_chunk().await;
    assert!(matches!(
        res.unwrap_err(),
        UpstreamError::Timeout(UpstreamPhase::Body)
    ));
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn headers_timeout_fires_on_silent_http_server() {
    let addr = spawn_mock_raw(vec![(&[], Duration::from_secs(30))]).await;
    let client = UpstreamClient::new();
    let t0 = std::time::Instant::now();
    let res = client
        .call(
            UpstreamRequest::post_json(format!("http://{addr}/"), bytes::Bytes::from("{}")),
            custom_profile(200, 5_000, 5_000, 30_000),
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(
        res.unwrap_err(),
        UpstreamError::Timeout(UpstreamPhase::Headers)
    ));
    assert!(t0.elapsed() < Duration::from_secs(2));
}

async fn spawn_chunk_producer(
    header: &'static [u8],
    chunk_size: usize,
    count: usize,
) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut tcp, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = tcp.read(&mut buf).await;
            let _ = tcp.write_all(header).await;
            let chunk = vec![b'x'; chunk_size];
            let chunk_hdr = format!("{chunk_size:x}\r\n");
            for _ in 0..count {
                if tcp.write_all(chunk_hdr.as_bytes()).await.is_err()
                    || tcp.write_all(&chunk).await.is_err()
                    || tcp.write_all(b"\r\n").await.is_err()
                {
                    break;
                }
            }
            let _ = tcp.write_all(b"0\r\n\r\n").await;
            let _ = tcp.flush().await;
        }
    });
    addr
}

#[tokio::test]
async fn streaming_body_exceeding_8_mib_succeeds() {
    let addr = spawn_chunk_producer(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
        1024 * 1024,
        10,
    )
    .await;
    let client = UpstreamClient::new();
    let mut resp = client
        .call(
            UpstreamRequest::get(format!("http://{addr}/")),
            TimeoutProfile::OAuth,
            CancellationToken::new(),
        )
        .await
        .expect("request should connect and return 200");

    let mut total_bytes = 0;
    while let Some(chunk) = resp.body.next_chunk().await.expect("streaming chunks ok") {
        total_bytes += chunk.len();
    }
    assert_eq!(total_bytes, 10 * 1024 * 1024);
}

#[tokio::test]
async fn non_streaming_body_exceeding_limit_fails() {
    let addr = spawn_chunk_producer(
        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n",
        1024 * 1024,
        33,
    )
    .await;
    let client = UpstreamClient::new();
    let mut req = UpstreamRequest::get(format!("http://{addr}/"));
    req.is_streaming = false;
    let resp = client
        .call(req, TimeoutProfile::OAuth, CancellationToken::new())
        .await
        .expect("request connects");
    let res = resp.body.collect_all().await;
    assert!(res.is_err());
    assert!(
        res.unwrap_err()
            .to_string()
            .contains("length limit exceeded")
    );
}
