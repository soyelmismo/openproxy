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
    extract::Extension,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use bytes::Bytes;
use futures::stream::Stream;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;
use openproxy_core::{
    api_keys as core_api_keys,
    ids::{ApiKeyId, ComboId, RequestId, TraceId},
    pipeline::{Pipeline, PipelineConfig, PipelineRequest},
    redact::redact_sensitive_headers,
    routing::{self, build_synthetic_combo, RoutingPlan, SYNTHETIC_COMBO_ID},
    translation::OpenAIRequest,
    CoreError,
};
use serde_json::json;
use std::time::Instant;

use crate::{
    disconnect::CancelWatch,
    error::ApiError,
    state::AppState,
};

/// SSE keepalive interval. Sends `: keep-alive\n\n` (an SSE comment)
/// every 15 seconds so proxies and load balancers don't close the
/// connection while the upstream is still generating tokens.
const SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

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
pub async fn chat_completions(
    State(state): State<AppState>,
    Extension(cancel): Extension<CancelWatch>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> Result<axum::response::Response, ApiError> {
    run_pipeline(state, cancel, headers, body).await
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
    cancel: CancelWatch,
    headers: HeaderMap,
    body: serde_json::Value,
) -> Result<axum::response::Response, ApiError> {
    // 1. Parse the OpenAI request.
    let openai_req: OpenAIRequest = serde_json::from_value(body)
        .map_err(|e| ApiError(CoreError::Parse(e.to_string())))?;

    // 2. Authenticate (backward-compatible: no header = anonymous).
    //
    // The MVP keeps the chat endpoint open to anonymous traffic so
    // local development and the in-cluster dashboard can hit
    // /v1/chat/completions without a key. If the caller sends a
    // `Bearer` token we *do* enforce it: an unknown / revoked /
    // expired / scope-insufficient / model-disallowed key is a 401.
    //
    // The model-allowlist check uses the *proxy-level* id the client
    // sent (which carries the `<provider>/` prefix returned by
    // /v1/models). We strip the prefix further down before talking
    // to the upstream; the allowlist match stays prefix-aware so
    // a client that only knows the full id is still gated correctly.
    let auth_result = authenticate(&state, &headers, &openai_req.model)?;
    let api_key_id: Option<ApiKeyId> = auth_result.as_ref().map(|r| r.key_id);

    // HIGH-3 fix: per-key rate limiting. Default: 60 req/min per key.
    // Anonymous requests (no key) are rate-limited by client IP.
    let rl_key = if let Some(id) = api_key_id {
        format!("key:{}", id.0)
    } else {
        // Use the connection's remote addr as the rate-limit key for
        // anonymous requests. If unavailable, fall back to a shared
        // "anon" bucket.
        format!("ip:{}", headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).unwrap_or("anon"))
    };
    if !state.rate_limiter().check(&rl_key) {
        return Err(ApiError(CoreError::RateLimited {
            provider: "rate_limiter".into(),
            retry_after_ms: 60_000,
        }));
    }

    // 3. Resolve the routing plan.
    //
    // Two paths:
    //   a) Legacy override: `x-openproxy-combo: <name>` forces a
    //      specific combo, bypassing model resolution. This is the
    //      back-compat shim the previous header-driven routing
    //      depended on; we keep it so existing clients keep working.
    //   b) Model-driven (default): the `model` field is run through
    //      `routing::resolve` which returns one of
    //      `Direct` / `Combo` / `NotFound`.
    //
    // We hold the writer for the duration of the resolution so the
    // combo/account lookups see a consistent view of the DB.
    let legacy_combo_name = headers
        .get("x-openproxy-combo")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let plan = {
        let w = state.db_pool().writer();
        if let Some(name) = legacy_combo_name.as_deref() {
            // Legacy override path. Bypass model resolution and
            // build a Combo plan by name.
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
                None => {
                    return Err(ApiError(CoreError::ComboNotFound(0)));
                }
            }
        } else {
            routing::resolve(&w, &openai_req.model)?
        }
    };

    // 4. Translate the plan into a `PipelineRequest`. The `Direct`
    //    branch builds a synthetic in-memory combo so the rest of
    //    the pipeline is reused unchanged.
    //
    //    MEDIUM-1 fix: enforce `allowed_combos` here. The field was
    //    stored on the API key but never checked — a key with
    //    `allowed_combos=[5]` could still hit any combo via the
    //    `x-openproxy-combo` header or the `combo:<name>` model alias.
    //    Now we check after routing resolves the combo_id.
    if let RoutingPlan::Combo { combo_id, .. } = &plan
        && let Some(auth) = &auth_result
        && let Some(allowed) = &auth.allowed_combos
        && !allowed.is_empty()
        && !allowed.contains(&combo_id.0)
    {
        return Err(ApiError(CoreError::Auth(
            "combo not allowed for this key".to_string()
        )));
    }

    let (combo_id, combo_override, targets_override) = match &plan {
        RoutingPlan::Direct {
            provider_id,
            account_id,
            model_row_id,
            ..
        } => {
            let (synthetic_combo, synthetic_targets) = build_synthetic_combo(
                provider_id.clone(),
                *account_id,
                *model_row_id,
            );
            // The pipeline carries the synthetic combo + targets in
            // its override slots; `combo_id` is the sentinel so
            // usage rows can be grepped for synthetic dispatch.
            (
                ComboId(SYNTHETIC_COMBO_ID),
                Some(synthetic_combo),
                Some(synthetic_targets),
            )
        }
        RoutingPlan::Combo { combo_id, .. } => (*combo_id, None, None),
        RoutingPlan::NotFound { model, hint } => {
            // Write a usage row so the dashboard's Live Logs tail
            // shows the misroute.
            let _ = record_model_not_found_usage_row(
                &state,
                RequestId::new(),
                api_key_id,
                model,
            );
            let mut msg = format!("model not found: {}", model);
            if let Some(h) = hint {
                msg.push_str(&format!(" (hint: {})", h));
            }
            return Err(ApiError(CoreError::ModelNotFound {
                provider: "<unknown>".into(),
                model: msg,
            }));
        }
    };

    // 5. Build the pipeline config from the app config.
    let config = PipelineConfig {
        defaults: openproxy_core::timeouts::Timeouts::from_config(
            &state.timeouts(),
        ),
        racing: state.config().racing.clone(),
        retries: state.config().retries,
        max_attempts: state.config().retries.max_attempts,
        master_key: state.master_key().clone(),
        adapters: Arc::new(state.adapters()),
        http_client: state.http_client().clone(),
        // Read from `[cooldown].cooldown_secs` / `OPENPROXY_COOLDOWN_SECS`.
        // Default 60s when neither is set; the loader fills in
        // `CooldownConfig::default()` for the TOML case.
        cooldown_secs: state.config().cooldown.cooldown_secs,
        // Use the shared `UpstreamClient` from `AppState` (created once
        // at startup). Sharing it means the underlying hyper client's
        // per-host connection pool is reused across all in-flight
        // requests, eliminating the per-request TCP+TLS handshake
        // (~50-200ms on WAN) that a fresh `UpstreamClient::new()` would
        // pay. `state.upstream_client()` returns `&Arc<UpstreamClient>`;
        // the cheap `Arc` clone here is all that's needed.
        upstream_client: state.upstream_client().clone(),
        oauth_provider_registry: Some(state.oauth_provider_registry()),
        compression_mode: state.compression_mode(),
        idle_chunk_retryable: state.idle_chunk_retryable(),
    };
    let pipeline = Pipeline::with_recording_flag(
        state.db_pool().writer_arc(),
        config,
        state.record_bodies_and_flags(),
    );

    // 6. Per-request IDs. The middleware already stamped a
    //    `RequestId` in the request extensions; we use a fresh one
    //    here so the pipeline output and the usage row share the
    //    same value.
    let request_id = RequestId::new();
    let trace_id = TraceId::new();

    // 7. Watch channel for client-disconnect signal.
    //
    // The pipeline's dispatch loop checks `client_disconnected` at
    // each target boundary (pipeline.rs:475-478) and aborts with
    // `CoreError::ClientDisconnected` (HTTP 499) when it fires. It
    // ALSO short-circuits the upstream `reqwest::send()` and SSE
    // stream reads via `tokio::select!` (see pipeline.rs, the
    // `cancellation_during_send_aborts_upstream_request` /
    // `cancellation_during_streaming_aborts_response_stream` /
    // `cancellation_mid_sse_stream_aborts_immediately` regression
    // tests).
    //
    // The watch is allocated by the `client_disconnect_middleware`
    // mounted in router.rs on the chat routes. That middleware
    // wraps both the *request* and *response* body in a
    // `DisconnectBody` that fires the watch on any body-level error
    // — so a real TCP-level cancel (RST, half-close, hangup) flips
    // the watch within milliseconds of the kernel noticing.
    //
    // The deadline-based watchdog below is a *fallback* for the
    // case where the client doesn't close the TCP connection but
    // simply stops reading the streaming response (a "soft"
    // cancel). The kernel won't fire a body error in that case, so
    // we use a timer as the second source of truth. The client can
    // shrink the deadline with `x-request-deadline-ms`; we cap it
    // at `timeouts.total` so a misbehaving client cannot drag the
    // watchdog past the upstream call's own timeout.
    let cancel_tx = cancel.tx.clone();
    let cancel_rx = cancel.rx.clone();

    // Determine the deadline for the watchdog. The client may
    // override it via `x-request-deadline-ms` (a millisecond
    // budget they want the proxy to honor); we cap it at
    // `timeouts.total` so a misbehaving client cannot drag the
    // watchdog past the upstream call's own timeout.
    let client_deadline_ms: Option<u64> = headers
        .get("x-request-deadline-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0);
    let total_ms = state.timeouts().total_ms;
    let watchdog_budget_ms = match client_deadline_ms {
        Some(client_ms) if client_ms < total_ms => {
            tracing::debug!(
                client_ms,
                total_ms,
                "client requested shorter cancellation deadline than upstream total"
            );
            client_ms
        }
        _ => total_ms,
    };

    // Spawn the watchdog. It holds only the `cancel_tx` sender,
    // not the pipeline state, so a panic in the spawn is contained
    // to this task and the request still completes via the
    // existing `total` timeout on the reqwest send.
    {
        let cancel_tx = cancel_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(watchdog_budget_ms)).await;
            // `send` on a watch is a no-op if the channel is closed
            // (i.e. the receiver was dropped because the pipeline
            // finished earlier). We don't care about the result.
            let _ = cancel_tx.send(true);
        });
    }

    // 8. Build and run the pipeline request.
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    let req = PipelineRequest {
        request_id,
        trace_id,
        combo_id,
        openai_request: openai_req.clone(),
        client_disconnected: cancel_rx,
        stream_sink: Some(openproxy_core::race_sink::StreamSink::Direct(tx)),
        api_key_id,
        combo_override,
        targets_override,
        // Strip the secret-bearing headers BEFORE the BTreeMap
        // crosses into the pipeline. The full HeaderMap would
        // persist verbatim into `usage.request_headers` whenever
        // recording is on, which would leak the caller's
        // `Authorization: Bearer *** ` `Cookie: ...`, etc. The
        // helper is the single source of truth for what counts as
        // "sensitive" (see `openproxy_core::redact`).
        request_headers: redact_sensitive_headers(&headers),
        race_cancelled: false,
        race_cancel: None,
    };

    if openai_req.stream {
        // Streaming path: spawn the pipeline in a background task
        // and return an SSE stream that reads from the channel.
        //
        // Errors are propagated through a separate channel so the
        // client receives a structured error event instead of a
        // silent disconnect.
        let (error_tx, error_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(1);

        tokio::spawn(async move {
            let result = pipeline.run(req).await;

            if let Some(err) = result.error {
                let error_json = serde_json::json!({
                    "error": {
                        "message": err.to_string(),
                        "type": err.code(),
                        "code": err.http_status(),
                    }
                });
                let error_str = serde_json::to_string(&error_json).unwrap_or_default();
                // Pre-format error as SSE frame so the response write
                // path sees the same `data: {json}\n\n` shape as normal
                // chunks — no special-casing needed.
                let mut frame = bytes::BytesMut::with_capacity(error_str.len() + 16);
                frame.extend_from_slice(b"data: ");
                frame.extend_from_slice(error_str.as_bytes());
                frame.extend_from_slice(b"\n\n");
                let _ = error_tx.send(frame.freeze()).await;
            }
            // error_tx drops here → error channel closes
        });

        // Merge both SSE channels into one stream with keepalive.
        let main_stream = ReceiverStream::new(rx);
        let error_stream = ReceiverStream::new(error_rx);
        let mut merged = futures::stream::SelectAll::new();
        merged.push(main_stream);
        merged.push(error_stream);

        let sse_stream = SseBytesStream {
            inner: merged,
            keepalive: tokio::time::interval(SSE_KEEPALIVE_INTERVAL),
        };

        // Use `Body::from_stream` to write the pre-formatted SSE
        // frames directly to the HTTP response, bypassing axum's
        // `Sse` wrapper (which would re-wrap our already-formatted
        // `data: {...}\n\n` frames).
        let body = axum::body::Body::from_stream(sse_stream);
        return Ok((
            [(
                axum::http::header::CONTENT_TYPE,
                "text/event-stream; charset=utf-8",
            )],
            body,
        )
            .into_response());
    }

    // Non-streaming path: run the pipeline synchronously.
    let started = Instant::now();
    let result = pipeline.run(req).await;
    let elapsed_ms = started.elapsed().as_millis();

    // 9. Translate the pipeline result into an HTTP response.
    if let Some(err) = result.error {
        let status = StatusCode::from_u16(err.http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
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
fn authenticate(
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
            let active = core_api_keys::count_active(&state.db_pool().reader())
                .map_err(ApiError)?;
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
        let active = core_api_keys::count_active(&state.db_pool().reader())
            .map_err(ApiError)?;
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
struct AuthResult {
    key_id: ApiKeyId,
    allowed_combos: Option<Vec<i64>>,
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
    };
    // MEDIUM-5 fix: use try_writer_for with the hot-path timeout so
    // this write doesn't block indefinitely under admin lock contention.
    // If the lock can't be acquired in 100ms, log and drop the row —
    // a missing usage row is preferable to a 5xx on the 404 path.
    let w = match state.db_pool().try_writer_for(std::time::Duration::from_millis(100)) {
        Some(w) => w,
        None => {
            tracing::warn!("hot-path writer lock timeout on model_not_found usage row; dropping");
            return Ok(());
        }
    };
    let _ = cost::record(&w, &input).map_err(ApiError);
    Ok(())
}
