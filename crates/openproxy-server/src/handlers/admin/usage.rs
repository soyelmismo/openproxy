use super::{
    ADMIN_LOCK_TIMEOUT, AccountId, ApiError, ApiKeyId, AppState, ComboId, CoreError, Deserialize,
    HeaderMap, IntoResponse, Message, ProviderId, Serialize, StatusCode, StreamExt, UsageFilter,
    WebSocket, WebSocketUpgrade, analytics, authenticate_admin_ws, json,
};
use crate::handlers::admin::debug::json_text;
use axum::{
    Json,
    extract::{Query, State},
};

use openproxy_core::usage as core_usage;

pub const ERRORS_DEFAULT_LIMIT: u32 = 100;
pub const USAGE_RECENT_DEFAULT_LIMIT: u32 = 50;
pub const USAGE_RECENT_MAX_LIMIT: u32 = 500;
pub const USAGE_RECENT_MAX_SINCE_ID: i64 = i64::MAX / 2;
pub const WS_OUTBOX_CAPACITY: usize = 2048;

#[derive(Debug, Default, Deserialize)]
pub struct RecentQuery {
    pub since_id: Option<i64>,
    pub limit: Option<u32>,
}

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

#[derive(Debug, Default, Deserialize)]
pub struct DetailQuery {
    pub id: Option<i64>,
    pub trace_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct UsageDetailResponse {
    pub row: core_usage::UsageDetailRow,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsageQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub account_id: Option<i64>,
    pub combo_id: Option<i64>,
    pub api_key_id: Option<i64>,
    pub preset: Option<String>,
}

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/summary", axum::routing::get(usage_summary))
        .route("/by-model", axum::routing::get(usage_by_model))
        .route("/by-provider", axum::routing::get(usage_by_provider))
        .route(
            "/monthly-by-provider",
            axum::routing::get(usage_monthly_by_provider),
        )
        .route("/by-day", axum::routing::get(usage_by_day))
        .route("/by-account", axum::routing::get(usage_by_account))
        .route("/by-status", axum::routing::get(usage_by_status))
        .route("/errors", axum::routing::get(usage_errors))
        .route("/latency", axum::routing::get(usage_latency))
        .route("/races", axum::routing::get(usage_races))
        .route("/recent", axum::routing::get(usage_recent))
        .route("/detail", axum::routing::get(usage_detail))
        .route(
            "/recompute-costs",
            axum::routing::post(recompute_usage_costs),
        )
}

macro_rules! analytics_handler {
    ($fn_name:ident, $tag:literal, $core_fn:path, $res_ty:ty) => {
        pub async fn $fn_name(
            State(s): State<AppState>,
            Query(q): Query<UsageQuery>,
        ) -> Result<Json<$res_ty>, ApiError> {
            let f = q.into_filter()?;
            let result =
                run_analytics_query_with_filter(&s, &f, $tag, |conn, fl| $core_fn(conn, fl))?;
            Ok(Json(result))
        }
    };
}

analytics_handler!(
    usage_summary,
    "summary",
    core_usage::summary,
    core_usage::UsageSummary
);
analytics_handler!(
    usage_by_model,
    "by_model",
    core_usage::by_model,
    Vec<core_usage::ByModelRow>
);
analytics_handler!(
    usage_by_provider,
    "by_provider",
    core_usage::by_provider,
    Vec<core_usage::ByProviderRow>
);
analytics_handler!(
    usage_monthly_by_provider,
    "monthly_by_provider",
    core_usage::monthly_by_provider,
    Vec<core_usage::MonthlyByProviderRow>
);
analytics_handler!(
    usage_by_day,
    "by_day",
    core_usage::by_day,
    Vec<core_usage::ByDayRow>
);
analytics_handler!(
    usage_by_account,
    "by_account",
    core_usage::by_account,
    Vec<core_usage::ByAccountRow>
);
analytics_handler!(
    usage_by_status,
    "by_status",
    core_usage::by_status,
    Vec<core_usage::ByStatusRow>
);
analytics_handler!(
    usage_latency,
    "latency",
    analytics::latency_percentiles,
    analytics::LatencyPercentiles
);
analytics_handler!(
    usage_races,
    "races",
    analytics::race_stats,
    analytics::RaceStats
);

pub async fn usage_errors(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<Vec<core_usage::ErrorRow>>, ApiError> {
    let f = q.into_filter()?;
    let result = run_analytics_query_with_filter(&s, &f, "errors", |conn, fl| {
        core_usage::errors(conn, fl, ERRORS_DEFAULT_LIMIT)
    })?;
    Ok(Json(result))
}

pub async fn recompute_usage_costs(
    State(s): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let updated = {
        let w = s.db_pool().writer();
        match openproxy_core::models_dev_sync::recompute_costs(&w) {
            Ok(n) => n,
            Err(e) => return Err(ApiError(e)),
        }
    };
    Ok(Json(serde_json::json!({
        "message": format!("re-priced {} usage rows", updated),
        "updated": updated,
    })))
}

pub async fn usage_recent(
    State(s): State<AppState>,
    Query(q): Query<RecentQuery>,
) -> Result<Json<Vec<openproxy_types::usage::RecentUsageRow>>, ApiError> {
    let since_id = q.since_id.unwrap_or(0).clamp(0, USAGE_RECENT_MAX_SINCE_ID);
    let limit = q
        .limit
        .unwrap_or(USAGE_RECENT_DEFAULT_LIMIT)
        .clamp(1, USAGE_RECENT_MAX_LIMIT);
    // Read-only SELECT — use the READER. The dashboard polls this
    // endpoint frequently; going through the writer would
    // serialize every poll against `cost::record` writes.
    let r = s.db_pool().reader();
    // SEC-MEDIUM-C fix: drop the heavy request/response payloads
    // from the WS/REST surface — they can be multi-MB and would
    // fan out PII to every dashboard subscriber. The detail
    // endpoint reads them straight from the database on demand.
    let rows = if since_id == 0 {
        core_usage::recent_desc(&r, limit)?
    } else {
        core_usage::recent(&r, since_id, limit)?
    };
    let rows: Vec<_> = rows
        .into_iter()
        .map(openproxy_types::usage::redact_for_broadcast)
        .collect();
    Ok(Json(rows))
}

fn is_allowed_origin(origin: &str, host: &str) -> bool {
    let is_same_host = !host.is_empty()
        && (origin == format!("http://{host}") || origin == format!("https://{host}"));
    if is_same_host {
        return true;
    }

    const LOCAL_ORIGIN_PREFIXES: &[&str] = &[
        "http://localhost",
        "http://127.0.0.1",
        "https://localhost",
        "https://127.0.0.1",
    ];

    LOCAL_ORIGIN_PREFIXES
        .iter()
        .any(|prefix| origin == *prefix || origin.starts_with(&format!("{prefix}:")))
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

fn fetch_usage_detail(
    r: &rusqlite::Connection,
    q: &DetailQuery,
) -> Result<Option<core_usage::UsageDetailRow>, ApiError> {
    if let Some(id) = q.id
        && id > 0
    {
        return core_usage::detail_by_id(r, id).map_err(ApiError);
    }
    if let Some(trace_id) = q.trace_id.as_deref() {
        return core_usage::detail_by_trace_id(r, trace_id).map_err(ApiError);
    }
    if let Some(id) = q.id {
        return core_usage::detail_by_id(r, id).map_err(ApiError);
    }
    Err(ApiError(CoreError::Validation(
        "Either 'id' or 'trace_id' query parameter must be provided".into(),
    )))
}

pub async fn usage_detail(
    State(s): State<AppState>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<DetailQuery>,
) -> Result<Json<UsageDetailResponse>, ApiError> {
    authenticate_admin_ws(&s, &headers, None, Some(&addr))?;
    let r = s.db_pool().reader();

    let row = fetch_usage_detail(&r, &q)?;
    let Some(r) = row else {
        return Err(ApiError(CoreError::Internal(format!(
            "usage row not found for query {q:?}"
        ))));
    };

    Ok(Json(UsageDetailResponse { row: r }))
}

async fn outbox_send(tx: &tokio::sync::mpsc::Sender<Box<str>>, val: serde_json::Value) {
    if let Ok(text) = json_text(&val) {
        let _ = tx.send(text.into_boxed_str()).await;
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
    let rows: Vec<openproxy_types::usage::RecentUsageRow> = tokio::task::block_in_place(|| {
        let r = state.db_pool().reader();
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

fn fetch_initial_history_snapshot(state: &AppState) -> (i64, serde_json::Value) {
    let rows = tokio::task::block_in_place(|| {
        let r = state.db_pool().reader();
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
            if let Err(e) = ws_sender.send(Message::Text(text.into_string().into())).await {
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

pub(crate) async fn stream_usage_rows(socket: WebSocket, state: AppState) {
    let (ws_sender, ws_receiver) = socket.split();
    let (outbox_tx, outbox_rx) = tokio::sync::mpsc::channel::<Box<str>>(WS_OUTBOX_CAPACITY);
    let sender_task = spawn_ws_sender_task(ws_sender, outbox_rx);

    let (last_known_id, snapshot) = fetch_initial_history_snapshot(&state);
    outbox_send(&outbox_tx, snapshot).await;

    run_ws_usage_event_loop(&state, ws_receiver, outbox_tx, last_known_id).await;
    let _ = sender_task.await;
}

fn is_disk_io_error(err: &CoreError) -> bool {
    let err_str = format!("{err:?}");
    err_str.contains("disk I/O")
        || err_str.contains("SQLITE_IOERR")
        || err_str.contains("database disk image is malformed")
        || err_str.contains("database is locked")
}

fn recover_and_retry_reader<'a>(
    state: &'a AppState,
    query_name: &str,
) -> Result<openproxy_db::conn::ReaderGuard<'a>, ApiError> {
    {
        let w = state.db_pool().writer();
        let _ = w.pragma_update(None, "wal_checkpoint", "TRUNCATE");
    }
    tracing::info!(
        query = %query_name,
        "analytics retry: reopening DB connections to clear stale page cache"
    );
    if let Err(reopen_err) = state.db_pool().reopen() {
        tracing::warn!(
            error = %reopen_err,
            "analytics retry: reopen failed (continuing with existing connection)"
        );
    }
    state
        .db_pool()
        .try_reader_for(ADMIN_LOCK_TIMEOUT)
        .ok_or_else(|| {
            ApiError(CoreError::ServiceUnavailable(
                "reader lock busy on retry; the database may be under heavy load".into(),
            ))
        })
}

pub(crate) fn run_analytics_query_with_filter<T, F>(
    state: &AppState,
    filter: &core_usage::UsageFilter,
    query_name: &str,
    query_fn: F,
) -> Result<T, ApiError>
where
    F: Fn(&openproxy_db::conn::ReaderGuard<'_>, &core_usage::UsageFilter) -> Result<T, CoreError>,
{
    let reader = state
        .db_pool()
        .try_reader_for(ADMIN_LOCK_TIMEOUT)
        .ok_or_else(|| {
            ApiError(CoreError::ServiceUnavailable(
                "reader lock busy: another query is holding the database; retry in a few seconds"
                    .into(),
            ))
        })?;

    match query_fn(&reader, filter) {
        Ok(result) => Ok(result),
        Err(err) => {
            if !is_disk_io_error(&err) {
                return Err(ApiError(err));
            }

            tracing::warn!(
                error = %err,
                query = %query_name,
                "analytics query failed with disk I/O error; attempting WAL checkpoint + retry"
            );

            drop(reader);
            let reader2 = recover_and_retry_reader(state, query_name)?;
            query_fn(&reader2, filter).map_err(ApiError)
        }
    }
}

fn iso_z(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn midnight_iso(y: i32, m: u32, d: u32) -> String {
    use chrono::{NaiveDate, TimeZone, Utc};
    let naive = NaiveDate::from_ymd_opt(y, m, d)
        .expect("valid ymd")
        .and_hms_opt(0, 0, 0)
        .expect("valid hms");
    iso_z(Utc.from_utc_datetime(&naive))
}

fn compute_calendar_preset(
    preset: &str,
    y: i32,
    m: u32,
    today: chrono::NaiveDate,
) -> Option<(String, String)> {
    use chrono::{Datelike, Duration};
    match preset {
        "today" => {
            let from = midnight_iso(y, m, today.day());
            let tomorrow = today + Duration::days(1);
            let to = midnight_iso(tomorrow.year(), tomorrow.month(), tomorrow.day());
            Some((from, to))
        }
        "this_month" => {
            let from = midnight_iso(y, m, 1);
            let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            let to = midnight_iso(ny, nm, 1);
            Some((from, to))
        }
        "last_month" => {
            let (ly, lm) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
            let from = midnight_iso(ly, lm, 1);
            let to = midnight_iso(y, m, 1);
            Some((from, to))
        }
        "last_6_months" => {
            let mut ly = y;
            let mut lm = m;
            for _ in 0..6 {
                if lm == 1 {
                    lm = 12;
                    ly -= 1;
                } else {
                    lm -= 1;
                }
            }
            let from = midnight_iso(ly, lm, 1);
            let to = midnight_iso(y, m, 1);
            Some((from, to))
        }
        "ytd" => {
            let from = midnight_iso(y, 1, 1);
            let to = midnight_iso(y + 1, 1, 1);
            Some((from, to))
        }
        _ => None,
    }
}

pub(crate) fn resolve_preset(preset: &str) -> Result<Option<(String, String)>, ApiError> {
    use chrono::{Datelike, Duration, Utc};
    let now = Utc::now();

    if let Some(range) = compute_calendar_preset(preset, now.year(), now.month(), now.date_naive())
    {
        return Ok(Some(range));
    }

    match preset {
        "7d" => Ok(Some((iso_z(now - Duration::days(7)), iso_z(now)))),
        "30d" => Ok(Some((iso_z(now - Duration::days(30)), iso_z(now)))),
        "custom" => Ok(None),
        other => Err(CoreError::Validation(format!(
            "preset must be one of today|7d|30d|this_month|last_month|last_6_months|ytd|custom; got `{other}`"
        ))
        .into()),
    }
}

fn parse_provider_filter(provider_id: Option<String>) -> Result<Option<ProviderId>, ApiError> {
    provider_id
        .map(|s| {
            if s.is_empty() {
                Err(CoreError::Validation(
                    "provider_id must not be empty".into(),
                ))
            } else {
                Ok(ProviderId::new(s))
            }
        })
        .transpose()
        .map_err(ApiError)
}

pub(crate) fn parse_usage_timestamp(s: &str, field_name: &str) -> Result<String, ApiError> {
    if let Ok(dt) = openproxy_types::timestamp::parse_timestamp(s) {
        return Ok(iso_z(dt));
    }
    Err(CoreError::Validation(format!(
        "'{field_name}' parameter '{s}' must be an RFC-3339 timestamp (e.g. 2026-06-18T07:00:00Z) or SQLite format (2026-06-18 07:00:00)"
    ))
    .into())
}

fn resolve_query_time_bounds(
    from_raw: Option<String>,
    to_raw: Option<String>,
    preset: Option<&str>,
) -> Result<(Option<String>, Option<String>), ApiError> {
    let mut from = from_raw
        .map(|s| parse_usage_timestamp(&s, "from"))
        .transpose()?;
    let mut to = to_raw
        .map(|s| parse_usage_timestamp(&s, "to"))
        .transpose()?;

    if let Some(p) = preset {
        if from.is_some() || to.is_some() {
            tracing::warn!(
                preset = %p,
                from = ?from,
                to = ?to,
                "UsageQuery: preset is set and will override explicit from/to"
            );
        }
        if let Some((pf, pt)) = resolve_preset(p)? {
            from = Some(pf);
            to = Some(pt);
        }
    }

    if let (Some(f), Some(t)) = (&from, &to)
        && f > t
    {
        return Err(CoreError::Validation(format!("from ({f}) must be <= to ({t})")).into());
    }

    Ok((from, to))
}

impl UsageQuery {
    /// Project into a [`UsageFilter`]. An empty `provider_id` string
    /// surfaces here as a 400 via [`CoreError::Validation`].
    pub(crate) fn into_filter(self) -> Result<UsageFilter, ApiError> {
        let provider_id = parse_provider_filter(self.provider_id)?;
        let (from, to) = resolve_query_time_bounds(self.from, self.to, self.preset.as_deref())?;

        Ok(UsageFilter {
            from,
            to,
            provider_id,
            model_id: self.model_id,
            account_id: self.account_id.map(AccountId::new),
            combo_id: self.combo_id.map(ComboId),
            api_key_id: self.api_key_id.map(ApiKeyId),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_usage_timestamp_rfc3339() {
        let ts = "2026-06-18T07:00:00Z";
        let parsed = parse_usage_timestamp(ts, "from").unwrap();
        assert_eq!(parsed, "2026-06-18T07:00:00Z");
    }

    #[test]
    fn test_parse_usage_timestamp_sqlite_style() {
        let ts = "2026-06-18 07:00:00";
        let parsed = parse_usage_timestamp(ts, "from").unwrap();
        assert_eq!(parsed, "2026-06-18T07:00:00Z");
    }

    #[test]
    fn test_parse_usage_timestamp_invalid() {
        let ts = "not a timestamp";
        let err = parse_usage_timestamp(ts, "from").unwrap_err();
        match err {
            ApiError(CoreError::Validation(msg)) => {
                assert!(msg.contains("must be an RFC-3339 timestamp"));
                assert!(msg.contains("from"));
            }
            _ => panic!("Expected Validation error, got {err:?}"),
        }
    }
}
