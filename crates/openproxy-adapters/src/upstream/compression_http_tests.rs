//! HTTP-level tests for transparent content-encoding, against a real
//! `UpstreamClient` over a real TCP socket.
//!
//! These cover the two places the transport touches the wire: `call` negotiates
//! on the way out, and the response body is inverted before `next_chunk`
//! returns. Every request shape goes through the same path, so unary JSON, SSE
//! and multipart are exercised here rather than per provider.

#![cfg(all(feature = "upstream-hyper", test))]

use super::compression_tests::{gzip_container, zlib_container};
use super::{CancellationToken, TimeoutProfile, UpstreamClient, UpstreamError, UpstreamRequest};
use bytes::Bytes;
use http::StatusCode;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const SSE_PAYLOAD: &str = concat!(
    "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hola\"}}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" mundo\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

// Mock server plumbing

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// `true` once the request head and its declared body have both arrived.
fn request_is_complete(request: &[u8]) -> bool {
    let Some(index) = find(request, b"\r\n\r\n") else {
        return false;
    };
    let head_end = index + 4;
    let Ok(head) = std::str::from_utf8(&request[..index]) else {
        return true;
    };
    let declared = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .find_map(|(_, value)| value.trim().parse::<usize>().ok());
    match declared {
        Some(length) => request.len() >= head_end + length,
        None => true,
    }
}

/// Serve one request, replying with whatever `handler` builds from the raw
/// request bytes. The handler sees the exact bytes on the wire, which is how
/// the negotiation assertions are made.
async fn spawn_responder<F>(handler: F) -> std::net::SocketAddr
where
    F: Fn(&[u8]) -> Vec<u8> + Send + 'static,
{
    let Ok(listener) = TcpListener::bind("127.0.0.1:0").await else {
        panic!("failed to bind mock upstream");
    };
    let Ok(addr) = listener.local_addr() else {
        panic!("mock upstream has no local address");
    };
    tokio::spawn(async move {
        if let Ok((mut tcp, _)) = listener.accept().await {
            let mut request: Vec<u8> = Vec::new();
            let mut buf = [0u8; 8192];
            while !request_is_complete(&request) {
                match tcp.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(read) => request.extend_from_slice(&buf[..read]),
                    Err(_) => break,
                }
            }
            let response = handler(&request);
            let _ = tcp.write_all(&response).await;
            let _ = tcp.flush().await;
            let _ = tcp.shutdown().await;
        }
    });
    addr
}

/// Serve one request and then go silent, holding the connection open so a
/// cancel or a timeout is the only thing that can end the body read.
async fn spawn_stalling_responder(
    status_line: &'static str,
    headers: &'static [(&'static str, &'static str)],
    first_frame: Vec<u8>,
    stall: Duration,
) -> std::net::SocketAddr {
    let Ok(listener) = TcpListener::bind("127.0.0.1:0").await else {
        panic!("failed to bind mock upstream");
    };
    let Ok(addr) = listener.local_addr() else {
        panic!("mock upstream has no local address");
    };
    tokio::spawn(async move {
        if let Ok((mut tcp, _)) = listener.accept().await {
            let mut buf = [0u8; 8192];
            let _ = tcp.read(&mut buf).await;
            let mut response = head(status_line, headers);
            response.extend_from_slice(&chunked_frame(&first_frame));
            let _ = tcp.write_all(&response).await;
            let _ = tcp.flush().await;
            tokio::time::sleep(stall).await;
        }
    });
    addr
}

fn head(status_line: &str, headers: &[(&str, &str)]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status_line}\r\n").into_bytes();
    for (name, value) in headers {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out
}

fn body_response(status_line: &str, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status_line}\r\n").into_bytes();
    for (name, value) in headers {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

fn chunked_frame(chunk: &[u8]) -> Vec<u8> {
    let mut out = format!("{:x}\r\n", chunk.len()).into_bytes();
    out.extend_from_slice(chunk);
    out.extend_from_slice(b"\r\n");
    out
}

fn url(addr: std::net::SocketAddr) -> String {
    format!("http://{addr}/v1/chat/completions")
}

/// Split a request into its head and body.
fn split_request(raw: &[u8]) -> (&[u8], &[u8]) {
    match find(raw, b"\r\n\r\n") {
        Some(index) => (&raw[..index], &raw[index + 4..]),
        None => (raw, &[]),
    }
}

fn head_text(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).to_ascii_lowercase()
}

// Request-side negotiation

#[tokio::test]
async fn unary_request_advertises_the_codecs_we_can_invert() {
    let addr = spawn_responder(|request| {
        let length = request.len().to_string();
        body_response(
            "200 OK",
            &[
                ("content-type", "application/octet-stream".to_string()),
                ("content-length", length),
            ],
            request,
        )
    })
    .await;

    let mut spec = UpstreamRequest::post_json(
        url(addr),
        Bytes::from_static(br#"{"model":"gpt-4o-mini","stream":false}"#),
    );
    spec.is_streaming = false;
    let response = UpstreamClient::new()
        .call(spec, TimeoutProfile::OAuth, CancellationToken::new())
        .await
        .expect("call ok");
    assert_eq!(response.status, StatusCode::OK);
    let echoed = response.collect().await.expect("collect ok");

    let (head, body) = split_request(&echoed);
    assert_eq!(body, br#"{"model":"gpt-4o-mini","stream":false}"#);
    assert!(head_text(head).contains("accept-encoding: gzip, deflate"));
    // The request payload is never compressed by this transport.
    assert!(!head_text(head).contains("content-encoding:"));
}

#[tokio::test]
async fn explicit_accept_encoding_is_an_opt_out() {
    for explicit in ["identity", "gzip", "br, zstd"] {
        // `spawn_responder` serves exactly one connection, so each iteration
        // needs its own listener.
        let addr = spawn_responder(move |request| {
            let length = request.len().to_string();
            body_response(
                "200 OK",
                &[
                    ("content-type", "application/octet-stream".to_string()),
                    ("content-length", length),
                ],
                request,
            )
        })
        .await;
        let mut spec = UpstreamRequest::get(url(addr));
        if let Ok(value) = http::HeaderValue::from_str(explicit) {
            spec.headers.insert(http::header::ACCEPT_ENCODING, value);
        }
        let response = UpstreamClient::new()
            .call(spec, TimeoutProfile::OAuth, CancellationToken::new())
            .await
            .expect("call ok");
        let echoed = response.collect().await.expect("collect ok");
        let (head, _) = split_request(&echoed);
        let text = head_text(head);
        assert!(
            text.contains(&format!("accept-encoding: {explicit}")),
            "{text}"
        );
        if explicit == "identity" {
            assert!(!text.contains("gzip, deflate"), "{text}");
        }
    }
}

#[tokio::test]
async fn multipart_upload_body_is_untouched_by_negotiation() {
    let multipart = concat!(
        "--bnd\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.ogg\"\r\n",
        "Content-Type: audio/ogg\r\n\r\nfake-audio-bytes\r\n--bnd--\r\n",
    );
    let payload = Bytes::from_static(multipart.as_bytes());
    let addr = spawn_responder(move |request| {
        let length = request.len().to_string();
        body_response(
            "200 OK",
            &[
                ("content-type", "application/octet-stream".to_string()),
                ("content-length", length),
            ],
            request,
        )
    })
    .await;

    let spec = UpstreamRequest::post_multipart(
        url(addr),
        "multipart/form-data; boundary=bnd",
        payload.clone(),
    );
    let response = UpstreamClient::new()
        .call(spec, TimeoutProfile::OAuth, CancellationToken::new())
        .await
        .expect("call ok");
    let echoed = response.collect().await.expect("collect ok");
    let (head, body) = split_request(&echoed);

    assert_eq!(body, payload.as_ref());
    assert!(head_text(head).contains("accept-encoding: gzip, deflate"));
    assert!(!head_text(head).contains("content-encoding:"));
    assert!(head_text(head).contains("multipart/form-data; boundary=bnd"));
}

// Response-side decoding

#[tokio::test]
async fn unary_gzip_response_is_decoded_and_wire_headers_rewritten() {
    let payload = br#"{"id":"chatcmpl-1","choices":[{"message":{"role":"assistant","content":"hola mundo"}}]}"#;
    let container = gzip_container(payload, 0x00);
    let addr = spawn_responder(move |_request| {
        let length = container.len().to_string();
        body_response(
            "200 OK",
            &[
                ("content-type", "application/json".to_string()),
                ("content-encoding", "gzip".to_string()),
                ("content-length", length),
            ],
            &container,
        )
    })
    .await;

    let mut spec = UpstreamRequest::post_json(
        url(addr),
        Bytes::from_static(br#"{"model":"gpt-4o-mini","stream":false}"#),
    );
    spec.is_streaming = false;
    let response = UpstreamClient::new()
        .call(spec, TimeoutProfile::OAuth, CancellationToken::new())
        .await
        .expect("call ok");

    assert_eq!(response.status, StatusCode::OK);
    // The caller gets headers describing plaintext, not the wire.
    assert!(response.headers.get("content-encoding").is_none());
    assert!(response.headers.get("content-length").is_none());
    assert_eq!(
        response
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );

    let decoded = response.collect().await.expect("collect ok");
    assert_eq!(decoded.as_ref(), payload);
}

#[tokio::test]
async fn unary_deflate_zlib_response_is_decoded() {
    let payload = br#"{"object":"list","data":[{"id":"model-a"}]}"#;
    let container = zlib_container(payload);
    let addr = spawn_responder(move |_request| {
        let length = container.len().to_string();
        body_response(
            "200 OK",
            &[
                ("content-type", "application/json".to_string()),
                ("content-encoding", "deflate".to_string()),
                ("content-length", length),
            ],
            &container,
        )
    })
    .await;

    let mut spec = UpstreamRequest::get(url(addr));
    spec.is_streaming = false;
    let response = UpstreamClient::new()
        .call(spec, TimeoutProfile::OAuth, CancellationToken::new())
        .await
        .expect("call ok");
    let decoded = response.collect().await.expect("collect ok");
    assert_eq!(decoded.as_ref(), payload);
}

#[tokio::test]
async fn sse_gzip_response_is_decoded_across_transport_frames() {
    let container = gzip_container(SSE_PAYLOAD.as_bytes(), 0x00);
    let addr = spawn_responder(move |_request| {
        let mut response = head(
            "200 OK",
            &[
                ("content-type", "text/event-stream"),
                ("content-encoding", "gzip"),
                ("transfer-encoding", "chunked"),
            ],
        );
        // Deliberately small frames so the gzip stream is split mid-symbol.
        for frame in container.chunks(17) {
            response.extend_from_slice(&chunked_frame(frame));
        }
        response.extend_from_slice(b"0\r\n\r\n");
        response
    })
    .await;

    let spec = UpstreamRequest::post_json(
        url(addr),
        Bytes::from_static(br#"{"model":"gpt-4o-mini","stream":true}"#),
    );
    let mut response = UpstreamClient::new()
        .call(spec, TimeoutProfile::Chat, CancellationToken::new())
        .await
        .expect("call ok");
    assert!(response.headers.get("content-encoding").is_none());

    let mut collected: Vec<u8> = Vec::new();
    let mut frames = 0usize;
    while let Some(chunk) = response.body.next_chunk().await.expect("chunk ok") {
        collected.extend_from_slice(&chunk);
        frames += 1;
        response.body.note_content_chunk();
    }
    assert!(frames >= 1, "expected at least one decoded frame");
    // Every SSE event survived, in order: the decoder is transparent.
    for event in [
        "\"content\":\"Hola\"",
        "\"content\":\" mundo\"",
        "\"finish_reason\":\"stop\"",
        "data: [DONE]",
    ] {
        assert!(
            String::from_utf8_lossy(&collected).contains(event),
            "missing {event}"
        );
    }
    assert_eq!(collected.as_slice(), SSE_PAYLOAD.as_bytes());
}

#[tokio::test]
async fn multipart_gzip_response_is_decoded() {
    let payload = br#"{"text":"transcripcion de prueba"}"#;
    let container = gzip_container(payload, 0x00);
    let addr = spawn_responder(move |_request| {
        let length = container.len().to_string();
        body_response(
            "200 OK",
            &[
                ("content-type", "application/json".to_string()),
                ("content-encoding", "gzip".to_string()),
                ("content-length", length),
            ],
            &container,
        )
    })
    .await;

    let spec = UpstreamRequest::post_multipart(
        url(addr),
        "multipart/form-data; boundary=bnd",
        Bytes::from_static(b"--bnd--\r\n"),
    );
    let response = UpstreamClient::new()
        .call(spec, TimeoutProfile::Quota, CancellationToken::new())
        .await
        .expect("call ok");
    let decoded = response.collect().await.expect("collect ok");
    assert_eq!(decoded.as_ref(), payload);
}

#[tokio::test]
async fn truncated_gzip_stream_is_reported_as_a_decode_error() {
    let payload = SSE_PAYLOAD.as_bytes();
    let container = gzip_container(payload, 0x00);
    // Cut into the DEFLATE stream itself, not just its 8-byte trailer.
    let cut = container.len() / 2;
    let addr = spawn_responder(move |_request| {
        let body = container.get(..cut).map(<[u8]>::to_vec).unwrap_or_default();
        let length = body.len().to_string();
        body_response(
            "200 OK",
            &[
                ("content-type", "text/event-stream".to_string()),
                ("content-encoding", "gzip".to_string()),
                ("content-length", length),
            ],
            &body,
        )
    })
    .await;

    let spec = UpstreamRequest::post_json(
        url(addr),
        Bytes::from_static(br#"{"model":"gpt-4o-mini","stream":true}"#),
    );
    let response = UpstreamClient::new()
        .call(spec, TimeoutProfile::Chat, CancellationToken::new())
        .await
        .expect("headers arrive fine");
    let err = response
        .collect()
        .await
        .expect_err("a truncated stream must not look like a complete one");
    let message = err.to_string();
    assert!(message.contains("truncated"), "{message}");
}

#[tokio::test]
async fn unsupported_codec_fails_before_any_body_byte_is_read() {
    let addr = spawn_responder(|_request| {
        body_response(
            "200 OK",
            &[
                ("content-type", "application/json".to_string()),
                ("content-encoding", "compress".to_string()),
                ("content-length", "4".to_string()),
            ],
            b"raw!",
        )
    })
    .await;

    let spec = UpstreamRequest::get(url(addr));
    let err = UpstreamClient::new()
        .call(spec, TimeoutProfile::OAuth, CancellationToken::new())
        .await
        .expect_err("compress is outside the codec scope");
    let message = err.to_string();
    assert!(message.contains("compress"), "{message}");
    assert!(message.contains("not supported"), "{message}");
}

#[tokio::test]
async fn cancel_still_fires_while_decoding_a_compressed_stream() {
    let container = gzip_container(SSE_PAYLOAD.as_bytes(), 0x00);
    // Only the first half of the compressed stream is ever sent, so the body
    // read cannot complete on its own: the cancel is what ends it.
    let first = container
        .get(..container.len() / 2)
        .map(<[u8]>::to_vec)
        .unwrap_or_default();
    let addr = spawn_stalling_responder(
        "200 OK",
        &[
            ("content-type", "text/event-stream"),
            ("content-encoding", "gzip"),
            ("transfer-encoding", "chunked"),
        ],
        first,
        Duration::from_secs(10),
    )
    .await;

    let cancel = CancellationToken::new();
    let spec = UpstreamRequest::post_json(
        url(addr),
        Bytes::from_static(br#"{"model":"gpt-4o-mini","stream":true}"#),
    );
    // Bounded profile: if the decoder ever failed to yield the first chunk,
    // the test times out instead of hanging.
    let mut response = UpstreamClient::new()
        .call(spec, TimeoutProfile::OAuth, cancel.clone())
        .await
        .expect("headers arrive fine");

    let chunk = response
        .body
        .next_chunk()
        .await
        .expect("first decoded chunk ok")
        .expect("decoded bytes");
    assert!(!chunk.is_empty());

    cancel.cancel();
    let err = response
        .body
        .next_chunk()
        .await
        .expect_err("cancel must win");
    assert!(matches!(err, UpstreamError::Cancel), "{err}");
}

#[tokio::test]
async fn pending_output_drains_before_polling_a_live_connection() {
    let payload = vec![b'a'; 12 * 1024 * 1024];
    let container = gzip_container(&payload, 0x00);
    let addr = spawn_stalling_responder(
        "200 OK",
        &[
            ("content-encoding", "gzip"),
            ("transfer-encoding", "chunked"),
        ],
        container,
        Duration::from_secs(5),
    )
    .await;
    let cancel = CancellationToken::new();
    let mut response = UpstreamClient::new()
        .call(
            UpstreamRequest::get(url(addr)),
            TimeoutProfile::Chat,
            cancel.clone(),
        )
        .await
        .unwrap();
    let mut received = Vec::new();
    while received.len() < payload.len() {
        let chunk = tokio::time::timeout(Duration::from_millis(500), response.body.next_chunk())
            .await
            .expect("must drain without waiting for upstream EOF")
            .unwrap()
            .unwrap();
        received.extend_from_slice(&chunk);
    }
    assert_eq!(received, payload);
    cancel.cancel();
    assert!(matches!(
        response.body.next_chunk().await,
        Err(UpstreamError::Cancel)
    ));
}

#[tokio::test]
async fn large_compressed_response_collects_all_decoded_frames() {
    let payload = vec![b'x'; 12 * 1024 * 1024];
    let container = gzip_container(&payload, 0x00);
    let addr = spawn_responder(move |_| {
        body_response(
            "200 OK",
            &[
                ("content-encoding", "gzip".to_string()),
                ("content-length", container.len().to_string()),
            ],
            &container,
        )
    })
    .await;
    let mut spec = UpstreamRequest::get(url(addr));
    spec.is_streaming = false;
    let response = UpstreamClient::new()
        .call(spec, TimeoutProfile::Chat, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.collect().await.unwrap(), payload);
}

#[tokio::test]
async fn compressed_error_body_is_decoded_without_changing_status() {
    let payload = br#"{"error":{"message":"quota exceeded"}}"#;
    let container = gzip_container(payload, 0x00);
    let addr = spawn_responder(move |_| {
        body_response(
            "400 Bad Request",
            &[
                ("content-encoding", "gzip".to_string()),
                ("content-length", container.len().to_string()),
            ],
            &container,
        )
    })
    .await;
    let response = UpstreamClient::new()
        .call(
            UpstreamRequest::get(url(addr)),
            TimeoutProfile::OAuth,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.collect().await.unwrap().as_ref(), payload);
}
