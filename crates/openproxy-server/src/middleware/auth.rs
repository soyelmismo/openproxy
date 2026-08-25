use crate::{error::ApiError, state::AppState};
use axum::{extract::State, http::HeaderMap};
use openproxy_core::api_keys as core_api_keys;
use openproxy_types::{CoreError, ids::ApiKeyId};
use std::sync::Arc;

/// Extracted parsed JSON payload for the chat endpoint.
#[derive(Clone)]
pub struct ParsedChatRequest {
    pub parsed: Arc<openproxy_types::OpenAIRequest>,
    pub bytes: bytes::Bytes,
}

/// Result of a successful chat authentication — the key id plus any
/// per-key restrictions that need to be enforced after routing.
#[derive(Clone, Debug)]
pub struct ValidatedApiToken {
    pub key_id: ApiKeyId,
    pub allowed_models: Option<Vec<String>>,
    pub allowed_combos: Option<Vec<i64>>,
    pub blacklisted_providers: Option<Vec<String>>,
    pub blacklisted_models: Option<Vec<String>>,
}

impl ValidatedApiToken {
    pub fn is_combo_allowed(&self, combo_id: i64) -> bool {
        match &self.allowed_combos {
            Some(allowed) if !allowed.is_empty() => allowed.contains(&combo_id),
            _ => true,
        }
    }

    pub fn is_provider_allowed(&self, provider_id: &str) -> bool {
        if let Some(blacklisted) = &self.blacklisted_providers
            && blacklisted.iter().any(|p| p == provider_id || p == "*")
        {
            return false;
        }
        true
    }

    pub fn is_model_allowed(&self, model: &str, provider_id: Option<&str>) -> bool {
        let (prov_from_model, bare_model) = match model.split_once('/') {
            Some((p, rest)) => (Some(p), rest),
            None => (None, model),
        };

        let full_id = provider_id.and_then(|p| {
            if model
                .strip_prefix(p)
                .is_some_and(|rest| rest.starts_with('/'))
            {
                None
            } else {
                Some(format!("{p}/{model}"))
            }
        });

        let matches_spec = |spec: &str, candidate: &str| -> bool {
            if spec == "*" || spec == candidate {
                return true;
            }
            if let Some(prefix) = spec.strip_suffix('*')
                && candidate.starts_with(prefix)
            {
                return true;
            }
            if let Some(suffix) = spec.strip_prefix('*')
                && candidate.ends_with(suffix)
            {
                return true;
            }
            false
        };

        let check_match = |pattern: &str| -> bool {
            matches_spec(pattern, model)
                || matches_spec(pattern, bare_model)
                || full_id.as_deref().is_some_and(|f| matches_spec(pattern, f))
                || model
                    .strip_prefix(pattern)
                    .is_some_and(|rest| rest.starts_with('/'))
                || model
                    .strip_suffix(pattern)
                    .is_some_and(|rest| rest.ends_with('/'))
        };

        // 1. Allowlist check
        if let Some(allowed) = &self.allowed_models
            && !allowed.is_empty()
        {
            let matches_allowed = allowed.iter().any(|m| check_match(m));
            if !matches_allowed {
                return false;
            }
        }

        // 2. Blacklisted providers check
        if let Some(blacklisted_provs) = &self.blacklisted_providers {
            if let Some(p) = provider_id
                && blacklisted_provs.iter().any(|bp| bp == p || bp == "*")
            {
                return false;
            }
            if let Some(p) = prov_from_model
                && blacklisted_provs.iter().any(|bp| bp == p || bp == "*")
            {
                return false;
            }
        }

        // 3. Blacklisted models check
        if let Some(blacklisted) = &self.blacklisted_models
            && blacklisted.iter().any(|b| check_match(b))
        {
            return false;
        }

        true
    }
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
/// | `Authorization: Bearer *** | look up by SHA-256, enforce active+unexpired+scope+allowlist+blacklist. |
/// | `Bearer <key>` not in the table        | 401 `invalid api key`. |
/// | key is revoked / inactive              | 401 `api key revoked or inactive`. |
/// | key has expired                       | 401 `api key expired`. |
/// | key lacks the `chat` scope            | 403 `api key lacks 'chat' scope`. |
/// | key's model allowlist excludes request | 403 `model '...' not allowed for this key`. |
/// | key's blacklist excludes request      | 403 `model '...' is blacklisted for this key`. |
pub(crate) fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    requested_model: &str,
) -> Result<Option<ValidatedApiToken>, ApiError> {
    let Some(token) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim)
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
        })
    else {
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
        let active = core_api_keys::count_active(&state.db_pool().reader()).map_err(ApiError)?;
        if active == 0 {
            tracing::debug!(
                target: "openproxy::auth",
                "anonymous request admitted (no active api keys configured)"
            );
            return Ok(None);
        }
        return Err(ApiError(CoreError::Auth("missing api key".into())));
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

    let key = verify_key_credentials(state, token, "chat")?;

    if !key.is_model_allowed(requested_model, None) {
        return Err(ApiError(CoreError::Auth(format!(
            "model '{requested_model}' not allowed or blacklisted for this key"
        ))));
    }

    Ok(Some(ValidatedApiToken {
        key_id: key.id,
        allowed_models: key.allowed_models,
        allowed_combos: key.allowed_combos,
        blacklisted_providers: key.blacklisted_providers,
        blacklisted_models: key.blacklisted_models,
    }))
}

/// Validate an API key credential against active keys, verifying:
/// 1. Hash matching
/// 2. Active status
/// 3. Expiration date
/// 4. Required scope presence
///
/// If valid, touches `last_used_at` asynchronously on a blocking thread
/// and returns the [`openproxy_core::api_keys::ApiKey`].
pub(crate) fn verify_key_credentials(
    state: &AppState,
    token: &str,
    required_scope: &str,
) -> Result<core_api_keys::ApiKey, ApiError> {
    let key_hash = core_api_keys::hash_key(token);
    // Auth is a SELECT by hash — use the READER so requests don't
    // serialize through the writer mutex.
    let r = state.db_pool().reader();
    let key = core_api_keys::get_by_hash(&r, &key_hash)
        .map_err(ApiError)?
        .ok_or_else(|| ApiError(CoreError::Auth("invalid api key".into())))?;

    if !key.is_active {
        return Err(ApiError(CoreError::Auth(
            "api key revoked or inactive".into(),
        )));
    }

    if let Some(exp) = &key.expires_at
        && core_api_keys::is_expired(Some(exp), chrono::Utc::now())
            .map_err(|e| ApiError(CoreError::Internal(format!("expires_at check: {e}"))))?
    {
        return Err(ApiError(CoreError::Auth("api key expired".into())));
    }

    if !key.scopes.iter().any(|s| s == required_scope) {
        return Err(ApiError(CoreError::Auth(
            "api key lacks required scope".into(),
        )));
    }

    // Fire-and-forget the `last_used_at` UPDATE on a blocking thread.
    // The hot path no longer blocks on acquiring the writer mutex.
    // `touch_last_used` already throttles itself to 5-minute writes
    // (see `LAST_USED_THROTTLE_SECS` in `api_keys.rs`).
    let pool = Arc::clone(state.db_pool());
    let key_id = key.id;
    tokio::task::spawn_blocking(move || {
        let w = pool.writer();
        let _ = core_api_keys::touch_last_used(&w, key_id);
    });

    Ok(key)
}

/// Authenticate the request against active API keys and verify model/combo authorization.
///
/// Returns `Ok(Some(key_id))` if authenticated with a key, `Ok(None)` if anonymous
/// access is permitted (0 active keys configured), or an [`ApiError`] on failure.
pub(crate) fn authenticate_and_authorize_model(
    state: &AppState,
    headers: &HeaderMap,
    model_name: &str,
) -> Result<Option<ApiKeyId>, ApiError> {
    let auth_result = authenticate(state, headers, model_name)?;
    let api_key_id: Option<ApiKeyId> = auth_result.as_ref().map(|r| r.key_id);

    if let Ok(openproxy_core::routing::RoutingPlan::Combo { combo_id, .. }) =
        openproxy_core::routing::resolve(&state.db_pool().reader(), model_name)
        && let Some(auth) = &auth_result
        && !auth.is_combo_allowed(combo_id.0)
    {
        return Err(ApiError(CoreError::Auth(
            "combo not allowed for this key".into(),
        )));
    }

    Ok(api_key_id)
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
/// `UpstreamClient::call()` `tokio::select!`, and the SSE `stream.next()`
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
            let err_str = e.to_string();
            if err_str.contains("length limit exceeded") {
                return Ok(axum::response::IntoResponse::into_response(
                    axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                ));
            }
            return Err(crate::error::ApiError(openproxy_types::CoreError::Parse(
                err_str,
            )));
        }
    };

    let mut parsed: openproxy_types::OpenAIRequest = serde_json::from_slice(&bytes)
        .map_err(|e| crate::error::ApiError(openproxy_types::CoreError::Parse(e.to_string())))?;

    // Sanitize orphaned tool calls and tool messages to avoid upstream 400 Bad Request errors.
    // DeepSeek and other strict OpenAI-compatible providers require that every
    // tool_call in an assistant message is followed by a matching tool response.
    const MAX_TOOL_CALLS: usize = 64;
    const MAX_ID_LEN: usize = 128;

    let mut valid_messages = Vec::with_capacity(parsed.messages.len());
    let mut last_assistant_tool_calls: Vec<String> = Vec::new();

    for mut msg in std::mem::take(&mut parsed.messages) {
        if msg.role == "assistant" {
            last_assistant_tool_calls.clear();
            if let Some(calls) = &mut msg.tool_calls {
                calls.truncate(MAX_TOOL_CALLS);
                for call in calls.iter() {
                    if let Some(id) = call.get("id").and_then(|v| v.as_str())
                        && id.len() <= MAX_ID_LEN
                    {
                        last_assistant_tool_calls.push(id.to_string());
                    }
                }
            }
            valid_messages.push(msg);
        } else if msg.role == "tool" {
            if let Some(id) = msg.tool_call_id.as_deref()
                && id.len() <= MAX_ID_LEN
                && let Some(pos) = last_assistant_tool_calls.iter().position(|c| c == id)
            {
                last_assistant_tool_calls.remove(pos);
                valid_messages.push(msg);
            }
        } else {
            last_assistant_tool_calls.clear();
            valid_messages.push(msg);
        }
    }
    parsed.messages = valid_messages;

    let mut remainder = parsed.messages.as_mut_slice();
    while let Some((msg, tail)) = remainder.split_first_mut() {
        remainder = tail;
        if msg.role == "assistant"
            && let Some(calls) = &mut msg.tool_calls
        {
            calls.retain(|call| {
                let call_id = call.get("id").and_then(|v| v.as_str());
                call_id.is_some_and(|id| {
                    id.len() <= MAX_ID_LEN
                        && remainder
                            .iter()
                            .take_while(|m| m.role == "tool")
                            .take(MAX_TOOL_CALLS)
                            .any(|m| m.tool_call_id.as_deref() == Some(id))
                })
            });
            if calls.is_empty() {
                msg.tool_calls = None;
            }
        }
    }

    // DeepSeek thinking mode requires `reasoning_content` to be passed back in assistant messages.
    // If a client strips it, we inject an empty string to prevent 400 errors.
    if parsed.model.to_lowercase().contains("deepseek") {
        for msg in &mut parsed.messages {
            if msg.role == "assistant" && !msg.extra.contains_key("reasoning_content") {
                msg.extra.insert(
                    "reasoning_content".to_string(),
                    serde_json::Value::String(String::new()),
                );
            }
        }
    }

    if parsed.model.is_empty() && parts.uri.path().starts_with("/v1/images") {
        parsed.model = "dall-e-2".to_string();
    }

    let requested_model = &parsed.model;

    let auth_result = authenticate(&state, &parts.headers, requested_model)?;

    parts.extensions.insert(ParsedChatRequest {
        parsed: Arc::new(parsed),
        bytes: bytes::Bytes::clone(&bytes),
    });
    if let Some(res) = auth_result {
        parts.extensions.insert(res);
    }

    let req = axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes));
    Ok(next.run(req).await)
}
