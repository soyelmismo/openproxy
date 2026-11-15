use crate::{error::ApiError, state::AppState};
use axum::{extract::State, http::HeaderMap};
// The plan-quota window system only exists with payments.
#[cfg(feature = "payments")]
use openproxy_core::api_key_quotas;
use openproxy_core::api_keys::{self as core_api_keys, ApiKey};
use openproxy_types::{CoreError, ids::ApiKeyId};
use std::sync::Arc;

use super::auth_sanitization::{
    inject_deepseek_reasoning_if_needed, read_request_body_capped, sanitize_tool_calls,
};
pub(crate) use super::auth_sanitization::{
    normalize_responses_tools, translate_responses_to_openai,
};

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
    pub key: Arc<core_api_keys::ApiKey>,
    pub key_id: ApiKeyId,
}

impl ValidatedApiToken {
    pub fn is_combo_allowed(&self, combo_id: i64) -> bool {
        self.key.is_combo_allowed(combo_id)
    }

    pub fn is_provider_allowed(&self, provider_id: &str) -> bool {
        self.key.is_provider_allowed(provider_id)
    }

    pub fn is_model_allowed(&self, model: &str, provider_id: Option<&str>) -> bool {
        self.key.is_model_allowed(model, provider_id)
    }
}

pub(crate) fn extract_bearer_or_api_key_token(headers: &HeaderMap) -> Option<&str> {
    headers
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
        .filter(|t| !t.is_empty())
}

/// Resolve the caller from the `Authorization` header.
/// Reject the request with 429 when the key already spent its dollar cap in
/// any enabled quota window. Keys without a quota row pass untouched.
///
/// The fixed 5h window opens lazily with the key's first request: when the
/// check reports a missing/expired anchor, the request re-anchors the window
/// (a conditional write that only fires on expiry) and checks again — the
/// fresh window starts at zero.
///
/// Payments-disabled build: no quota tables exist, so no key can ever have a
/// cap and the check no-ops.
async fn enforce_key_quota(state: &AppState, key_id: ApiKeyId) -> Result<(), ApiError> {
    #[cfg(not(feature = "payments"))]
    {
        let _ = (state, key_id);
        Ok(())
    }
    #[cfg(feature = "payments")]
    {
        let mut outcome = state
            .db_pool()
            .spawn_read(move |r| api_key_quotas::check(r, key_id))
            .await
            .map_err(|e| {
                tracing::error!(%e, "db error checking api key quota");
                ApiError(CoreError::Internal("quota check failed".into()))
            })?;
        if matches!(outcome, api_key_quotas::QuotaCheck::ReanchorNeeded) {
            state
                .db_pool()
                .spawn_write(move |w| api_key_quotas::anchor_five_hour_window(w, key_id))
                .await
                .map_err(|e| {
                    tracing::error!(%e, "db error anchoring the 5h quota window");
                    ApiError(CoreError::Internal("quota window anchor failed".into()))
                })?;
            outcome = state
                .db_pool()
                .spawn_read(move |r| api_key_quotas::check(r, key_id))
                .await
                .map_err(|e| {
                    tracing::error!(%e, "db error re-checking api key quota");
                    ApiError(CoreError::Internal("quota check failed".into()))
                })?;
        }
        let api_key_quotas::QuotaCheck::Violated(violation) = outcome else {
            return Ok(());
        };
        tracing::warn!(
            key_id = key_id.0,
            window = violation.window,
            spent = violation.spent_usd,
            cap = violation.cap_usd,
            "api key quota exceeded"
        );
        Err(ApiError(CoreError::QuotaExceeded {
            window: violation.window.to_string(),
            spent_usd: violation.spent_usd,
            cap_usd: violation.cap_usd,
            resets_at: violation
                .resets_at
                .unwrap_or_else(|| "unknown reset".into()),
        }))
    }
}

/// Reject the request with 402 when the key is credit-based (carries at
/// least one ledger entry) and its effective prepaid balance is zero.
/// Keys without ledger rows are not governed by the balance system.
async fn enforce_key_balance(state: &AppState, key_id: ApiKeyId) -> Result<(), ApiError> {
    let outcome = state
        .db_pool()
        .spawn_read(move |r| {
            if !openproxy_db::credits_ledger::is_credit_based(r, key_id.0)? {
                return Ok(None);
            }
            openproxy_db::credits_ledger::balance_units(r, key_id.0).map(Some)
        })
        .await
        .map_err(|e| {
            tracing::error!(%e, "db error checking credit balance");
            ApiError(CoreError::Internal("credit balance check failed".into()))
        })?;
    let Some(balance_units) = outcome else {
        return Ok(());
    };
    if balance_units > 0 {
        return Ok(());
    }
    tracing::warn!(key_id = key_id.0, "credit balance exhausted");
    Err(ApiError(CoreError::InsufficientCredits {
        balance_usd: 0.0,
    }))
}

pub(crate) async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<ValidatedApiToken>, ApiError> {
    static ALLOW_ANONYMOUS_DEPRECATION: std::sync::Once = std::sync::Once::new();
    if state.config().server.allow_anonymous {
        ALLOW_ANONYMOUS_DEPRECATION.call_once(|| {
            tracing::warn!(
                "server.allow_anonymous is deprecated and ignored: every /v1 request requires an api key"
            );
        });
    }
    let Some(token) = extract_bearer_or_api_key_token(headers) else {
        return Err(ApiError(CoreError::Auth("missing api key".into())));
    };

    let key = match verify_key_credentials(state, token, "chat").await {
        Ok(k) => k,
        Err(e) => {
            if let Ok(admin_key) = verify_key_credentials(state, token, "manage").await {
                admin_key
            } else {
                return Err(e);
            }
        }
    };

    if key.scopes.iter().any(|s| s == "manage")
        && let Some(target_key_id) = headers
            .get("x-openproxy-api-key-id")
            .or_else(|| headers.get("x-openproxy-key-id"))
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<i64>().ok())
    {
        let target_key = state
            .db_pool()
            .spawn_read(move |r| core_api_keys::get_by_id(r, ApiKeyId(target_key_id)))
            .await
            .map_err(|e| {
                tracing::error!(%e, "db error looking up simulated api key");
                ApiError(CoreError::Auth("invalid simulated api key".into()))
            })?
            .ok_or_else(|| {
                ApiError(CoreError::Auth(format!(
                    "api key id #{target_key_id} not found"
                )))
            })?;

        validate_key_record(&target_key, "chat")?;

        let token = ValidatedApiToken {
            key_id: target_key.id,
            key: Arc::new(target_key),
        };
        enforce_key_quota(state, token.key_id).await?;
        enforce_key_balance(state, token.key_id).await?;
        return Ok(Some(token));
    }

    let token = ValidatedApiToken {
        key_id: key.id,
        key,
    };
    enforce_key_quota(state, token.key_id).await?;
    enforce_key_balance(state, token.key_id).await?;
    Ok(Some(token))
}

pub(crate) fn validate_key_record_any_scope(
    key: &ApiKey,
    allowed_scopes: &[&str],
) -> Result<(), ApiError> {
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

    if !allowed_scopes.is_empty()
        && !key
            .scopes
            .iter()
            .any(|s| allowed_scopes.contains(&s.as_str()))
    {
        return Err(ApiError(CoreError::Auth(
            "api key lacks required scope".into(),
        )));
    }
    Ok(())
}

pub(crate) fn validate_key_record(key: &ApiKey, required_scope: &str) -> Result<(), ApiError> {
    validate_key_record_any_scope(key, &[required_scope])
}

/// Validate an API key credential against active keys for any of the given scopes
pub(crate) async fn verify_key_credentials_any_scope(
    state: &AppState,
    token: &str,
    allowed_scopes: &[&str],
) -> Result<Arc<core_api_keys::ApiKey>, ApiError> {
    let key_hash = core_api_keys::hash_key(token);
    let key = if let Some(cached) = state.get_cached_api_key(&key_hash) {
        cached
    } else {
        let lookup_hash = key_hash.clone();
        let fetched = state
            .db_pool()
            .spawn_read(move |r| core_api_keys::get_by_hash(r, &lookup_hash))
            .await
            .map_err(|e| {
                tracing::error!(%e, "db error looking up api key");
                ApiError(CoreError::Auth("invalid api key".into()))
            })?
            .ok_or_else(|| ApiError(CoreError::Auth("invalid api key".into())))?;
        let arc_key = Arc::new(fetched);
        state.cache_api_key(Arc::clone(&arc_key));
        arc_key
    };

    validate_key_record_any_scope(&key, allowed_scopes)?;

    // Security (OP-15): `touch_last_used` is throttled in SQL to one write per
    // LAST_USED_THROTTLE_SECS, but the old code contended for the pool's
    // SINGLE writer mutex on every authenticated request — at high RPS a
    // single key monopolized the writer and delayed usage tracking and admin
    // operations. The fetched row already tells us whether a stamp is due, so
    // only take the writer when it actually is.
    if last_used_needs_stamp(key.last_used_at.as_ref()) {
        let key_id = key.id;
        if let Err(e) = state.touch_last_used_batched(key_id).await {
            tracing::warn!(%e, "failed to touch last_used_at for api key");
        }
        // Refresh the cached stamp so the next request does not re-contend for
        // another LAST_USED_THROTTLE_SECS window (the cache holds an Arc, so
        // the row is cloned with the new stamp).
        let mut refreshed = (*key).clone();
        refreshed.last_used_at = Some(chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string());
        state.cache_api_key(Arc::new(refreshed));
    }

    Ok(key)
}

/// Validate an API key credential against active keys
pub(crate) async fn verify_key_credentials(
    state: &AppState,
    token: &str,
    required_scope: &str,
) -> Result<Arc<core_api_keys::ApiKey>, ApiError> {
    verify_key_credentials_any_scope(state, token, &[required_scope]).await
}

/// `true` when the row's `last_used_at` is older than the DB-side throttle
/// window (or missing/unparseable, which errs on the side of stamping).
fn last_used_needs_stamp(last_used_at: Option<&String>) -> bool {
    let Some(stamp) = last_used_at else {
        return true;
    };
    match chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d %H:%M:%S") {
        Ok(stamp) => {
            (chrono::Utc::now().naive_utc() - stamp).num_seconds()
                > core_api_keys::LAST_USED_THROTTLE_SECS
        }
        Err(_) => true,
    }
}

async fn verify_combo_authorization(
    state: &AppState,
    auth: Option<&ValidatedApiToken>,
    model_name: &str,
    allow_shadowed_combo: bool,
) -> Result<Option<openproxy_core::routing::RoutingPlan>, ApiError> {
    let requested = model_name.to_owned();
    let model_name = model_name.to_owned();
    let plan = state
        .db_pool()
        .spawn_read(move |r| openproxy_core::routing::resolve(r, &model_name))
        .await
        .ok();

    if let Some(openproxy_core::routing::RoutingPlan::Combo { combo_id, .. }) = &plan
        && combo_id.0 != openproxy_core::routing::SYNTHETIC_COMBO_ID
        && let Some(auth) = auth
        && !auth.is_combo_allowed(combo_id.0)
    {
        return Err(ApiError(CoreError::Auth(
            "combo not allowed for this key".into(),
        )));
    }

    // Resolution is model-first: a combo whose name collides with a real model
    // row (e.g. the `glm-5.3` combo next to the `glm-5.3` model rows other
    // providers advertise) never wins. A key that is denied that model but is
    // allowed the same-named combo asked for the combo, so resolve it through
    // the explicit `combo:` prefix and dispatch there instead of a 401.
    if allow_shadowed_combo && let Some(auth) = auth {
        let resolved_as_real_combo = matches!(
            &plan,
            Some(openproxy_core::routing::RoutingPlan::Combo { combo_id, .. })
                if combo_id.0 != openproxy_core::routing::SYNTHETIC_COMBO_ID
        );
        if !resolved_as_real_combo && !auth.is_model_allowed(&requested, None) {
            let combo_key = format!("combo:{requested}");
            let combo_plan = state
                .db_pool()
                .spawn_read(move |r| openproxy_core::routing::resolve(r, &combo_key))
                .await
                .ok();
            if let Some(openproxy_core::routing::RoutingPlan::Combo { combo_id, .. }) = &combo_plan
                && combo_id.0 != openproxy_core::routing::SYNTHETIC_COMBO_ID
                && auth.is_combo_allowed(combo_id.0)
            {
                return Ok(combo_plan);
            }
        }
    }
    Ok(plan)
}

/// Authenticate the request against active API keys and verify model/combo authorization.
pub(crate) async fn authenticate_and_authorize_model(
    state: &AppState,
    headers: &HeaderMap,
    model_name: &str,
) -> Result<Option<ApiKeyId>, ApiError> {
    let auth_result = authenticate(state, headers).await?;

    // These endpoints dispatch the model string themselves, so a combo that
    // shadows a model row cannot be honoured here: authorizing the combo would
    // route the caller to a model its key is not allowed. Only the chat path,
    // which dispatches the resolved plan, opts into the shadowed-combo swap.
    let combo_plan =
        verify_combo_authorization(state, auth_result.as_ref(), model_name, false).await?;
    // A combo that passed the key's combo allowlist (`is_combo_allowed`) is
    // authorized as a combo: the model-name gate does not apply to it. A
    // combo name will generally not match the plan's model allowlist even
    // when the combo itself is explicitly allowed.
    let combo_allowed = matches!(
        combo_plan,
        Some(openproxy_core::routing::RoutingPlan::Combo { combo_id, .. })
            if combo_id.0 != openproxy_core::routing::SYNTHETIC_COMBO_ID
    );

    if let Some(token) = &auth_result
        && !combo_allowed
        && !token.is_model_allowed(model_name, None)
    {
        return Err(ApiError(CoreError::Auth(format!(
            "model '{model_name}' not allowed or blacklisted for this key"
        ))));
    }

    Ok(auth_result.as_ref().map(|r| r.key_id))
}

pub async fn auth_middleware(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, crate::error::ApiError> {
    let (mut parts, body) = req.into_parts();
    let path = parts.uri.path();
    let is_responses = path == "/v1/responses"
        || path == "/responses"
        || path.starts_with("/v1/responses?")
        || path.starts_with("/responses?")
        || path.ends_with("/responses");

    let auth_result = authenticate(&state, &parts.headers).await?;

    // Security (OP-14): use the configured request body limit instead of a
    // hardcoded 32 MiB that ignored `server.request_max_body_bytes`.
    let bytes =
        match read_request_body_capped(body, state.config().server.request_max_body_bytes).await {
            Ok(b) => b,
            Err(resp) => return Ok(*resp),
        };

    let mut parsed: openproxy_types::OpenAIRequest = if is_responses {
        let responses_req: openproxy_types::ResponsesRequest = serde_json::from_slice(&bytes)
            .map_err(|e| {
                crate::error::ApiError(openproxy_types::CoreError::Parse(format!(
                    "Invalid Responses request: {e}"
                )))
            })?;
        if responses_req.model.trim().is_empty() {
            return Err(ApiError(CoreError::Validation("model is required".into())));
        }
        translate_responses_to_openai(&responses_req)
    } else {
        let mut req: openproxy_types::OpenAIRequest =
            serde_json::from_slice(&bytes).map_err(|e| {
                crate::error::ApiError(openproxy_types::CoreError::Parse(e.to_string()))
            })?;

        if req.messages.is_empty()
            && let Some(input_val) = req.extra.remove("input")
            && let Ok(input_items) = serde_json::from_value::<
                Vec<openproxy_types::ResponsesInputItem>,
            >(input_val.clone())
            .or_else(|_| {
                serde_json::from_value::<String>(input_val).map(|s| {
                    vec![openproxy_types::ResponsesInputItem::Message {
                        role: "user".to_string(),
                        content: openproxy_types::ResponsesContent::Plain(s),
                    }]
                })
            })
        {
            let max_output = req
                .extra
                .remove("max_output_tokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let synthetic_req = openproxy_types::ResponsesRequest {
                model: req.model.clone(),
                instructions: None,
                input: input_items,
                tools: req.tools.clone(),
                tool_choice: req.tool_choice.clone(),
                stream: req.stream,
                max_output_tokens: max_output,
                temperature: req.temperature,
                top_p: req.top_p,
                previous_response_id: None,
                extra: req.extra.clone(),
            };
            req = translate_responses_to_openai(&synthetic_req);
        }
        req.tools = normalize_responses_tools(req.tools);
        req
    };

    sanitize_tool_calls(&mut parsed.messages);
    for msg in &mut parsed.messages {
        msg.sanitize_name();
    }
    inject_deepseek_reasoning_if_needed(&mut parsed);

    // Security (audit H-9): redact PII from the sidecar identity fields
    // (`user`, Anthropic `metadata.user_id`) before the request is frozen
    // into its shared Arc and raw bytes, so formatters, translations and
    // recordings cannot leak what the messages redactor scrubbed.
    let pii_sidecar_changed =
        super::auth_sanitization::redact_pii_sidecar_fields(&mut parsed, &state.config().pii);
    let bytes = if pii_sidecar_changed {
        serde_json::to_vec(&parsed)
            .map(bytes::Bytes::from)
            .map_err(|e| ApiError(CoreError::Parse(e.to_string())))?
    } else {
        bytes
    };

    if parsed.model.is_empty() && parts.uri.path().starts_with("/v1/images") {
        parsed.model = "dall-e-2".to_string();
    }

    let requested_model = &parsed.model;
    let mut pre_resolved_plan = None;
    if let Some(token) = &auth_result {
        if !token
            .key
            .scopes
            .iter()
            .any(|s| s == "chat" || s == "manage")
        {
            return Err(ApiError(CoreError::Auth(
                "api key lacks required scope".into(),
            )));
        }

        // A combo that passed the key's combo allowlist (`is_combo_allowed`)
        // is authorized as a combo: the model-name gate does not apply to it.
        pre_resolved_plan =
            verify_combo_authorization(&state, Some(token), requested_model, true).await?;
        let combo_allowed = matches!(
            pre_resolved_plan,
            Some(openproxy_core::routing::RoutingPlan::Combo { combo_id, .. })
                if combo_id.0 != openproxy_core::routing::SYNTHETIC_COMBO_ID
        );
        if !combo_allowed && !token.is_model_allowed(requested_model, None) {
            return Err(ApiError(CoreError::Auth(format!(
                "model '{requested_model}' not allowed or blacklisted for this key"
            ))));
        }
    }

    if let Some(plan) = pre_resolved_plan {
        parts
            .extensions
            .insert(super::routing::PreResolvedRoutingPlan {
                model: requested_model.to_string(),
                legacy_combo_name: None,
                provider_override: None,
                account_override: None,
                plan,
            });
    }

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

/// Header-only API-key gate for non-chat `/v1` endpoints (images, embeddings,
/// audio, systemone).
///
/// Unlike [`auth_middleware`], this middleware authenticates the caller from
/// the `Authorization` header alone and never reads or buffers the request
/// body, so an unauthenticated client is rejected with 401 **before** any
/// expensive per-endpoint work (body buffering, remote fetches, multipart
/// parsing) can start. Model- and combo-level authorization stays in the
/// handlers, which run after the body has been parsed.
///
/// Security: without this gate, `POST /v1/images/edits` and
/// `POST /v1/images/variations` fetched attacker-supplied remote URLs before
/// authenticating (OP-01), and the JSON/multipart extractors of the media
/// endpoints buffered 32-64 MiB request bodies from unauthenticated clients
/// (OP-02).
pub async fn key_auth_middleware(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, crate::error::ApiError> {
    let (parts, body) = req.into_parts();
    let auth_result = authenticate(&state, &parts.headers).await?;
    let mut req = axum::extract::Request::from_parts(parts, body);
    if let Some(res) = auth_result {
        req.extensions_mut().insert(res);
    }
    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests;
