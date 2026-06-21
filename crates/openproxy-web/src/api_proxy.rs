//! Reverse proxy: /web/api/* → ${OPENPROXY_CORE_URL}/admin/*
//!
//! Evita problemas de CORS y simplifica el cliente (no necesita
//! OPENPROXY_CORE_URL en el browser).

use axum::{
    body::Body,
    extract::{Request, State, ws::WebSocketUpgrade},
    http::{HeaderMap, StatusCode, Method, Version},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::MaybeTlsStream;
use futures::{SinkExt, StreamExt};

use crate::WebState;

pub fn router() -> axum::Router<WebState> {
    axum::Router::new()
        .route("/usage/stream", get(websocket_handler))
        .fallback(http_handler)
}

async fn websocket_handler(
    State(state): State<WebState>,
    ws: WebSocketUpgrade,
    req: Request,
) -> Response {
    // Convert HTTP(S) URL to WS(S) scheme for upstream WebSocket connection
    let upstream_base = if state.core_url.starts_with("http://") {
        format!("ws://{}", &state.core_url[7..])
    } else if state.core_url.starts_with("https://") {
        format!("wss://{}", &state.core_url[8..])
    } else {
        state.core_url.clone()
    };
    let mut upstream_url = format!("{}/admin/usage/stream", upstream_base);
    if let Some(token) = &state.admin_token {
        upstream_url = format!("{}?token={}", upstream_url, token);
    }
    let mut upstream_headers = HeaderMap::new();
    
    // Copy all non‑hop‑by‑hop headers from the client request
    for (key, value) in req.headers().iter() {
        if !is_hop_by_hop(key.as_str()) {
            upstream_headers.insert(key, value.clone());
        }
    }

    // Ensure Host header is forwarded
    if let Some(host) = req.headers().get("host").and_then(|h| h.to_str().ok()) {
        upstream_headers.insert("host", host.parse().unwrap());
    }

    // Add admin token header if not already present
    if upstream_headers.get("authorization").is_none() {
        if let Some(token) = &state.admin_token {
            upstream_headers.insert("authorization", format!("Bearer {}", token).parse().unwrap());
        }
    }

    let upstream_ws_connect = connect_async_with_headers(upstream_url.as_str(), upstream_headers)
        .await;

    let (upstream_ws, _) = match upstream_ws_connect {
        Ok(ws) => ws,
        Err(e) => {
            tracing::error!(error = %e, "failed to connect to upstream websocket");
            return (
                StatusCode::BAD_GATEWAY,
                format!("failed to connect to upstream: {}", e),
            )
                .into_response();
        }
    };

    ws.on_upgrade(move |socket| handle_websocket_proxy(socket, upstream_ws))
        .into_response()
}

async fn http_handler(State(state): State<WebState>, req: Request) -> Response {
    let path = req.uri().path();
    let upstream_path = format!("/admin{}", path);
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{}", q))
        .unwrap_or_default();
    let upstream_url = format!(
        "{}{}{}",
        state.core_url.trim_end_matches('/'),
        upstream_path,
        query
    );
    tracing::debug!(upstream = %upstream_url, "proxy forward");

    let method = req.method().clone();
    let headers = req.headers().clone();
    let body_bytes = axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap_or_else(|_| Bytes::new());

    let mut upstream_req = state.http.request(method, &upstream_url);
    for (k, v) in headers.iter() {
        if is_hop_by_hop(k.as_str()) {
            continue;
        }
        upstream_req = upstream_req.header(k.as_str(), v.as_bytes());
    }
    // Inject admin token if the client didn't send one. Required for
    // admin endpoints that require auth (e.g. /admin/recording).
    if !headers.contains_key("authorization") {
        if let Some(token) = &state.admin_token {
            upstream_req = upstream_req.header("authorization", format!("Bearer {}", token));
        }
    }
    if !headers.contains_key("x-forwarded-host") {
        if let Some(host) = headers.get("host").and_then(|h| h.to_str().ok()) {
            upstream_req = upstream_req.header("x-forwarded-host", host);
        }
    }
    if !headers.contains_key("x-forwarded-proto") {
        upstream_req = upstream_req.header("x-forwarded-proto", "http");
    }
    if let Some(host) = headers.get("host").and_then(|h| h.to_str().ok()) {
        upstream_req = upstream_req.header("host", host);
    }
    if !body_bytes.is_empty() {
        upstream_req = upstream_req.body(body_bytes);
    }

    let upstream_resp = match upstream_req.send().await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("upstream error: {}", e),
            )
                .into_response();
        }
    };

    let status = upstream_resp.status();
    let headers = upstream_resp.headers().clone();
    let body = upstream_resp.bytes().await.unwrap_or_default();

    let mut response_headers = HeaderMap::new();
    for (k, v) in headers.iter() {
        if is_hop_by_hop(k.as_str()) {
            continue;
        }
        response_headers.insert(k, v.clone());
    }

    (status, response_headers, Body::from(body)).into_response()
}

async fn handle_websocket_proxy(
    client_ws: axum::extract::ws::WebSocket,
    upstream_ws: tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
) {
    let (mut client_ws_tx, mut client_ws_rx) = client_ws.split();
    let (mut upstream_ws_tx, mut upstream_ws_rx) = upstream_ws.split();

    let client_to_upstream = async {
        while let Some(Ok(msg)) = client_ws_rx.next().await {
            if let Some(t_msg) = to_tungstenite_msg(msg) {
                if let Err(e) = upstream_ws_tx.send(t_msg).await {
                    tracing::error!(error = %e, "error sending to upstream websocket");
                    break;
                }
            }
        }
        let _ = upstream_ws_tx.close().await;
    };

    let upstream_to_client = async {
        while let Some(Ok(msg)) = upstream_ws_rx.next().await {
            if let Some(a_msg) = to_axum_msg(msg) {
                if let Err(e) = client_ws_tx.send(a_msg).await {
                    tracing::error!(error = %e, "error sending to client websocket");
                    break;
                }
            }
        }
        let _ = client_ws_tx.close().await;
    };

    tokio::select! {
        _ = client_to_upstream => {},
        _ = upstream_to_client => {},
    }
}

async fn connect_async_with_headers(
    url: &str,
    headers: HeaderMap,
) -> Result<(tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, axum::http::Response<Option<Vec<u8>>>), tokio_tungstenite::tungstenite::Error> {
    let mut request_builder = axum::http::Request::builder()
        .method(Method::GET)
        .uri(url)
        .version(Version::HTTP_11)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", tungstenite::handshake::client::generate_key());

    if let Some(headers_map) = request_builder.headers_mut() {
        for (key, value) in headers.iter() {
            let key_str = key.as_str();
            if !key_str.eq_ignore_ascii_case("connection")
                && !key_str.eq_ignore_ascii_case("upgrade")
                && !key_str.eq_ignore_ascii_case("sec-websocket-version")
                && !key_str.eq_ignore_ascii_case("sec-websocket-key")
            {
                headers_map.insert(key, value.clone());
            }
        }
    }

    let request = request_builder
        .body(())
        .map_err(|e| tokio_tungstenite::tungstenite::Error::Io(std::io::Error::other(e)))?;

    let (ws_stream, response) = connect_async(request).await?;

    Ok((ws_stream, response))
}

fn to_tungstenite_msg(msg: axum::extract::ws::Message) -> Option<tokio_tungstenite::tungstenite::Message> {
    match msg {
        axum::extract::ws::Message::Text(s) => Some(tokio_tungstenite::tungstenite::Message::Text(s.to_string().into())),
        axum::extract::ws::Message::Binary(v) => Some(tokio_tungstenite::tungstenite::Message::Binary(v)),
        axum::extract::ws::Message::Ping(v) => Some(tokio_tungstenite::tungstenite::Message::Ping(v)),
        axum::extract::ws::Message::Pong(v) => Some(tokio_tungstenite::tungstenite::Message::Pong(v)),
        axum::extract::ws::Message::Close(frame) => {
            Some(tokio_tungstenite::tungstenite::Message::Close(frame.map(|f| {
                tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: f.code.into(),
                    reason: f.reason.to_string().into(),
                }
            })))
        }
    }
}

fn to_axum_msg(msg: tokio_tungstenite::tungstenite::Message) -> Option<axum::extract::ws::Message> {
    match msg {
        tokio_tungstenite::tungstenite::Message::Text(s) => Some(axum::extract::ws::Message::Text(s.to_string().into())),
        tokio_tungstenite::tungstenite::Message::Binary(v) => Some(axum::extract::ws::Message::Binary(v)),
        tokio_tungstenite::tungstenite::Message::Ping(v) => Some(axum::extract::ws::Message::Ping(v)),
        tokio_tungstenite::tungstenite::Message::Pong(v) => Some(axum::extract::ws::Message::Pong(v)),
        tokio_tungstenite::tungstenite::Message::Close(frame) => {
            Some(axum::extract::ws::Message::Close(frame.map(|f| {
                axum::extract::ws::CloseFrame {
                    code: f.code.into(),
                    reason: f.reason.to_string().into(),
                }
            })))
        }
        _ => None,
    }
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "host"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
    )
}
