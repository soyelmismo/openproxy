use crate::config::{WorkerConfig, emit, read_line};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::IntoResponse,
    routing::post,
};
use bytes::Bytes;
use serde_json::json;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_stream::wrappers::ReceiverStream;

struct Payloads {
    regular: Bytes,
    remainder: Option<Bytes>,
    terminal: Bytes,
    count: usize,
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

async fn chat(State(payloads): State<Arc<Payloads>>) -> impl IntoResponse {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    tokio::spawn(async move {
        for _ in 0..payloads.count {
            if tx.send(Ok(payloads.regular.clone())).await.is_err() {
                return;
            }
        }
        if let Some(tail) = &payloads.remainder
            && tx.send(Ok(tail.clone())).await.is_err()
        {
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
    });
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    emit(
        &json!({"pid": std::process::id(), "url": format!("http://{}/v1", listener.local_addr()?)}),
    )?;
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .with_state(payloads);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    tokio::task::spawn_blocking(read_line).await??;
    server.abort();
    Ok(())
}
