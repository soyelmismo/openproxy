use crate::{error::ApiError, state::AppState};
use axum::{extract::State, http::HeaderMap};
use openproxy_core::{CoreError, api_keys as core_api_keys, ids::ApiKeyId};
use std::sync::Arc;

/// Extracted parsed JSON payload for the chat endpoint.
#[derive(Clone)]
pub struct ParsedChatRequest {
    pub raw: Arc<serde_json::Value>,
    pub bytes: bytes::Bytes,
}

/// Result of a successful chat authentication — the key id plus any
/// per-key restrictions that need to be enforced after routing.
#[derive(Clone)]
pub struct ValidatedApiToken {
    pub(crate) key_id: ApiKeyId,
    pub(crate) allowed_combos: Option<Vec<i64>>,
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
) -> Result<Option<ValidatedApiToken>, ApiError> {
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

    Ok(Some(ValidatedApiToken {
        key_id: key.id,
        allowed_combos: key.allowed_combos.clone(),
    }))
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
pub async fn auth_middleware(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, crate::error::ApiError> {
    let (mut parts, body) = req.into_parts();

    // Enforce 32 MiB limit directly, matching DefaultBodyLimit
    let bytes = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            if e.to_string().contains("length limit exceeded") {
                return Ok(axum::response::IntoResponse::into_response(
                    axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                ));
            } else {
                return Err(crate::error::ApiError(openproxy_core::CoreError::Parse(
                    e.to_string(),
                )));
            }
        }
    };

    let parsed: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| crate::error::ApiError(openproxy_core::CoreError::Parse(e.to_string())))?;

    let requested_model = parsed.get("model").and_then(|v| v.as_str()).unwrap_or("");

    let auth_result = authenticate(&state, &parts.headers, requested_model)?;

    parts.extensions.insert(ParsedChatRequest {
        raw: Arc::new(parsed),
        bytes: bytes.clone(),
    });
    if let Some(res) = auth_result {
        parts.extensions.insert(res);
    }

    let req = axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes));
    Ok(next.run(req).await)
}
