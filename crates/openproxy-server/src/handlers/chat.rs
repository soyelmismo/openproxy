//! `POST /v1/chat/completions` — the public entry point.
//!
//! Spec §2.1 describes the contract:
//! 1. Parse the incoming JSON as an [`OpenAIRequest`].
//! 2. Resolve the routing plan from the `model` field via
//!    [`openproxy_core::routing::resolve`]. A model that matches a
//!    row in the `models` table goes direct (via a synthetic
//!    single-target combo); a `combo:<name>` matches a combo; anything
//!    else is 404.
//! 3. The optional `x-openproxy-combo` header is a legacy override
//!    that forces a specific combo, bypassing model resolution.
//! 4. Hand the resolved plan + the parsed request to the
//!    [`Pipeline`] which dispatches it to the configured upstream,
//!    with retries, timeouts, and usage writes.
//! 5. Translate the pipeline's [`PipelineResult`] back into either
//!    an OpenAI-shaped JSON response or a structured error.
//!
//! Streaming (`stream: true` in the request body) is intentionally
//! not wired up: the MVP is non-streaming and the pipeline's SSE
//! plumbing is a follow-up.

use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use futures::stream::Stream;
use openproxy_core::{
    CoreError, api_keys as core_api_keys,
    ids::{ApiKeyId, ComboId, RequestId, TraceId},
    pipeline::{Pipeline, PipelineConfig, PipelineRequest},
    redact::redact_sensitive_headers,
    routing::{self, RoutingPlan, SYNTHETIC_COMBO_ID, build_synthetic_combo},
    translation::OpenAIRequest,
};
use serde_json::json;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use std::time::Instant;
use tokio_stream::wrappers::ReceiverStream;

use crate::{disconnect::CancelWatch, error::ApiError, state::AppState};

/// SSE keepalive interval. Sends `: keep-alive\n\n` (an SSE comment)
/// periodically to keep the connection alive while the upstream is
/// generating. This is critical for streaming requests where the
/// upstream takes a long time to produce the first token (e.g. large
/// prompts, reasoning models). Without frequent keepalives,
/// intermediate proxies (nginx, cloudflare) and client HTTP
/// libraries may close the connection due to inactivity, causing
/// false-positive "client disconnected" errors.
///
/// CRITICAL: the first keepalive is DELAYED by this interval (not
/// sent immediately). The previous code used `tokio::time::interval`
/// which fires on the FIRST tick (immediately), sending `: keep-alive\n\n`
/// as the VERY FIRST bytes of the response body — before any `data: {...}`
/// frame. Some SSE clients (notably the OpenAI Python library's httpx-sse
/// parser) may not handle a leading SSE comment correctly and close the
/// connection. Using `interval_at` with a delayed start ensures the first
/// keepalive only fires after `SSE_KEEPALIVE_INTERVAL` of inactivity,
/// giving the upstream time to send the first real data frame.
const SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// A stream that yields pre-formatted SSE frames (`Bytes`) from an
/// mpsc channel, interleaved with periodic SSE keepalive comments.
/// Unlike `axum::response::Sse`, this writes raw `Bytes` directly to
/// the HTTP body with zero additional wrapping — the pipeline already
/// formats each chunk as `data: {payload}\n\n`.
struct SseBytesStream {
    inner: futures::stream::SelectAll<ReceiverStream<Bytes>>,
    keepalive: tokio::time::Interval,
}

impl Stream for SseBytesStream {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // All fields are `Unpin`, so `get_mut()` is safe.
        let this = self.get_mut();

        // Check keepalive first (biased): if the keepalive timer
        // has elapsed, emit a comment to keep the connection alive
        // without adding data to the stream.
        if this.keepalive.poll_tick(cx).is_ready() {
            return Poll::Ready(Some(Ok(Bytes::from_static(b": keep-alive\n\n"))));
        }

        // Poll the merged channel stream and wrap each item in Ok.
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(chunk)) => Poll::Ready(Some(Ok(chunk))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// `POST /v1/chat/completions`.
///
/// The full body is parsed as an `OpenAIRequest`; on parse failure we
/// return 400 with the standard error envelope. On success we hand
/// the request to the pipeline, which returns a [`PipelineResult`]
/// we translate into a `(status, body)` response.
///
/// The `CancelWatch` extension is injected by the
/// [`crate::disconnect::client_disconnect_middleware`]; it carries a
/// `watch::Receiver<bool>` that flips to `true` the moment the client
/// closes the TCP connection (request-body read error OR
/// response-body write error). We thread it into the pipeline as
/// `PipelineRequest::client_disconnected` so the dispatch loop, the
/// `reqwest::send()` `tokio::select!`, and the SSE `stream.next()`
/// `tokio::select!` all observe the real cancel — no time-based
/// watchdog needed.
/// `POST /v1/chat/completions`.
///
/// The handler creates its own fresh cancel watch (NOT from the
/// middleware — see router.rs for why the middleware was removed).
/// The fresh watch is driven only by the watchdog timer (total_ms).
pub async fn chat_completions(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Result<axum::response::Response, ApiError> {
    // Create a dummy CancelWatch — the middleware is no longer
    // applied to this route, so we create our own fresh watch pair.
    let cancel = crate::disconnect::CancelWatch::new();
    run_pipeline(state, addr, cancel, headers, body).await
}

/// Drive one chat-completion request through the pipeline.
///
/// The body mirrors what a synchronous-style pipeline call would look
/// like: parse → authenticate → resolve routing → build config → run
/// pipeline → shape the response.
///
/// `cancel` is the per-request watch receiver produced by the
/// [`crate::disconnect::client_disconnect_middleware`]. It flips to
/// `true` on a real TCP-level client disconnect; the pipeline's
/// dispatch loop, the `reqwest::send()` `tokio::select!`, and the
/// SSE `stream.next()` `tokio::select!` all observe it.
async fn run_pipeline(
    state: AppState,
    client_addr: std::net::SocketAddr,
    _cancel: CancelWatch,
    headers: HeaderMap,
    body: serde_json::Value,
) -> Result<axum::response::Response, ApiError> {
    // 1. Parse the OpenAI request.
    let raw_request_body = body.clone();
    let openai_req: OpenAIRequest =
        serde_json::from_value(body).map_err(|e| ApiError(CoreError::Parse(e.to_string())))?;

    // 2. Authenticate and rate limit.
    let auth_result = authenticate(&state, &headers, &openai_req.model)?;
    let api_key_id: Option<ApiKeyId> = auth_result.as_ref().map(|r| r.key_id);

    let rl_key = if let Some(id) = api_key_id {
        format!("key:{}", id.0)
    } else {
        format!("ip:{}", client_addr.ip())
    };
    if !state.rate_limiter().check(&rl_key) {
        return Err(ApiError(CoreError::RateLimited {
            provider: "rate_limiter".into(),
            retry_after_ms: 60_000,
        }));
    }

    // 3. Resolve routing.
    let plan = resolve_routing_plan(&state, &headers, &openai_req, &auth_result)?;

    // 4. Translate plan to pipeline targets.
    let (combo_id, combo_override, targets_override) =
        translate_plan_to_targets(&state, &plan, api_key_id)?;

    // 5. Build pipeline.
    let pipeline = build_pipeline(&state);

    // 6. Per-request IDs.
    let request_id = RequestId::new();
    let trace_id = TraceId::new();

    // 7. Watchdog handling.
    let watchdog_budget_ms = calculate_watchdog_budget(&state, &headers);
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let (client_disconnected, watchdog_tx) = create_watchdog_channels(openai_req.stream);
    let stream_sink = if openai_req.stream {
        Some(openproxy_core::race_sink::StreamSink::Direct(tx))
    } else {
        Some(openproxy_core::race_sink::StreamSink::Discard)
    };

    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    spawn_watchdog(done_rx, watchdog_tx, watchdog_budget_ms);

    // 8. Build request and run.
    let req = PipelineRequest {
        request_id,
        trace_id,
        combo_id,
        openai_request: openai_req.clone(),
        client_disconnected,
        stream_sink,
        api_key_id,
        combo_override,
        targets_override,
        request_headers: redact_sensitive_headers(&headers),
        request_body_json: Some(raw_request_body),
        race_cancelled: false,
        race_cancel: None,
        endpoint_kind: openproxy_core::endpoint::EndpointKind::Chat,
    };

    if openai_req.stream {
        return handle_streaming_response(pipeline, req, done_tx, rx).await;
    }

    handle_sync_response(pipeline, req, done_tx).await
}

fn resolve_routing_plan(
    state: &AppState,
    headers: &HeaderMap,
    openai_req: &OpenAIRequest,
    auth_result: &Option<AuthResult>,
) -> Result<RoutingPlan, ApiError> {
    let legacy_combo_name = headers
        .get("x-openproxy-combo")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let plan = {
        let w = state.db_pool().writer();
        if let Some(name) = legacy_combo_name.as_deref() {
            match openproxy_core::combos::get_combo_by_name(&w, name)? {
                Some(combo) => {
                    let targets = openproxy_core::combos::list_targets(&w, combo.id)?;
                    RoutingPlan::Combo {
                        combo_id: combo.id,
                        combo_name: combo.name,
                        strategy: combo.strategy,
                        race_size: combo.race_size,
                        targets,
                    }
                }
                None => return Err(ApiError(CoreError::ComboNotFound(0))),
            }
        } else {
            routing::resolve(&w, &openai_req.model)?
        }
    };

    if let RoutingPlan::Combo { combo_id, .. } = &plan
        && let Some(auth) = auth_result
        && let Some(allowed) = &auth.allowed_combos
        && !allowed.is_empty()
        && !allowed.contains(&combo_id.0)
    {
        return Err(ApiError(CoreError::Auth(
            "combo not allowed for this key".to_string(),
        )));
    }

    Ok(plan)
}

fn translate_plan_to_targets(
    state: &AppState,
    plan: &RoutingPlan,
    api_key_id: Option<ApiKeyId>,
) -> Result<(ComboId, Option<openproxy_core::combos::Combo>, Option<Vec<openproxy_core::combos::ComboTarget>>), ApiError> {
    match plan {
        RoutingPlan::Direct {
            provider_id,
            account_id,
            model_row_id,
            ..
        } => {
            let (synthetic_combo, synthetic_targets) =
                build_synthetic_combo(provider_id.clone(), *account_id, *model_row_id);
            Ok((
                ComboId(SYNTHETIC_COMBO_ID),
                Some(synthetic_combo),
                Some(synthetic_targets),
            ))
        }
        RoutingPlan::Combo { combo_id, .. } => Ok((*combo_id, None, None)),
        RoutingPlan::NotFound { model, hint } => {
            let _ = record_model_not_found_usage_row(state, RequestId::new(), api_key_id, model);
            let mut msg = format!("model not found: {}", model);
            if let Some(h) = hint {
                msg.push_str(&format!(" (hint: {})", h));
            }
            Err(ApiError(CoreError::ModelNotFound {
                provider: "<unknown>".into(),
                model: msg,
            }))
        }
    }
}

fn build_pipeline(state: &AppState) -> Pipeline {
    let config = PipelineConfig {
        defaults: openproxy_core::timeouts::Timeouts::from_config(&state.timeouts()),
        racing: state.config().racing.clone(),
        retries: state.config().retries,
        max_attempts: state.config().retries.max_attempts,
        master_key: state.master_key().clone(),
        adapters: Arc::new(state.adapters()),
        http_client: state.http_client().clone(),
        cooldown_secs: state.config().cooldown.cooldown_secs,
        cooldown_max_secs: state.config().cooldown.max_secs,
        cooldown_factor: state.config().cooldown.factor,
        upstream_client: state.upstream_client().clone(),
        oauth_provider_registry: Some(state.oauth_provider_registry()),
        compression_mode: state.compression_mode(),
        idle_chunk_retryable: state.idle_chunk_retryable(),
        quota_protection: state.quota_protection(),
    };
    Pipeline::with_selection_registry(
        state.db_pool().writer_arc(),
        config,
        state.record_bodies_and_flags(),
        state.selection_registry(),
        state.circuit_breaker(),
    )
}

fn calculate_watchdog_budget(state: &AppState, headers: &HeaderMap) -> u64 {
    let client_deadline_ms: Option<u64> = headers
        .get("x-request-deadline-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0);
    let total_ms = state.timeouts().total_ms;
    match client_deadline_ms {
        Some(client_ms) if client_ms < total_ms => {
            tracing::debug!(
                client_ms,
                total_ms,
                "client requested shorter cancellation deadline than upstream total"
            );
            client_ms
        }
        _ => total_ms,
    }
}

fn create_watchdog_channels(_is_streaming: bool) -> (tokio::sync::watch::Receiver<bool>, tokio::sync::watch::Sender<bool>) {
    let (fresh_tx, fresh_rx) = tokio::sync::watch::channel(false);
    (fresh_rx, fresh_tx)
}

fn spawn_watchdog(
    done_rx: tokio::sync::oneshot::Receiver<()>,
    watchdog_tx: tokio::sync::watch::Sender<bool>,
    budget_ms: u64,
) {
    tokio::spawn(async move {
        tokio::select! {
            _ = done_rx => {}
            _ = tokio::time::sleep(std::time::Duration::from_millis(budget_ms)) => {
                tracing::warn!(
                    budget_ms,
                    "watchdog timer fired — cancelling pipeline (this is a total-budget timeout, NOT a client disconnect)"
                );
                let _ = watchdog_tx.send(true);
            }
        }
    });
}

async fn handle_streaming_response(
    pipeline: Pipeline,
    req: PipelineRequest,
    done_tx: tokio::sync::oneshot::Sender<()>,
    rx: tokio::sync::mpsc::Receiver<Bytes>,
) -> Result<axum::response::Response, ApiError> {
    let (error_tx, error_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(1);

    tokio::spawn(async move {
        let result = pipeline.run(req).await;
        let _ = done_tx.send(());

        if let Some(err) = result.error {
            let error_json = serde_json::json!({
                "error": {
                    "message": err.to_string(),
                    "type": err.code(),
                    "code": err.http_status(),
                }
            });
            let error_str = serde_json::to_string(&error_json).unwrap_or_default();
            let mut frame = bytes::BytesMut::with_capacity(error_str.len() + 16);
            frame.extend_from_slice(b"data: ");
            frame.extend_from_slice(error_str.as_bytes());
            frame.extend_from_slice(b"\n\n");
            let _ = error_tx.send(frame.freeze()).await;
        }
    });

    let main_stream = ReceiverStream::new(rx);
    let error_stream = ReceiverStream::new(error_rx);
    let mut merged = futures::stream::SelectAll::new();
    merged.push(main_stream);
    merged.push(error_stream);

    let sse_stream = SseBytesStream {
        inner: merged,
        keepalive: tokio::time::interval_at(
            tokio::time::Instant::now() + SSE_KEEPALIVE_INTERVAL,
            SSE_KEEPALIVE_INTERVAL,
        ),
    };

    let body = axum::body::Body::from_stream(sse_stream);
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "text/event-stream; charset=utf-8",
        )],
        body,
    )
        .into_response())
}

async fn handle_sync_response(
    pipeline: Pipeline,
    req: PipelineRequest,
    done_tx: tokio::sync::oneshot::Sender<()>,
) -> Result<axum::response::Response, ApiError> {
    let started = Instant::now();
    let result = pipeline.run(req).await;
    let _ = done_tx.send(());
    let elapsed_ms = started.elapsed().as_millis();

    if let Some(err) = result.error {
        let status =
            StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        tracing::debug!(
            status = status.as_u16(),
            attempts = result.attempts,
            elapsed_ms,
            error = %err,
            "chat_completions: error from pipeline"
        );
        return Err(ApiError(err));
    }

    let body_value = match result.final_response {
        Some(resp) => serde_json::to_value(&resp).unwrap_or_else(|e| {
            json!({
                "error": {
                    "code": "internal",
                    "message": format!("serialize response: {e}"),
                }
            })
        }),
        None => {
            tracing::warn!(
                attempts = result.attempts,
                elapsed_ms,
                "chat_completions: pipeline returned neither error nor response"
            );
            json!({
                "error": {"code": "internal", "message": "no response from pipeline"}
            })
        }
    };

    Ok(Json(body_value).into_response())
}

/// Resolve the caller from the `Authorization` header.
///
/// Behaviour matrix:
///
/// | Header state                          | Result    |
/// | ------------------------------------- | --------- |
/// | absent, no active keys configured     | `Ok(None)` — anonymous OK (local-dev). |
/// | absent, ≥1 active key configured      | 401 `missing api key`. |
/// | `Authorization: <other-scheme> ...`   | treated as missing → falls into the two rows above. |
/// | `Authorization: Bearer *** | look up by SHA-256, enforce active+unexpired+scope+allowlist. |
/// | `Bearer <key>` not in the table        | 401 `invalid api key`. |
/// | key is revoked / inactive              | 401 `api key revoked or inactive`. |
/// | key has expired                       | 401 `api key expired`. |
/// | key lacks the `chat` scope            | 403 `api key lacks 'chat' scope`. |
/// | key's model allowlist excludes request | 403 `model '...' not allowed for this key`. |
pub(crate) fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    requested_model: &str,
) -> Result<Option<AuthResult>, ApiError> {
    let token = match headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
    {
        Some(t) => t.trim(),
        None => {
            // MEDIUM fix (audit finding #5): the previous behaviour
            // silently admitted anonymous traffic, so an open proxy
            // on the public internet would forward any client's
            // prompts to paid upstreams — the operator would foot
            // the bill with no visibility or per-key rate limits.
            //
            // Backward-compat path: if NO active API keys are
            // configured, this is a fresh install (local-dev /
            // docker / first run) and anonymous traffic is fine.
            // As soon as the operator creates the first key, the
            // chat endpoint requires that key. The transition is
            // automatic — no config knob needed.
            //
            // `count_active` is a SELECT COUNT(*) — use the READER so
            // the anonymous-fallback check doesn't serialize through
            // the writer mutex (see `db::conn::DbPool::reader`).
            let active =
                core_api_keys::count_active(&state.db_pool().reader()).map_err(ApiError)?;
            if active == 0 {
                tracing::debug!(
                    target: "openproxy::auth",
                    "anonymous request admitted (no active api keys configured)"
                );
                return Ok(None);
            }
            return Err(ApiError(CoreError::Auth("missing api key".into())));
        }
    };
    if token.is_empty() {
        // Same gate: a bare `Authorization: Bearer ` (empty
        // token) is treated as "no header".
        let active = core_api_keys::count_active(&state.db_pool().reader()).map_err(ApiError)?;
        if active == 0 {
            return Ok(None);
        }
        return Err(ApiError(CoreError::Auth("missing api key".into())));
    }

    let key_hash = core_api_keys::hash_key(token);
    // Auth is a SELECT by hash — use the READER so chat requests don't
    // serialize through the writer mutex (same fix as the admin path).
    let r = state.db_pool().reader();
    let key = match core_api_keys::get_by_hash(&r, &key_hash).map_err(ApiError)? {
        Some(k) => k,
        None => {
            return Err(ApiError(CoreError::Auth("invalid api key".into())));
        }
    };

    if !key.is_active {
        return Err(ApiError(CoreError::Auth(
            "api key revoked or inactive".into(),
        )));
    }

    if let Some(exp) = &key.expires_at {
        // LOW fix (#15): same parser-based check as in admin.rs —
        // see api_keys.rs::is_expired for the rationale.
        if core_api_keys::is_expired(Some(exp), chrono::Utc::now())
            .map_err(|e| ApiError(CoreError::Internal(format!("expires_at check: {e}"))))?
        {
            return Err(ApiError(CoreError::Auth("api key expired".into())));
        }
    }

    if !key.scopes.iter().any(|s| s == "chat") {
        return Err(ApiError(CoreError::Auth(
            "api key lacks required scope".into(),
        )));
    }

    if let Some(allowed) = &key.allowed_models
        && !allowed.is_empty()
        && !allowed.iter().any(|m| m == requested_model)
    {
        return Err(ApiError(CoreError::Auth(format!(
            "model '{}' not allowed for this key",
            requested_model
        ))));
    }

    // Fire-and-forget the `last_used_at` UPDATE on a blocking thread.
    // The chat hot path no longer blocks on acquiring the writer mutex.
    // `touch_last_used` already throttles itself to 5-minute writes
    // (see `LAST_USED_THROTTLE_SECS` in `api_keys.rs`), so the extra
    // writer acquisition only happens once per key per five minutes.
    let pool = Arc::clone(state.db_pool());
    let key_id = key.id;
    tokio::task::spawn_blocking(move || {
        let w = pool.writer();
        let _ = core_api_keys::touch_last_used(&w, key_id);
    });

    Ok(Some(AuthResult {
        key_id: key.id,
        allowed_combos: key.allowed_combos.clone(),
    }))
}

/// Result of a successful chat authentication — the key id plus any
/// per-key restrictions that need to be enforced after routing.
pub(crate) struct AuthResult {
    pub(crate) key_id: ApiKeyId,
    pub(crate) allowed_combos: Option<Vec<i64>>,
}

/// Record a single `usage` row for the `RoutingPlan::NotFound` path.
///
/// Mirrors the `record_no_healthy_targets_row` helper in the pipeline:
/// zero tokens, zero cost, `race_total=1`, `race_lost=false`,
/// `error_msg="model_not_found"`, and `status_code=404`. Without this
/// row a misconfigured client (or a typo in the model name) would
/// never appear in the dashboard's Live Logs tail — a confusing UX
/// since the operator would see the same "No recent requests yet"
/// message whether the system is healthy or completely empty.
fn record_model_not_found_usage_row(
    state: &AppState,
    request_id: RequestId,
    api_key_id: Option<ApiKeyId>,
    upstream_model: &str,
) -> std::result::Result<(), ApiError> {
    use openproxy_core::{
        cost::{self, UsageInput},
        ids::{ProviderId, TraceId},
    };
    let input = UsageInput {
        request_id,
        trace_id: TraceId::new(),
        attempt: 1,
        provider_id: ProviderId::new(""),
        account_id: None,
        combo_id: None,
        combo_target_id: None,
        model_row_id: None,
        upstream_model_id: upstream_model.to_string(),
        prompt_tokens: None,
        completion_tokens: None,
        connect_ms: None,
        ttft_ms: None,
        total_ms: 0,
        status_code: 404,
        error_msg: Some("model_not_found".to_string()),
        race_total: 1,
        race_lost: false,
        api_key_id,
        request_body_json: None,
        response_body_json: None,
        request_headers: None,
        response_headers: None,
        error_message: Some("model_not_found".to_string()),
        race_attempts: 1,
        is_streaming: false,
        stream_complete: false,
        stop_reason: None,
        compression_savings_pct: None,
        compression_techniques: None,
        // model_not_found is a terminal 404 — the client sees this error.
        client_response: true,
        // model_not_found: tokens are not estimated (no request was sent)
        prompt_tokens_estimated: false,
        completion_tokens_estimated: false,
        endpoint_kind: openproxy_core::endpoint::EndpointKind::Chat,
    };
    // MEDIUM-5 fix: use try_writer_for with the hot-path timeout so
    // this write doesn't block indefinitely under admin lock contention.
    // If the lock can't be acquired in 100ms, log and drop the row —
    // a missing usage row is preferable to a 5xx on the 404 path.
    let w = match state
        .db_pool()
        .try_writer_for(std::time::Duration::from_millis(100))
    {
        Some(w) => w,
        None => {
            tracing::warn!("hot-path writer lock timeout on model_not_found usage row; dropping");
            return Ok(());
        }
    };
    let _ = cost::record(&w, &input).map_err(ApiError);
    Ok(())
}
