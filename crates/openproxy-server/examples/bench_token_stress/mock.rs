use crate::config::{WorkerConfig, emit, read_line};
use anyhow::{Context as _, anyhow};
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::IntoResponse,
    routing::post,
};
use bytes::Bytes;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use hyper_util::service::TowerToHyperService;
use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio_rustls::TlsAcceptor;
use tokio_stream::wrappers::ReceiverStream;

struct Payloads {
    regular: Bytes,
    remainder: Option<Bytes>,
    terminal: Bytes,
    count: usize,
    interval: std::time::Duration,
    cut_after_events: usize,
    cut_percent: u64,
    request_counter: AtomicUsize,
}

fn content_frame(tokens: usize) -> Bytes {
    Bytes::from(format!(
        "data: {}\n\n",
        json!({
            "id": "bench-chunk", "object": "chat.completion.chunk", "created": 1700000000,
            "model": "bench-stream-model", "choices": [{"index": 0, "delta": {"content": " tok".repeat(tokens)}, "finish_reason": null}]
        })
    ))
}

fn cut_request(request_index: u64, percent: u64) -> bool {
    request_index % 100 < percent
}

fn is_warmup(request: &serde_json::Value) -> bool {
    request["messages"][0]["content"]
        .as_str()
        .is_some_and(|s| s.starts_with("bench client warmup request #"))
}

pub fn self_test() -> anyhow::Result<()> {
    anyhow::ensure!(
        is_warmup(
            &json!({"messages":[{"role":"user","content":"bench client warmup request #0"}]})
        ),
        "warmup must be recognized from the forwarded body, not a private header"
    );
    anyhow::ensure!(
        !is_warmup(
            &json!({"messages":[{"role":"user","content":"bench client stream request #0"}]})
        ),
        "measured request must not bypass injected faults"
    );
    for percent in [0, 1, 20, 50, 100] {
        anyhow::ensure!(
            (0..100).filter(|&i| cut_request(i, percent)).count() as u64 == percent,
            "upstream cut percentage must match each 100-request window"
        );
    }
    Ok(())
}

async fn chat(
    State(payloads): State<Arc<Payloads>>,
    Json(request): Json<serde_json::Value>,
) -> impl IntoResponse {
    let should_cut = !is_warmup(&request)
        && payloads.cut_after_events > 0
        && cut_request(
            payloads.request_counter.fetch_add(1, Ordering::Relaxed) as u64,
            payloads.cut_percent,
        );

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    tokio::spawn(async move {
        // Optional pacing emulates a real model's inter-token gap. Delay keeps
        // the spacing when the mock falls behind instead of bursting to catch up.
        let mut pace = (!payloads.interval.is_zero()).then(|| {
            let mut tick = tokio::time::interval(payloads.interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick
        });
        let mut events_emitted = 0;
        for _ in 0..payloads.count {
            if let Some(tick) = pace.as_mut() {
                tick.tick().await;
            }
            if should_cut && events_emitted >= payloads.cut_after_events {
                let _ = tx
                    .send(Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "upstream fault injected: stream cut after requested event count",
                    )))
                    .await;
                return;
            }
            if tx.send(Ok(payloads.regular.clone())).await.is_err() {
                return;
            }
            events_emitted += 1;
        }
        if let Some(tail) = &payloads.remainder {
            if should_cut && events_emitted >= payloads.cut_after_events {
                let _ = tx
                    .send(Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "upstream fault injected: stream cut before tail event",
                    )))
                    .await;
                return;
            }
            if tx.send(Ok(tail.clone())).await.is_err() {
                return;
            }
            events_emitted += 1;
        }
        if should_cut && events_emitted >= payloads.cut_after_events {
            let _ = tx
                .send(Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "upstream fault injected: stream cut before terminal event",
                )))
                .await;
            return;
        }
        let _ = tx.send(Ok(payloads.terminal.clone())).await;
    });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(ReceiverStream::new(rx)),
    )
}

/// TLS+h2 mock upstream (bench only). The auto builder serves HTTP/2 when
/// ALPN negotiated `h2` and HTTP/1.1 otherwise, so the proxy exercises the
/// same transport path production uses against real providers.
async fn serve_tls(
    listener: tokio::net::TcpListener,
    app: Router,
    dir: std::path::PathBuf,
) -> anyhow::Result<()> {
    let certs = CertificateDer::pem_file_iter(dir.join("server.pem"))
        .and_then(|iter| iter.collect::<Result<Vec<_>, _>>())
        .context("read server.pem")?;
    let key = PrivateKeyDer::from_pem_file(dir.join("server.key")).context("read server.key")?;
    let mut tls = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow!("invalid bench certificate: {e}"))?;
    // The experiment measures HTTPS/h2 only. Refuse ALPN fallback here so
    // successful runs also prove the proxy used h2, not merely that curl can.
    tls.alpn_protocols = vec![b"h2".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let Ok(tls_stream) = acceptor.accept(stream).await else {
                return;
            };
            if tls_stream.get_ref().1.alpn_protocol() != Some(b"h2".as_slice()) {
                tracing::error!("TLS benchmark requires negotiated ALPN h2");
                return;
            }
            let _ = AutoBuilder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(tls_stream), TowerToHyperService::new(app))
                .await;
        });
    }
}

pub async fn run(config: WorkerConfig) -> anyhow::Result<()> {
    // Prebuild deterministic upstream fixtures, outside the measured proxy.
    // Still emit one event per configured chunk. No event batching to inflate scores.
    let payloads = Arc::new(Payloads {
        regular: content_frame(config.chunk),
        remainder: (!config.tokens.is_multiple_of(config.chunk))
            .then(|| content_frame(config.tokens % config.chunk)),
        terminal: Bytes::from(format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({
                "id": "bench-chunk", "object": "chat.completion.chunk", "created": 1700000000,
                "model": "bench-stream-model", "choices": [{"index":0,"delta":{},"finish_reason":"stop"}],
                "usage": {"prompt_tokens":10,"completion_tokens":config.tokens,"total_tokens": config.tokens + 10}
            })
        )),
        count: config.tokens / config.chunk,
        interval: std::time::Duration::from_micros(config.event_interval_us),
        cut_after_events: config.upstream_cut_after_events,
        cut_percent: config.upstream_cut_percent,
        request_counter: AtomicUsize::new(0),
    });
    let listener = {
        let s = tokio::net::TcpSocket::new_v4()?;
        s.bind("127.0.0.1:0".parse()?)?;
        s.listen(4096)?
    };
    let scheme = if config.upstream_tls { "https" } else { "http" };
    let direct_addr = listener.local_addr()?;

    let (announced_port, _relay_shutdown) = if config.network_delay_ms > 0 {
        let (relay_addr, shutdown_tx) =
            crate::relay::start_relay(direct_addr, config.network_delay_ms).await?;
        (relay_addr.port(), Some(shutdown_tx))
    } else {
        (direct_addr.port(), None)
    };

    emit(
        &json!({"pid": std::process::id(), "url": format!("{scheme}://localhost:{announced_port}/v1")}),
    )?;
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .with_state(payloads);
    let server = if config.upstream_tls {
        let dir =
            std::env::var("BENCH_TLS_DIR").context("--upstream-tls requires BENCH_TLS_DIR")?;
        tokio::spawn(async move {
            let _ = serve_tls(listener, app, dir.into()).await;
        })
    } else {
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        })
    };
    tokio::task::spawn_blocking(read_line).await??;
    server.abort();
    Ok(())
}
