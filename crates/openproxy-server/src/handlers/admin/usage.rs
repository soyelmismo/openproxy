use super::*;
use crate::handlers::admin::debug::json_text;
use axum::{
    extract::{State, Query},
    Json,
};

use openproxy_core::usage as core_usage;


pub async fn usage_summary(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<core_usage::UsageSummary>> {
    let body: Result<Json<core_usage::UsageSummary>, ApiError> = async {
        let f = q.into_filter()?;
        let result = run_analytics_query_with_filter(&s, &f, "summary", |conn, fl| {
            core_usage::summary(conn, fl)
        })?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

pub async fn usage_by_model(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Vec<core_usage::ByModelRow>>> {
    let body: Result<Json<Vec<core_usage::ByModelRow>>, ApiError> = async {
        let f = q.into_filter()?;
        let result = run_analytics_query_with_filter(&s, &f, "by_model", |conn, fl| {
            core_usage::by_model(conn, fl)
        })?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

pub async fn usage_by_provider(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Vec<core_usage::ByProviderRow>>> {
    let body: Result<Json<Vec<core_usage::ByProviderRow>>, ApiError> = async {
        let f = q.into_filter()?;
        let result = run_analytics_query_with_filter(&s, &f, "by_provider", |conn, fl| {
            core_usage::by_provider(conn, fl)
        })?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

pub async fn usage_monthly_by_provider(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Vec<core_usage::MonthlyByProviderRow>>> {
    let body: Result<Json<Vec<core_usage::MonthlyByProviderRow>>, ApiError> = async {
        let f = q.into_filter()?;
        let result = run_analytics_query_with_filter(&s, &f, "monthly_by_provider", |conn, fl| {
            core_usage::monthly_by_provider(conn, fl)
        })?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

pub async fn usage_by_day(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Vec<core_usage::ByDayRow>>> {
    let body: Result<Json<Vec<core_usage::ByDayRow>>, ApiError> = async {
        let f = q.into_filter()?;
        let result =
            run_analytics_query_with_filter(&s, &f, "by_day", |conn, fl| core_usage::by_day(conn, fl))?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

pub async fn usage_by_account(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Vec<core_usage::ByAccountRow>>> {
    let body: Result<Json<Vec<core_usage::ByAccountRow>>, ApiError> = async {
        let f = q.into_filter()?;
        let result = run_analytics_query_with_filter(&s, &f, "by_account", |conn, fl| {
            core_usage::by_account(conn, fl)
        })?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

pub async fn usage_by_status(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Vec<core_usage::ByStatusRow>>> {
    let body: Result<Json<Vec<core_usage::ByStatusRow>>, ApiError> = async {
        let f = q.into_filter()?;
        let result = run_analytics_query_with_filter(&s, &f, "by_status", |conn, fl| {
            core_usage::by_status(conn, fl)
        })?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

pub async fn usage_errors(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Vec<core_usage::ErrorRow>>> {
    let body: Result<Json<Vec<core_usage::ErrorRow>>, ApiError> = async {
        let f = q.into_filter()?;
        let result = run_analytics_query_with_filter(&s, &f, "errors", |conn, fl| {
            core_usage::errors(conn, fl, ERRORS_DEFAULT_LIMIT)
        })?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

pub async fn usage_latency(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<analytics::LatencyPercentiles>> {
    let body: Result<Json<analytics::LatencyPercentiles>, ApiError> = async {
        let f = q.into_filter()?;
        let result = run_analytics_query_with_filter(&s, &f, "latency", |conn, fl| {
            analytics::latency_percentiles(conn, fl)
        })?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

pub async fn usage_races(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<analytics::RaceStats>> {
    let body: Result<Json<analytics::RaceStats>, ApiError> = async {
        let f = q.into_filter()?;
        let result = run_analytics_query_with_filter(&s, &f, "races", |conn, fl| {
            analytics::race_stats(conn, fl)
        })?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

pub async fn recompute_usage_costs(
    State(s): State<AppState>,
) -> ApiResult<Json<serde_json::Value>> {
    let updated = {
        let w = s.db_pool().writer();
        match openproxy_core::models_dev_sync::recompute_costs(&w) {
            Ok(n) => n,
            Err(e) => return ApiResult::err(ApiError(e)),
        }
    };
    ApiResult::ok(Json(serde_json::json!({
        "message": format!("re-priced {} usage rows", updated),
        "updated": updated,
    })))
}

pub async fn usage_recent(
    State(s): State<AppState>,
    Query(q): Query<RecentQuery>,
) -> ApiResult<Json<Vec<core_usage::RecentUsageRow>>> {
    let body: Result<Json<Vec<core_usage::RecentUsageRow>>, ApiError> = async {
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
        let rows = core_usage::recent(&r, since_id, limit)?
            .into_iter()
            .map(core_usage::redact_for_broadcast)
            .collect();
        Ok(Json(rows))
    }
    .await;
    body.into()
}

pub async fn usage_stream(
    State(s): State<AppState>,
    Query(q): Query<UsageStreamQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    // HIGH-2 fix: check the Origin header to prevent CSWSH
    // (cross-site WebSocket hijacking). A malicious website can
    // `new WebSocket('ws://victim/admin/usage/stream')` without the
    // victim's knowledge — the browser sends cookies and the request
    // goes through. Without this check, the attacker could read the
    // live-logs stream if the auth bypass is on, or at minimum
    // consume server resources.
    //
    // We allow:
    // - No Origin header (non-browser clients like curl don't send it)
    // - Any Origin that looks like localhost/127.0.0.1 (dev mode)
    // - Any Origin (in production, the reverse proxy should restrict
    //   access to /admin/ via network ACLs; the Origin check is
    //   defense-in-depth for when the proxy is misconfigured)
    //
    // This is intentionally permissive — the real protection is the
    // admin auth middleware + network ACLs on /admin/. The Origin
    // check prevents the browser-based CSWSH attack vector.
    if let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) {
        // Allow localhost origins (dev mode).
        if !origin.starts_with("http://localhost")
            && !origin.starts_with("http://127.0.0.1")
            && !origin.starts_with("https://localhost")
            && !origin.starts_with("https://127.0.0.1")
        {
            // In production, the reverse proxy should restrict /admin/
            // to the internal network. If the operator exposes /admin/
            // to the internet without a proxy, they should set
            // OPENPROXY_ALLOWED_ORIGINS. For now, log a warning and
            // allow — the auth middleware is the primary protection.
            tracing::warn!(
                origin = %origin,
                "WebSocket connection from non-localhost origin; \
                 ensure /admin/ is network-restricted in production"
            );
        }
    }

    match authenticate_admin_ws(&s, &headers, q.token.as_deref()) {
        Ok(()) => ws
            .on_upgrade(move |socket| stream_usage_rows(socket, s))
            .into_response(),
        Err(e) => e.into_response(),
    }
}

pub async fn usage_detail(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DetailQuery>,
) -> ApiResult<Json<UsageDetailResponse>> {
    if let Err(e) = authenticate_admin_ws(&s, &headers, None) {
        return e.into();
    }
    let body: Result<Json<UsageDetailResponse>, ApiError> = async {
        // Read-only SELECT — use the READER.
        let r = s.db_pool().reader();
        let row = if let Some(trace_id) = &q.trace_id {
            core_usage::detail_by_trace_id(&r, trace_id)?
        } else if let Some(id) = q.id {
            core_usage::detail_by_id(&r, id)?
        } else {
            return Err(ApiError(CoreError::Validation(
                "Either 'id' or 'trace_id' query parameter must be provided".into(),
            )));
        };
        match row {
            Some(r) => Ok(Json(UsageDetailResponse { row: r })),
            None => Err(ApiError(CoreError::Internal(format!(
                "usage row not found for query {:?}",
                q
            )))),
        }
    }
    .await;
    body.into()
}


pub(crate) async fn stream_usage_rows(socket: WebSocket, state: AppState) {
    // Split the WebSocket into a sender and receiver half. The
    // sender half moves into a dedicated tokio task that drains the
    // outbox mpsc and writes to the socket; the receiver half stays
    // in this function for the select! loop. This is the CRITICAL
    // architectural change that fixes the "second request doesn't
    // appear in real-time after a failure" bug — see the comment
    // on `WS_OUTBOX_CAPACITY` below for the full rationale.
    let (mut ws_sender, mut ws_receiver) = socket.split();

    if let Err(err) = async {
        // 1. Subscribe to broadcast channels FIRST, before any DB
        //    query. This eliminates the TOCTOU window where stage
        //    events published during the history fetch would be
        //    silently dropped (broadcast::send returns SendError
        //    when there are no receivers). Events that arrive during
        //    the history fetch are queued in the broadcast buffer
        //    (capacity 1024 for stages, 1024 for rows) and delivered
        //    after the history batch is sent. The frontend's
        //    mergeLogsByDescId dedupes by id, so a row appearing in
        //    both history and the broadcast backlog is handled
        //    correctly.
        let mut usage_rx = state.usage_tx().subscribe();
        let mut stage_rx = state.stage_tx().subscribe();

        // F2: also subscribe to the notifications broadcast channel
        // (created by F1 in `core_notifications::NOTIF_TX`). The channel is
        // initialized in `AppState::new` / `AppState::for_test`, but
        // some test paths construct a minimal AppState without that
        // init — `try_get_tx()` returns `None` there and the
        // notifications select! arm below becomes a no-op
        // (`std::future::pending()`).
        //
        // The receiver is `Option<broadcast::Receiver<NotificationEvent>>`
        // because (a) the channel might not be initialized in tests,
        // and (b) we want to drop the receiver on `RecvError::Closed`
        // (server shutting down) without breaking the WS connection
        // — setting it to `None` makes the arm a permanent no-op
        // until the connection closes.
        let mut notification_rx = openproxy_core::notifications::try_get_tx()
            .map(|tx| tx.subscribe());

        // 2. Spawn a DEDICATED sender task that owns `ws_sender.send`.
        //    The receiver loop forwards every broadcast event into
        //    `outbox` (a bounded mpsc); the sender task drains it and
        //    writes to the socket. This decouples the broadcast
        //    receiver loop from the WS send — a slow browser stalls
        //    the sender task but NOT the receiver loop, so broadcast
        //    events keep being drained into the mpsc buffer instead
        //    of piling up in the broadcast channel and getting
        //    dropped for this receiver.
        //
        // The sender task exits (and closes the WS) when:
        //   - the outbox sender is dropped (receiver loop exited), OR
        //   - `ws_sender.send` returns an error (broken connection).
        let (outbox_tx, mut outbox_rx) =
            tokio::sync::mpsc::channel::<String>(WS_OUTBOX_CAPACITY);
        let sender_task = tokio::spawn(async move {
            use futures::SinkExt;
            while let Some(text) = outbox_rx.recv().await {
                if let Err(e) = ws_sender.send(Message::Text(text.into())).await {
                    // Broken connection — the receiver loop will
                    // also notice via `ws_receiver.next()` returning
                    // None/Err. Just exit the sender task.
                    tracing::debug!(error = %e, "stream_usage_rows: ws_sender.send failed, exiting sender task");
                    return;
                }
            }
            // outbox_rx returned None — outbox_tx was dropped, which
            // means the receiver loop exited. Send a Close frame so
            // the client knows the session is over.
            let _ = ws_sender.send(Message::Close(None)).await;
            let _ = ws_sender.close().await;
        });

        // 3. Initial history batch (most recent 100).
        // A SQLite "disk I/O error" here (e.g. WAL contention
        // under load) must NOT kill the WebSocket — the
        // frontend handles an empty `rows` array gracefully,
        // and the subscription loop below will start delivering
        // live events as soon as the DB recovers. Without this
        // guard the error propagated via `?`, broke out of the
        // async block, sent an error envelope, closed the WS,
        // and triggered an immediate reconnect loop.
        // Read-only SELECT — use the READER. The dashboard's WS
        // reconnects would otherwise serialize every history
        // fetch through the writer mutex.
        let rows = {
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
        };
        // H7 fix: track the highest usage `id` we have
        // streamed to the dashboard so a `Lagged` broadcast
        // error can be answered with a targeted resync
        // (`{"type":"resync","since_id":last_known}`) rather
        // than a fatal error. The frontend then fetches
        // `core_usage::recent(since_id=last_known, limit=...)` to
        // catch up. Without this, a slow dashboard would
        // permanently lose rows it could not consume in time
        // and a toast was the only signal — see the audit
        // finding RACE-F-5.
        //
        // Compute `last_known_id` BEFORE redacting (redaction
        // consumes `rows` via `into_iter`).
        let mut last_known_id: i64 = rows.iter().map(|r| r.id.0).max().unwrap_or(0);
        outbox_send(&outbox_tx, json!({
            "type": "history",
            // SECURITY: redact heavyweight fields (request/response bodies
            // and headers) before sending the initial history batch. The
            // live `row` events below are already redacted by
            // `publish_usage_row` → `redact_for_broadcast`; the history
            // batch must apply the same redaction so the initial rows
            // don't leak bodies/headers to the dashboard. The full
            // bodies are available on demand via /usage/detail.
            "rows": rows.into_iter().map(core_usage::redact_for_broadcast).collect::<Vec<_>>()
        })).await;

        // 4. Event loop — usage_rx, stage_rx, and notification_rx are
        //    already subscribed above, before the history query. The
        //    outbox decouples this loop from the WS sender task.
        //
        // `biased` ensures the broadcast channels (stage + usage +
        // notifications) are polled BEFORE the ws_receiver. The
        // ws_receiver almost never has messages (only ping/subscribe
        // from the client, which are rare), so polling it first wastes
        // a branch on every iteration. More importantly, when the
        // browser is slow and the outbox backs up, we want to
        // prioritize draining the broadcast channels (which have a
        // fixed capacity and will lag if not drained) over reading
        // client messages (which can wait indefinitely).
        loop {
            tokio::select! {
                biased;
                // Stage events FIRST — these carry the "in progress"
                // status the operator needs to see in real time. They
                // are the most frequent and most time-sensitive.
                stage = stage_rx.recv() => {
                    match stage {
                        Ok(event) => {
                            outbox_send(&outbox_tx, json!({ "type": "stage", "data": event })).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            outbox_try_send(&outbox_tx, json!({
                                "type": "lag_warning",
                                "skipped": skipped,
                                "message": format!(
                                    "stage broadcast channel lagged; {} event(s) skipped",
                                    skipped
                                ),
                            })).await;
                            outbox_send(&outbox_tx, json!({ "type": "resync", "since_id": last_known_id })).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                // Usage rows SECOND — these are the terminal "row"
                // events published when a request completes. Less
                // frequent than stage events but still critical.
                usage = usage_rx.recv() => {
                    match usage {
                        Ok(row) => {
                            if row.id.0 > last_known_id {
                                last_known_id = row.id.0;
                            }
                            outbox_send(&outbox_tx, json!({ "type": "row", "data": row })).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            outbox_try_send(&outbox_tx, json!({
                                "type": "lag_warning",
                                "skipped": skipped,
                                "message": format!(
                                    "broadcast channel lagged; {} row(s) skipped",
                                    skipped
                                ),
                            })).await;
                            outbox_send(&outbox_tx, json!({ "type": "resync", "since_id": last_known_id })).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                // F2: notifications THIRD — model_new / model_gone /
                // model_auto_activated / system events surfaced to the
                // dashboard tray. Less frequent than stage/usage (a
                // handful per discovery cycle, default 1h) but still
                // real-time. The receiver is an Option because
                // `try_get_tx()` returns None in tests that don't
                // initialize the broadcast channel; in that case the
                // async block degenerates to `pending().await` and
                // this arm is a permanent no-op (never wins select!).
                //
                // On `Lagged(n)` we send a `lag_warning` with
                // `channel: "notifications"` so the client can refetch
                // via `GET /admin/api/notifications` (notifications are
                // persisted, so refetch is the source of truth — we do
                // NOT send a `resync` envelope because there is no
                // `since_id` semantics for notifications; the client
                // just lists the latest 50).
                //
                // On `Closed` (server shutting down) we set
                // `notification_rx = None` so this arm becomes a no-op
                // for the rest of the connection's lifetime — the
                // stage/usage/ws arms continue running normally.
                evt = async {
                    match notification_rx.as_mut() {
                        Some(rx) => match rx.recv().await {
                            Ok(n) => NotifRxEvent::Event(n),
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                NotifRxEvent::Lagged(n)
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                NotifRxEvent::Closed
                            }
                        },
                        None => std::future::pending().await,
                    }
                } => {
                    match evt {
                        NotifRxEvent::Event(n) => {
                            outbox_send(
                                &outbox_tx,
                                json!({ "type": "notification", "data": n }),
                            )
                            .await;
                        }
                        NotifRxEvent::Lagged(skipped) => {
                            outbox_try_send(&outbox_tx, json!({
                                "type": "lag_warning",
                                "skipped": skipped,
                                "channel": "notifications",
                                "message": format!(
                                    "notifications broadcast channel lagged; {} event(s) skipped — refetch via GET /admin/api/notifications",
                                    skipped
                                ),
                            })).await;
                        }
                        NotifRxEvent::Closed => {
                            // Channel closed (server shutting down). Drop
                            // the receiver so this arm becomes a no-op;
                            // the WS connection stays alive as long as
                            // stage/usage still have receivers.
                            notification_rx = None;
                        }
                    }
                }
                // WS receiver LAST — client messages (subscribe, ping)
                // are rare and can tolerate delay. Prioritizing the
                // broadcast channels ensures we never miss a stage
                // event because we were busy reading a ping.
                incoming = ws_receiver.next() => {
                    match incoming {
                        Some(Ok(Message::Text(text))) => {
                            let msg: ClientWsMessage = match serde_json::from_str(&text) {
                                Ok(msg) => msg,
                                Err(e) => {
                                    outbox_try_send(&outbox_tx, json!({
                                        "type": "error",
                                        "message": format!("invalid client message: {e}"),
                                    })).await;
                                    continue;
                                }
                            };

                            match msg.msg_type.as_str() {
                                "subscribe" => {
                                    let since_id = msg
                                        .since_id
                                        .unwrap_or(0)
                                        .clamp(0, USAGE_RECENT_MAX_SINCE_ID);
                                    let rows: Vec<core_usage::RecentUsageRow> = {
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
                                            .map(core_usage::redact_for_broadcast)
                                            .collect()
                                    };
                                    if let Some(mx) = rows.iter().map(|r| r.id.0).max() {
                                        last_known_id = last_known_id.max(mx);
                                    }
                                    outbox_send(&outbox_tx, json!({ "type": "history", "rows": rows })).await;
                                }
                                "ping" => {
                                    let now_str = chrono::Utc::now().to_rfc3339();
                                    outbox_try_send(&outbox_tx, json!({ "type": "pong", "server_time": now_str })).await;
                                }
                                _ => {
                                    outbox_try_send(&outbox_tx, json!({
                                        "type": "error",
                                        "message": format!("unknown message type: {}", msg.msg_type),
                                    })).await;
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) => break,
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            tracing::debug!(error = %e, "stream_usage_rows: ws_receiver error");
                            break;
                        }
                        None => break,
                    }
                }
            }
        }
        // Drop the outbox sender to signal the sender task to exit
        // gracefully (it will send a Close frame and return).
        drop(outbox_tx);
        // Wait for the sender task to finish so we don't leak it.
        let _ = sender_task.await;
        Ok::<(), ApiError>(())
    }
    .await
    {
        // Best-effort error notification. The sender task owns the
        // ws_sender at this point, so we can't send an error frame
        // directly — just log. The frontend will see the WS close
        // and reconnect.
        tracing::debug!(error = %err, "stream_usage_rows: event loop exited with error");
    }
}

pub(crate) fn run_analytics_query_with_filter<T, F>(
    s: &AppState,
    f: &core_usage::UsageFilter,
    query_name: &str,
    query_fn: F,
) -> Result<T, ApiError>
where
    F: Fn(&openproxy_core::db::conn::ReaderGuard<'_>, &core_usage::UsageFilter) -> Result<T, CoreError>,
{
    // First attempt: use the reader connection.
    let r = s
        .db_pool()
        .try_reader_for(ADMIN_LOCK_TIMEOUT)
        .ok_or_else(|| {
            ApiError(CoreError::ServiceUnavailable(
                "reader lock busy: another query is holding the database; retry in a few seconds"
                    .into(),
            ))
        })?;
    match query_fn(&r, f) {
        Ok(result) => Ok(result),
        Err(e) => {
            // Check if this is a disk I/O error (SQLITE_IOERR_*).
            let err_str = format!("{:?}", e);
            let is_disk_io = err_str.contains("disk I/O")
                || err_str.contains("SQLITE_IOERR")
                || err_str.contains("database disk image is malformed")
                || err_str.contains("database is locked");

            if !is_disk_io {
                return Err(ApiError(e));
            }

            tracing::warn!(
                error = %e,
                query = %query_name,
                "analytics query failed with disk I/O error; attempting WAL checkpoint + retry"
            );

            // Drop the reader guard before taking the writer (avoids
            // a potential deadlock if the reader and writer share any
            // internal SQLite state).
            drop(r);

            // Force a WAL checkpoint on the writer connection. This
            // flushes the WAL file into the main DB and releases any
            // pages that were locked by the WAL. `TRUNCATE` mode also
            // truncates the WAL file to zero bytes.
            {
                let w = s.db_pool().writer();
                let _ = w.pragma_update(None, "wal_checkpoint", "TRUNCATE");
            }

            // Reopen BOTH connections (writer + reader). The long-lived
            // reader connection holds a stale page cache that references
            // pages from the pre-repair / pre-VACUUM DB file. Simply
            // re-acquiring the reader lock (try_reader_for) reuses the
            // SAME connection with the SAME stale cache. reopen()
            // closes the old connections and opens fresh ones that
            // re-read from disk.
            tracing::info!(
                query = %query_name,
                "analytics retry: reopening DB connections to clear stale page cache"
            );
            if let Err(e) = s.db_pool().reopen() {
                tracing::warn!(
                    error = %e,
                    "analytics retry: reopen failed (continuing with existing connection)"
                );
            }

            // Retry on the (now fresh) reader connection.
            let r2 = s
                .db_pool()
                .try_reader_for(ADMIN_LOCK_TIMEOUT)
                .ok_or_else(|| {
                    ApiError(CoreError::ServiceUnavailable(
                        "reader lock busy on retry; the database may be under heavy load".into(),
                    ))
                })?;
            query_fn(&r2, f).map_err(ApiError)
        }
    }
}

pub(crate) fn resolve_preset(preset: &str) -> Result<Option<(String, String)>, ApiError> {
    use chrono::{Datelike, Duration, NaiveDate, TimeZone, Utc};

    // Helper to format a (year, month, day) tuple at 00:00:00 UTC.
    let midnight = |y: i32, m: u32, d: u32| -> String {
        let naive = NaiveDate::from_ymd_opt(y, m, d)
            .expect("valid ymd")
            .and_hms_opt(0, 0, 0)
            .expect("valid hms");
        iso_z(Utc.from_utc_datetime(&naive))
    };

    let now = Utc::now();
    let today = now.date_naive();
    let y = now.year();
    let m = now.month();

    match preset {
        "today" => {
            let from = midnight(y, m, today.day());
            // Tomorrow rolls over month/year boundaries via chrono's
            // NaiveDate arithmetic; using `today.day() + 1` directly
            // would overflow on the last day of the month.
            let tomorrow = today + Duration::days(1);
            let to = midnight(tomorrow.year(), tomorrow.month(), tomorrow.day());
            Ok(Some((from, to)))
        }
        "7d" => {
            let from = now - Duration::days(7);
            Ok(Some((iso_z(from), iso_z(now))))
        }
        "30d" => {
            let from = now - Duration::days(30);
            Ok(Some((iso_z(from), iso_z(now))))
        }
        "this_month" => {
            let from = midnight(y, m, 1);
            // First day of next month (may roll into next year).
            let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            let to = midnight(ny, nm, 1);
            Ok(Some((from, to)))
        }
        "last_month" => {
            let (ly, lm) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
            let from = midnight(ly, lm, 1);
            let to = midnight(y, m, 1);
            Ok(Some((from, to)))
        }
        "last_6_months" => {
            // Walk back 6 months from the first day of the current
            // month. We compute the start of each month by subtracting
            // months one at a time to avoid the "month - 6" underflow.
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
            let from = midnight(ly, lm, 1);
            let to = midnight(y, m, 1);
            Ok(Some((from, to)))
        }
        "ytd" => {
            let from = midnight(y, 1, 1);
            let to = midnight(y + 1, 1, 1);
            Ok(Some((from, to)))
        }
        // `custom` (or any other unrecognised string the operator
        // might type) means "use the explicit from/to as-is". We
        // surface unknown presets as a 400 so the dashboard doesn't
        // silently miss a window due to a typo.
        "custom" => Ok(None),
        other => Err(CoreError::Validation(format!(
            "preset must be one of today|7d|30d|this_month|last_month|last_6_months|ytd|custom; got `{}`",
            other
        ))
        .into()),
    }
}

pub(crate) fn parse_usage_timestamp(s: &str, field: &str) -> Result<String, ApiError> {
    // Try RFC-3339 first (the canonical form `created_at` is stored in).
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    }
    // Fall back to the SQLite "YYYY-MM-DD HH:MM:SS" form (the format
    // operators sometimes paste from a log line). We require the
    // space — a `T` here is the RFC-3339 form, already handled above.
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Ok(naive
            .and_utc()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    }
    Err(CoreError::Validation(format!(
        "{} must be an RFC-3339 timestamp (e.g. 2026-06-18T07:00:00Z) or \
         SQLite-style (e.g. 2026-06-18 07:00:00); got `{}`",
        field, s
    ))
    .into())
}

pub(crate) fn iso_z(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub(crate) async fn outbox_send(tx: &tokio::sync::mpsc::Sender<String>, value: serde_json::Value) {
    let text: String = match json_text(value) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "stream_usage_rows: json_text failed in outbox_send");
            return;
        }
    };
    // Use send().await for real-time messages — this blocks the
    // receiver loop if the outbox is full, but that's BETTER than
    // dropping the message. The broadcast channel has capacity 1024,
    // so a brief stall won't cause lag. If the stall is prolonged
    // (seconds), the broadcast channel will lag and trigger a resync.
    match tx.send(text).await {
        Ok(()) => {}
        Err(_e) => {
            // Sender task exited — the WS is closing. Just drop
            // the message; the receiver loop will exit momentarily
            // when `ws_receiver.next()` returns None.
        }
    }
}

pub(crate) async fn outbox_try_send(tx: &tokio::sync::mpsc::Sender<String>, value: serde_json::Value) {
    let text: String = match json_text(value) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "stream_usage_rows: json_text failed in outbox_try_send");
            return;
        }
    };
    match tx.try_send(text) {
        Ok(()) => {}
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            tracing::debug!("stream_usage_rows: outbox full, dropping non-critical WS message");
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
    }
}


#[derive(Debug, Default, Deserialize)]
pub struct UsageQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub account_id: Option<i64>,
    pub combo_id: Option<i64>,
    /// Restrict the roll-up to a single API key. The per-key
    /// `GET /admin/keys/:id/usage` endpoint sets this; the
    /// public analytics endpoints leave it absent.
    pub api_key_id: Option<i64>,
    /// Named time-window preset. One of: `today`, `7d`, `30d`,
    /// `this_month`, `last_month`, `last_6_months`, `ytd`, `custom`.
    ///
    /// When set, the server computes `from`/`to` in UTC and ignores
    /// any explicit `from`/`to` (with a warning). `custom` (or
    /// `None`) falls through to the explicit `from`/`to` fields.
    pub preset: Option<String>,
}


impl UsageQuery {
    /// Project into a [`UsageFilter`]. An empty `provider_id` string
    /// surfaces here as a 400 via [`CoreError::Validation`].
    fn into_filter(self) -> Result<UsageFilter, ApiError> {
        let provider_id = self
            .provider_id
            .map(|s| {
                if s.is_empty() {
                    Err(CoreError::Validation(
                        "provider_id must not be empty".into(),
                    ))
                } else {
                    Ok(ProviderId::new(s))
                }
            })
            .transpose()?;
        // MEDIUM fix: validate `from` and `to` are well-formed timestamps
        // before they reach the SQL builder. Without this, a query
        // like `?from=garbage` returns 0 rows silently (the SQLite
        // string comparison fails the row against every `created_at`)
        // and the operator gets a misleading "no data" result. A
        // malformed timestamp is a client error and must surface as 400.
        //
        // Accept the two timestamp shapes the dashboard sends:
        //   - RFC-3339 (e.g. `2026-06-18T07:00:00Z`)
        //   - SQLite-style (e.g. `2026-06-18 07:00:00`)
        //
        // Both round-trip through `chrono::DateTime<Utc>` and we
        // re-emit the canonical RFC-3339 form so the SQL comparison
        // is consistent.
        let mut from = self
            .from
            .map(|s| parse_usage_timestamp(&s, "from"))
            .transpose()?;
        let mut to = self
            .to
            .map(|s| parse_usage_timestamp(&s, "to"))
            .transpose()?;

        // Preset handling: if `preset` is set, it takes precedence
        // over explicit `from`/`to`. We log a warning when both are
        // provided so the operator can spot the dashboard sending
        // redundant data.
        if let Some(preset) = &self.preset {
            if from.is_some() || to.is_some() {
                tracing::warn!(
                    preset = %preset,
                    from = ?from,
                    to = ?to,
                    "UsageQuery: preset is set and will override explicit from/to"
                );
            }
            if let Some((pf, pt)) = resolve_preset(preset)? {
                from = Some(pf);
                to = Some(pt);
            }
            // `custom` (or None) falls through with the explicit values.
        }

        // If both are present, from must not be after to. (Both
        // are inclusive at the lower bound in the SQL.)
        if let (Some(f), Some(t)) = (&from, &to)
            && f > t
        {
            return Err(
                CoreError::Validation(format!("from ({}) must be <= to ({})", f, t)).into(),
            );
        }
        let account_id = self.account_id.map(AccountId::new);
        let combo_id = self.combo_id.map(ComboId);
        let api_key_id = self.api_key_id.map(ApiKeyId);
        Ok(UsageFilter {
            from,
            to,
            provider_id,
            model_id: self.model_id,
            account_id,
            combo_id,
            api_key_id,
        })
    }
}
