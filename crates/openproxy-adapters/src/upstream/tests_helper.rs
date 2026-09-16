//! Mock upstream helpers shared across unit tests.

#![cfg(all(feature = "upstream-hyper", test))]

use super::UpstreamClient;
use hyper_util::rt::TokioIo;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone)]
pub struct SingleResponseConnector {
    addr: std::net::SocketAddr,
}

impl tower_service::Service<http::Uri> for SingleResponseConnector {
    type Response = TokioIo<tokio::net::TcpStream>;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _uri: http::Uri) -> Self::Future {
        let addr = self.addr;
        Box::pin(async move {
            let tcp = tokio::net::TcpStream::connect(addr).await?;
            Ok(TokioIo::new(tcp))
        })
    }
}

pub async fn build_mock_upstream_returning_status(status: u16, body: &str) -> Arc<UpstreamClient> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body_bytes = body.as_bytes().to_vec();
    let body_header = format!("content-length: {}\r\n\r\n", body_bytes.len());
    let status_line = format!("HTTP/1.1 {status} OK\r\ncontent-type: text/plain\r\n");
    tokio::spawn(async move {
        if let Ok((mut tcp, _peer)) = listener.accept().await {
            let mut buf = vec![0u8; 4096];
            let _ = tcp.read(&mut buf).await;
            let _ = tcp.write_all(status_line.as_bytes()).await;
            let _ = tcp.write_all(body_header.as_bytes()).await;
            let _ = tcp.write_all(&body_bytes).await;
            let _ = tcp.flush().await;
        }
    });

    let connector = SingleResponseConnector { addr };
    UpstreamClient::for_test_with_connector(connector, None)
}

pub async fn build_mock_upstream_routing<F>(handler: F) -> Arc<UpstreamClient>
where
    F: Fn(&str) -> (u16, String) + Send + Sync + 'static,
{
    let handler = Arc::new(handler);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            if let Ok((mut tcp, _peer)) = listener.accept().await {
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = tcp.read(&mut buf).await.unwrap_or(0);
                    let req_str = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let target = req_str
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("")
                        .to_string();
                    let (status, body) = handler(&target);
                    let status_line =
                        format!("HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\n");
                    let body_header = format!("content-length: {}\r\n\r\n", body.len());
                    let _ = tcp.write_all(status_line.as_bytes()).await;
                    let _ = tcp.write_all(body_header.as_bytes()).await;
                    let _ = tcp.write_all(body.as_bytes()).await;
                    let _ = tcp.flush().await;
                });
            }
        }
    });

    let connector = SingleResponseConnector { addr };
    UpstreamClient::for_test_with_connector(connector, None)
}

#[test]
fn test_build_hyper_request_injects_default_user_agent() {
    let req = super::UpstreamRequest::get("https://example.com/v1/models");
    let hyper_req = super::client::build_hyper_request(req).expect("build request");
    let ua = hyper_req
        .headers()
        .get(http::header::USER_AGENT)
        .expect("user-agent present");
    assert!(
        ua.to_str().expect("valid ascii").starts_with("openproxy/"),
        "expected User-Agent to start with openproxy/, got {ua:?}"
    );
}

#[test]
fn test_build_hyper_request_preserves_custom_user_agent() {
    let mut req = super::UpstreamRequest::get("https://example.com/v1/models");
    req.headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static("custom-agent/2.0"),
    );
    let hyper_req = super::client::build_hyper_request(req).expect("build request");
    let ua = hyper_req
        .headers()
        .get(http::header::USER_AGENT)
        .expect("user-agent present");
    assert_eq!(ua.to_str().expect("valid ascii"), "custom-agent/2.0");
}
