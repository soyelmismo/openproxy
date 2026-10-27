//! Real socket regressions: an upstream cuts a body after sending a valid SSE frame.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use openproxy_adapters::adapters::{AdapterFormat, ProviderAdapterEnum};
use openproxy_types::{Combo, ComboId, ComboTarget, ComboTargetId, Model, ModelRowId, ProviderId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::context::{PipelineContext, ResolvedTarget};
use crate::stage::{PipelineChain, PipelineStageEnum};
use crate::test_utils::{MockAdapter, fresh_pool, make_request, test_config};

async fn interrupted_stream(first_body: &'static str, race_size: u8, discard: bool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0; 4096];
                let header_end = loop {
                    let n = socket.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let len = headers
                    .lines()
                    .filter_map(|l| l.split_once(':'))
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    .map_or(0, |(_, v)| v.trim().parse::<usize>().unwrap());
                while request.len() < header_end + len {
                    let n = socket.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    request.extend_from_slice(&buf[..n]);
                }
                let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
                // Other race lanes fail before producing any bytes. The first
                // lane commits a frame, then loses the connection.
                if first {
                    let headers = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 99999\r\nconnection: close\r\n\r\n";
                    socket.write_all(headers.as_bytes()).await.unwrap();
                    socket.write_all(first_body.as_bytes()).await.unwrap();
                    socket.flush().await.unwrap();
                    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                } else if race_size > 1 {
                    socket.write_all(b"HTTP/1.1 503 Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await.unwrap();
                } else {
                    let body = "data: {\"id\":\"second\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"replacement\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                }
                let _ = socket.shutdown().await;
            });
        }
    });

    let (_pool, conn, _path) = fresh_pool();
    let mut config = test_config(Arc::new(
        openproxy_db::secrets::MasterKey::generate().unwrap(),
    ));
    config.retries.max_attempts = 2;
    config.retries.backoff_base_ms = 0;
    config.racing.max_race_size = race_size.max(1);
    config.adapters = Arc::new(
        (1..=3)
            .map(|n| {
                ProviderAdapterEnum::Mock(Box::new(MockAdapter::new(
                    &format!("cut-{n}"),
                    &format!("http://127.0.0.1:{port}"),
                    AdapterFormat::Openai,
                )))
            })
            .collect(),
    );
    let pipeline = crate::Pipeline::new(conn, config);
    let combo = Combo {
        id: ComboId(1),
        name: "cut".into(),
        race_size,
        ..Default::default()
    };
    let targets = (1..=3)
        .map(|n| ResolvedTarget {
            target: ComboTarget {
                id: ComboTargetId(n),
                combo_id: combo.id,
                provider_id: ProviderId::new(format!("cut-{n}")),
                priority_order: n as i32,
                ..Default::default()
            },
            model: Model {
                row_id: ModelRowId(n),
                provider_id: ProviderId::new(format!("cut-{n}")),
                model_id: "test".into(),
                ..Default::default()
            },
            api_key: "test-key".into(),
            ..Default::default()
        })
        .collect();
    let (mut req, _cancel) = make_request(combo.id);
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    Arc::make_mut(&mut req.openai_request).stream = !discard;
    req.stream_sink = Some(if discard {
        crate::StreamSink::Discard
    } else {
        crate::StreamSink::direct(tx)
    });
    let mut ctx = PipelineContext::new(req, pipeline);
    ctx.combo = Some(combo);
    ctx.targets = targets;
    let chain = PipelineChain::new(vec![PipelineStageEnum::UpstreamExecutor(
        super::super::UpstreamExecutorStage,
    )]);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        chain.execute_nested(&mut ctx),
    )
    .await
    .unwrap()
    .unwrap();
    let mut wire = Vec::new();
    while let Ok(chunk) = rx.try_recv() {
        wire.extend_from_slice(&chunk);
    }
    let wire = String::from_utf8(wire).unwrap();
    server.abort();
    if first_body.contains("partial") && !discard {
        assert!(
            result.error.is_some(),
            "a committed partial response must fail, not restart: {wire}"
        );
        assert!(
            !wire.contains("replacement"),
            "must not append a second generation: {wire}"
        );
        assert!(
            !wire.contains("[DONE]"),
            "interruption must not signal successful completion: {wire}"
        );
        if race_size == 1 {
            assert_eq!(hits.load(Ordering::SeqCst), 1);
        } else {
            assert!(hits.load(Ordering::SeqCst) <= usize::from(race_size));
        }
    } else {
        assert!(
            result.error.is_none(),
            "failure before client output must remain retryable: {:?}",
            result.error
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }
}

const PARTIAL: &str = "data: {\"id\":\"first\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";

#[tokio::test]
async fn midstream_cut_never_retries_or_rotates_after_output() {
    interrupted_stream(PARTIAL, 1, false).await;
}

#[tokio::test]
async fn midstream_cut_race_winner_does_not_take_another_target() {
    interrupted_stream(PARTIAL, 2, false).await;
}

#[tokio::test]
async fn midstream_cut_before_output_still_retries() {
    interrupted_stream(": upstream keepalive\n\n", 1, false).await;
}

#[tokio::test]
async fn midstream_cut_sync_discard_can_retry_safely() {
    interrupted_stream(PARTIAL, 1, true).await;
}
