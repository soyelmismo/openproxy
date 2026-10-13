use super::*;
use bytes::Bytes;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn profile() -> TimeoutProfile {
    TimeoutProfile::Custom(ResolvedTimeouts {
        dns_ms: 2000,
        dial_ms: 2000,
        tls_ms: 2000,
        write_ms: 80,
        headers_ms: 100,
        body_chunk_ms: 80,
        total_ms: 1500,
    })
}
async fn read_request(tcp: &mut tokio::net::TcpStream) {
    let mut buf = Vec::new();
    let boundary = loop {
        let mut chunk = [0; 1024];
        let n = tcp.read(&mut chunk).await.unwrap();
        assert!(n > 0);
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..boundary]);
    let len: usize = headers
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap())
        })
        .unwrap_or(0);
    while buf.len() < boundary + len {
        let mut chunk = [0; 1024];
        let n = tcp.read(&mut chunk).await.unwrap();
        assert!(n > 0);
        buf.extend_from_slice(&chunk[..n]);
    }
}

#[tokio::test]
async fn unary_delayed_headers_and_body_survive_stream_and_send_budgets_on_pooled_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        for _ in 0..2 {
            read_request(&mut tcp).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            tcp.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            tcp.write_all(b"OK").await.unwrap();
        }
    });
    let client = UpstreamClient::new();
    for _ in 0..2 {
        let mut req =
            UpstreamRequest::post_json(format!("http://{addr}/"), Bytes::from_static(b"{}"));
        req.is_streaming = false;
        let resp = client
            .call(req, profile(), CancellationToken::new())
            .await
            .expect("unary uses total while generating");
        assert_eq!(resp.collect().await.unwrap(), Bytes::from_static(b"OK"));
    }
    server.await.unwrap();
    assert!(client.pool().reuses() >= 1);
}

#[tokio::test]
async fn unary_silent_response_is_total_not_write_or_ttft() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        read_request(&mut tcp).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    });
    let mut req = UpstreamRequest::post_json(format!("http://{addr}/"), Bytes::from_static(b"{}"));
    req.is_streaming = false;
    let mut timeout = profile().resolve();
    timeout.total_ms = 350;
    let err = UpstreamClient::new()
        .call(
            req,
            TimeoutProfile::Custom(timeout),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, UpstreamError::Timeout(UpstreamPhase::Total)),
        "{err:?}"
    );
    server.abort();
}

#[tokio::test]
async fn blocked_large_upload_still_times_out_write_in_both_modes() {
    for streaming in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_tcp, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let mut req = UpstreamRequest::post_json(
            format!("http://{addr}/"),
            Bytes::from(vec![b'x'; 32 * 1024 * 1024]),
        );
        req.is_streaming = streaming;
        let err = UpstreamClient::new()
            .call(req, profile(), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            matches!(err, UpstreamError::Timeout(UpstreamPhase::Write)),
            "{err:?}"
        );
        server.abort();
    }
}

#[derive(Clone)]
struct H2Connector(std::net::SocketAddr);
struct H2Io(hyper_util::rt::TokioIo<tokio::net::TcpStream>);
impl hyper_util::client::legacy::connect::Connection for H2Io {
    fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
        hyper_util::client::legacy::connect::Connected::new().negotiated_h2()
    }
}
impl hyper::rt::Read for H2Io {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
impl hyper::rt::Write for H2Io {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
impl tower_service::Service<http::Uri> for H2Connector {
    type Response = H2Io;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<H2Io, Self::Error>> + Send>>;
    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn call(&mut self, _: http::Uri) -> Self::Future {
        let addr = self.0;
        Box::pin(async move {
            Ok(H2Io(hyper_util::rt::TokioIo::new(
                tokio::net::TcpStream::connect(addr).await?,
            )))
        })
    }
}

#[tokio::test]
async fn http2_concurrent_unary_uploads_do_not_inherit_stream_timers() {
    use http_body_util::{BodyExt, Full};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(
                    |req: http::Request<hyper::body::Incoming>| async move {
                        let body = req.into_body().collect().await.unwrap().to_bytes();
                        assert!(!body.is_empty());
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        Ok::<_, std::convert::Infallible>(http::Response::new(Full::new(
                            Bytes::from_static(b"OK"),
                        )))
                    },
                );
                hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(hyper_util::rt::TokioIo::new(tcp), service)
                    .await
                    .unwrap();
            });
        }
    });
    let client = UpstreamClient::for_test_with_connector(H2Connector(addr), None);
    let call = |n| {
        let client = std::sync::Arc::clone(&client);
        async move {
            let mut req = UpstreamRequest::post_json("http://h2.test/", Bytes::from(vec![b'x'; n]));
            req.is_streaming = false;
            let resp = client
                .call(req, profile(), CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(resp.collect().await.unwrap(), Bytes::from_static(b"OK"));
        }
    };
    tokio::join!(call(200_000), call(2));
    server.abort();
}

#[tokio::test]
async fn h2_flow_control_stall_remains_an_upload_timeout() {
    use http_body_util::Full;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let service =
            hyper::service::service_fn(|req: http::Request<hyper::body::Incoming>| async move {
                // Hold the unread body alive: no stream WINDOW_UPDATE is returned.
                tokio::time::sleep(Duration::from_secs(2)).await;
                drop(req);
                Ok::<_, std::convert::Infallible>(http::Response::new(Full::new(
                    Bytes::from_static(b"OK"),
                )))
            });
        let _ = hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
            .serve_connection(hyper_util::rt::TokioIo::new(tcp), service)
            .await;
    });
    let client = UpstreamClient::for_test_with_connector(H2Connector(addr), None);
    let mut req =
        UpstreamRequest::post_json("http://h2.test/", Bytes::from(vec![b'x'; 32 * 1024 * 1024]));
    req.is_streaming = false;
    let err = client
        .call(req, profile(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(err, UpstreamError::Timeout(UpstreamPhase::Write)),
        "{err:?}"
    );
    server.abort();
}

#[tokio::test]
async fn cancelling_unary_generation_is_immediate() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (sent, uploaded) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        read_request(&mut tcp).await;
        sent.send(()).unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    });
    let mut req = UpstreamRequest::post_json(format!("http://{addr}/"), Bytes::from_static(b"{}"));
    req.is_streaming = false;
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let cancel = cancel.clone();
        async move { UpstreamClient::new().call(req, profile(), cancel).await }
    });
    uploaded.await.unwrap();
    cancel.cancel();
    assert!(matches!(
        task.await.unwrap().unwrap_err(),
        UpstreamError::Cancel
    ));
    server.abort();
}
