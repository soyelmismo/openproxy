use super::{
    AppState, Deserialize, HeaderMap, IntoResponse, Message, StatusCode, StreamExt, WebSocket,
    WebSocketUpgrade, authenticate_admin_ws, json,
};
use crate::handlers::admin::debug::json_text;
use axum::extract::{Query, State};
use openproxy_core::usage as core_usage;

pub const WS_OUTBOX_CAPACITY: usize = 2048;
pub const USAGE_RECENT_MAX_SINCE_ID: i64 = i64::MAX / 2;

#[derive(Debug, Deserialize)]
pub struct ClientWsMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub since_id: Option<i64>,
}

pub enum NotifRxEvent {
    Event(Box<openproxy_core::notifications::NotificationEvent>),
    Lagged(u64),
    Closed,
}

#[derive(Debug, Default, Deserialize)]
pub struct UsageStreamQuery {
    pub token: Option<String>,
}

fn is_allowed_origin(origin: &str, host: &str) -> bool {
    let is_same_host = !host.is_empty()
        && (origin == format!("http://{host}") || origin == format!("https://{host}"));
    if is_same_host {
        return true;
    }

    let origin = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .unwrap_or(origin);
    let host_part = origin.split(':').next().unwrap_or(origin);

    matches!(host_part, "localhost" | "127.0.0.1")
}

fn check_cswsh_origin(headers: &HeaderMap) -> Result<(), (StatusCode, &'static str)> {
    let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) else {
        return Ok(());
    };

    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !is_allowed_origin(origin, host) {
        tracing::warn!(
            origin = %origin,
            host = %host,
            "WebSocket connection rejected: non-matching origin"
        );
        return Err((StatusCode::FORBIDDEN, "Forbidden: origin mismatch"));
    }
    Ok(())
}

pub async fn usage_stream(
    State(s): State<AppState>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    Query(q): Query<UsageStreamQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    if let Err(resp) = check_cswsh_origin(&headers) {
        return resp.into_response();
    }

    match authenticate_admin_ws(&s, &headers, q.token.as_deref(), Some(&addr)) {
        Ok(()) => ws
            .on_upgrade(move |socket| stream_usage_rows(socket, s))
            .into_response(),
        Err(e) => e.into_response(),
    }
}

async fn outbox_send(tx: &tokio::sync::mpsc::Sender<Box<str>>, val: serde_json::Value) {
    if let Ok(text) = json_text(&val) {
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            tx.send(text.into_boxed_str()),
        )
        .await;
    }
}

fn outbox_try_send(tx: &tokio::sync::mpsc::Sender<Box<str>>, val: &serde_json::Value) {
    if let Ok(text) = json_text(val) {
        let _ = tx.try_send(text.into_boxed_str());
    }
}

async fn send_inflight_sync(outbox_tx: &tokio::sync::mpsc::Sender<Box<str>>) {
    let active = openproxy_core::usage::get_active_inflight_attempts();
    let snap_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    outbox_send(
        outbox_tx,
        json!({
            "type": "inflight_sync",
            "server_now": snap_now,
            "attempts": active,
        }),
    )
    .await;
}

async fn handle_stage_event(
    stage: Result<openproxy_types::usage::StageEvent, tokio::sync::broadcast::error::RecvError>,
    outbox_tx: &tokio::sync::mpsc::Sender<Box<str>>,
) -> bool {
    match stage {
        Ok(event) => {
            outbox_send(outbox_tx, json!({ "type": "stage", "data": event })).await;
            true
        }
        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
            tracing::warn!(
                "stage broadcast lagged; {} event(s) skipped — sending inflight snapshot",
                skipped
            );
            send_inflight_sync(outbox_tx).await;
            true
        }
        Err(tokio::sync::broadcast::error::RecvError::Closed) => false,
    }
}

async fn handle_usage_event(
    usage: Result<openproxy_types::usage::RecentUsageRow, tokio::sync::broadcast::error::RecvError>,
    last_known_id: &mut i64,
    outbox_tx: &tokio::sync::mpsc::Sender<Box<str>>,
) -> bool {
    match usage {
        Ok(row) => {
            if row.id.0 > *last_known_id {
                *last_known_id = row.id.0;
            }
            outbox_send(outbox_tx, json!({ "type": "row", "data": row })).await;
            true
        }
        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
            tracing::warn!(
                "usage broadcast lagged; {} row(s) skipped — sending inflight snapshot",
                skipped
            );
            send_inflight_sync(outbox_tx).await;
            true
        }
        Err(tokio::sync::broadcast::error::RecvError::Closed) => false,
    }
}

async fn recv_next_notification(
    notification_rx: &mut Option<
        tokio::sync::broadcast::Receiver<openproxy_core::notifications::NotificationEvent>,
    >,
) -> NotifRxEvent {
    match notification_rx.as_mut() {
        Some(rx) => match rx.recv().await {
            Ok(n) => NotifRxEvent::Event(Box::new(n)),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => NotifRxEvent::Lagged(n),
            Err(tokio::sync::broadcast::error::RecvError::Closed) => NotifRxEvent::Closed,
        },
        None => std::future::pending().await,
    }
}

async fn handle_notification_event(
    evt: NotifRxEvent,
    notification_rx: &mut Option<
        tokio::sync::broadcast::Receiver<openproxy_core::notifications::NotificationEvent>,
    >,
    outbox_tx: &tokio::sync::mpsc::Sender<Box<str>>,
) {
    match evt {
        NotifRxEvent::Event(n) => {
            outbox_send(outbox_tx, json!({ "type": "notification", "data": n })).await;
        }
        NotifRxEvent::Lagged(skipped) => {
            outbox_try_send(
                outbox_tx,
                &json!({
                    "type": "lag_warning",
                    "skipped": skipped,
                    "channel": "notifications",
                    "message": format!(
                        "notifications broadcast channel lagged; {} event(s) skipped — refetch via GET /admin/api/notifications",
                        skipped
                    ),
                }),
            );
        }
        NotifRxEvent::Closed => {
            *notification_rx = None;
        }
    }
}

async fn handle_client_subscribe(
    since_id: Option<i64>,
    state: &AppState,
    last_known_id: &mut i64,
    outbox_tx: &tokio::sync::mpsc::Sender<Box<str>>,
) {
    let since_id = since_id.unwrap_or(0).clamp(0, USAGE_RECENT_MAX_SINCE_ID);
    let pool = std::sync::Arc::clone(state.db_pool());
    let rows: Vec<openproxy_types::usage::RecentUsageRow> =
        tokio::task::spawn_blocking(move || {
            let r = pool.try_reader_for(std::time::Duration::from_secs(5));
            let Some(r) = r else {
                tracing::error!("stream_usage_rows: subscribe reader lock timeout");
                return Vec::new();
            };
            let rows = match core_usage::recent(&r, since_id, 100) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(error = %e, "stream_usage_rows: subscribe recent query failed");
                    Vec::new()
                }
            };
            drop(r);
            rows.into_iter()
                .map(openproxy_types::usage::redact_for_broadcast)
                .collect()
        })
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "stream_usage_rows: subscribe spawn_blocking failed");
            Vec::new()
        });
    if let Some(mx) = rows.iter().map(|r| r.id.0).max() {
        *last_known_id = (*last_known_id).max(mx);
    }
    outbox_send(outbox_tx, json!({ "type": "history", "rows": rows })).await;
}

async fn handle_client_text_message(
    text: &str,
    state: &AppState,
    last_known_id: &mut i64,
    outbox_tx: &tokio::sync::mpsc::Sender<Box<str>>,
) {
    let msg: ClientWsMessage = match serde_json::from_str(text) {
        Ok(msg) => msg,
        Err(e) => {
            outbox_try_send(
                outbox_tx,
                &json!({
                    "type": "error",
                    "message": format!("invalid client message: {e}"),
                }),
            );
            return;
        }
    };

    match msg.msg_type.as_str() {
        "subscribe" => {
            handle_client_subscribe(msg.since_id, state, last_known_id, outbox_tx).await;
        }
        "ping" => {
            let now_str = chrono::Utc::now().to_rfc3339();
            outbox_try_send(
                outbox_tx,
                &json!({ "type": "pong", "server_time": now_str }),
            );
        }
        _ => {
            outbox_try_send(
                outbox_tx,
                &json!({
                    "type": "error",
                    "message": format!("unknown message type: {}", msg.msg_type),
                }),
            );
        }
    }
}

async fn handle_incoming_ws_message(
    incoming: Option<Result<Message, axum::Error>>,
    state: &AppState,
    last_known_id: &mut i64,
    outbox_tx: &tokio::sync::mpsc::Sender<Box<str>>,
) -> bool {
    match incoming {
        Some(Ok(Message::Text(text))) => {
            handle_client_text_message(&text, state, last_known_id, outbox_tx).await;
            true
        }
        Some(Ok(Message::Close(_))) | None => false,
        Some(Ok(_)) => true,
        Some(Err(e)) => {
            tracing::debug!(error = %e, "stream_usage_rows: ws_receiver error");
            false
        }
    }
}

async fn fetch_initial_history_snapshot(state: &AppState) -> (i64, serde_json::Value) {
    let pool = std::sync::Arc::clone(state.db_pool());
    let rows = tokio::task::spawn_blocking(move || {
        let r = pool.try_reader_for(std::time::Duration::from_secs(5));
        let Some(r) = r else {
            tracing::error!("stream_usage_rows: initial history reader lock timeout");
            return Vec::new();
        };
        match core_usage::recent_desc(&r, 100) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "stream_usage_rows: initial history query failed, \
                     sending empty history and continuing with live events"
                );
                Vec::new()
            }
        }
    })
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "stream_usage_rows: initial history spawn_blocking failed");
        Vec::new()
    });
    let last_known_id = rows.iter().map(|r| r.id.0).max().unwrap_or(0);
    let active_attempts = openproxy_core::usage::get_active_inflight_attempts();
    let server_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let snapshot = json!({
        "type": "snapshot",
        "cursor": 0,
        "server_now": server_now,
        "rows": rows.into_iter().map(openproxy_types::usage::redact_for_broadcast).collect::<Vec<_>>(),
        "attempts": active_attempts,
    });
    (last_known_id, snapshot)
}

fn spawn_ws_sender_task(
    mut ws_sender: futures::stream::SplitSink<WebSocket, Message>,
    mut outbox_rx: tokio::sync::mpsc::Receiver<Box<str>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        use futures::SinkExt;
        while let Some(text) = outbox_rx.recv().await {
            if let Err(e) = ws_sender
                .send(Message::Text(text.into_string().into()))
                .await
            {
                tracing::debug!(error = %e, "stream_usage_rows: ws_sender.send failed, exiting sender task");
                return;
            }
        }
        let _ = ws_sender.send(Message::Close(None)).await;
        let _ = ws_sender.close().await;
    })
}

async fn run_ws_usage_event_loop(
    state: &AppState,
    mut ws_receiver: futures::stream::SplitStream<WebSocket>,
    outbox_tx: tokio::sync::mpsc::Sender<Box<str>>,
    mut last_known_id: i64,
) {
    let mut usage_rx = state.usage_tx().subscribe();
    let mut stage_rx = state.stage_tx().subscribe();
    let mut models_rx = state.models_refreshed_tx().subscribe();
    let mut notification_rx =
        openproxy_core::notifications::try_get_tx().map(tokio::sync::broadcast::Sender::subscribe);

    loop {
        tokio::select! {
            biased;
            stage = stage_rx.recv() => {
                if !handle_stage_event(stage, &outbox_tx).await {
                    break;
                }
            }
            models = models_rx.recv() => {
                match models {
                    Ok(m) => {
                        outbox_send(&outbox_tx, json!({ "type": "models_refreshed", "data": m })).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            usage = usage_rx.recv() => {
                if !handle_usage_event(usage, &mut last_known_id, &outbox_tx).await {
                    break;
                }
            }
            evt = recv_next_notification(&mut notification_rx) => {
                handle_notification_event(evt, &mut notification_rx, &outbox_tx).await;
            }
            incoming = ws_receiver.next() => {
                if !handle_incoming_ws_message(incoming, state, &mut last_known_id, &outbox_tx).await {
                    break;
                }
            }
        }
    }
}

pub async fn stream_usage_rows(socket: WebSocket, state: AppState) {
    let (ws_sender, ws_receiver) = socket.split();
    let (outbox_tx, outbox_rx) = tokio::sync::mpsc::channel::<Box<str>>(WS_OUTBOX_CAPACITY);
    let sender_task = spawn_ws_sender_task(ws_sender, outbox_rx);

    let (last_known_id, snapshot) = fetch_initial_history_snapshot(&state).await;
    outbox_send(&outbox_tx, snapshot).await;

    run_ws_usage_event_loop(&state, ws_receiver, outbox_tx, last_known_id).await;
    let _ = sender_task.await;
}
