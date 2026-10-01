use crate::{error::ApiError, state::AppState};
use axum::{extract::State, http::HeaderMap};
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
        match &self.key.allowed_combos {
            Some(allowed) if !allowed.is_empty() => allowed.contains(&combo_id),
            _ => true,
        }
    }

    pub fn is_provider_allowed(&self, provider_id: &str) -> bool {
        if let Some(blacklisted) = &self.key.blacklisted_providers
            && blacklisted.iter().any(|p| p == provider_id || p == "*")
        {
            return false;
        }
        true
    }

    pub fn is_model_allowed(&self, model: &str, provider_id: Option<&str>) -> bool {
        let (prov_from_model, bare_model) = model
            .split_once('/')
            .map_or((None, model), |(p, rest)| (Some(p), rest));

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

        if let Some(allowed) = &self.key.allowed_models
            && !allowed.is_empty()
            && !allowed
                .iter()
                .any(|m| matches_any_model_pattern(m, model, bare_model, full_id.as_deref()))
        {
            return false;
        }

        if let Some(blacklisted_provs) = &self.key.blacklisted_providers
            && is_provider_blacklisted(blacklisted_provs, provider_id, prov_from_model)
        {
            return false;
        }

        if let Some(blacklisted) = &self.key.blacklisted_models
            && blacklisted
                .iter()
                .any(|b| matches_any_model_pattern(b, model, bare_model, full_id.as_deref()))
        {
            return false;
        }

        true
    }
}

fn pattern_matches(spec: &str, candidate: &str) -> bool {
    if spec == "*" || spec == candidate {
        return true;
    }
    if spec
        .strip_suffix('*')
        .is_some_and(|p| candidate.starts_with(p))
    {
        return true;
    }
    let Some(suffix) = spec.strip_prefix('*') else {
        return false;
    };
    candidate.ends_with(suffix)
}

fn matches_any_model_pattern(
    pattern: &str,
    model: &str,
    bare_model: &str,
    full_id: Option<&str>,
) -> bool {
    if pattern_matches(pattern, model) {
        return true;
    }
    if pattern_matches(pattern, bare_model) {
        return true;
    }
    let Some(f) = full_id else {
        return false;
    };
    pattern_matches(pattern, f)
}

fn is_provider_blacklisted(
    blacklisted_provs: &[String],
    provider_id: Option<&str>,
    prov_from_model: Option<&str>,
) -> bool {
    if let Some(p) = provider_id
        && blacklisted_provs.iter().any(|bp| bp == p || bp == "*")
    {
        return true;
    }
    if let Some(p) = prov_from_model
        && blacklisted_provs.iter().any(|bp| bp == p || bp == "*")
    {
        return true;
    }
    false
}

fn extract_bearer_or_api_key_token(headers: &HeaderMap) -> Option<&str> {
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

async fn check_anonymous_fallback(state: &AppState) -> Result<Option<ValidatedApiToken>, ApiError> {
    let active = state
        .db_pool()
        .spawn_read(core_api_keys::count_active)
        .await
        .map_err(|e| {
            tracing::error!(%e, "db error counting active keys");
            ApiError(CoreError::Auth("missing api key".into()))
        })?;
    if active == 0 && state.config().server.allow_anonymous {
        tracing::debug!(
            target: "openproxy::auth",
            "anonymous request admitted (no active api keys configured)"
        );
        return Ok(None);
    }
    Err(ApiError(CoreError::Auth("missing api key".into())))
}

/// Resolve the caller from the `Authorization` header.
pub(crate) async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<ValidatedApiToken>, ApiError> {
    let Some(token) = extract_bearer_or_api_key_token(headers) else {
        return check_anonymous_fallback(state).await;
    };

    let key = verify_key_credentials(state, token, "chat").await?;

    Ok(Some(ValidatedApiToken {
        key_id: key.id,
        key,
    }))
}

pub(crate) fn validate_key_record(key: &ApiKey, required_scope: &str) -> Result<(), ApiError> {
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
    Ok(())
}

/// Validate an API key credential against active keys
pub(crate) async fn verify_key_credentials(
    state: &AppState,
    token: &str,
    required_scope: &str,
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

    validate_key_record(&key, required_scope)?;

    // Security (OP-15): `touch_last_used` is throttled in SQL to one write per
    // LAST_USED_THROTTLE_SECS, but the old code contended for the pool's
    // SINGLE writer mutex on every authenticated request — at high RPS a
    // single key monopolized the writer and delayed usage tracking and admin
    // operations. The fetched row already tells us whether a stamp is due, so
    // only take the writer when it actually is.
    if last_used_needs_stamp(key.last_used_at.as_ref()) {
        let key_id = key.id;
        if let Err(e) = state
            .db_pool()
            .spawn_write(move |w| core_api_keys::touch_last_used(w, key_id))
            .await
        {
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
) -> Result<(), ApiError> {
    let model_name = model_name.to_owned();
    let Ok(openproxy_core::routing::RoutingPlan::Combo { combo_id, .. }) = state
        .db_pool()
        .spawn_read(move |r| openproxy_core::routing::resolve(r, &model_name))
        .await
    else {
        return Ok(());
    };

    if let Some(auth) = auth
        && !auth.is_combo_allowed(combo_id.0)
    {
        return Err(ApiError(CoreError::Auth(
            "combo not allowed for this key".into(),
        )));
    }
    Ok(())
}

/// Authenticate the request against active API keys and verify model/combo authorization.
pub(crate) async fn authenticate_and_authorize_model(
    state: &AppState,
    headers: &HeaderMap,
    model_name: &str,
) -> Result<Option<ApiKeyId>, ApiError> {
    let auth_result = authenticate(state, headers).await?;

    if let Some(token) = &auth_result
        && !token.is_model_allowed(model_name, None)
    {
        return Err(ApiError(CoreError::Auth(format!(
            "model '{model_name}' not allowed or blacklisted for this key"
        ))));
    }

    verify_combo_authorization(state, auth_result.as_ref(), model_name).await?;
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

    if parsed.model.is_empty() && parts.uri.path().starts_with("/v1/images") {
        parsed.model = "dall-e-2".to_string();
    }

    let requested_model = &parsed.model;
    if let Some(token) = &auth_result {
        if !token.key.scopes.iter().any(|s| s == "chat") {
            return Err(ApiError(CoreError::Auth(
                "api key lacks required scope".into(),
            )));
        }

        if !token.is_model_allowed(requested_model, None) {
            return Err(ApiError(CoreError::Auth(format!(
                "model '{requested_model}' not allowed or blacklisted for this key"
            ))));
        }

        verify_combo_authorization(&state, Some(token), requested_model).await?;
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
mod tests {
    use super::*;
    use openproxy_adapters::adapters;
    use openproxy_core::AppConfig;
    use openproxy_db as core_db;
    use openproxy_db::MasterKey;
    use parking_lot::RwLock;
    use std::sync::Arc;

    async fn make_test_state() -> AppState {
        tokio::task::spawn_blocking(|| {
            let pool =
                core_db::DbPool::test_pool_with_prefix("openproxy-auth-test").expect("open pool");
            let db_pool = Arc::new(pool);
            let master_key = Arc::new(MasterKey::generate().unwrap());
            let adapters = Arc::new(RwLock::new(Arc::new(
                Vec::<adapters::ProviderAdapterEnum>::new(),
            )));
            let mut config = AppConfig::default();
            config.server.allow_anonymous = false;
            AppState::for_test(config, db_pool, master_key, adapters)
        })
        .await
        .expect("make_test_state join")
    }

    #[test]
    fn test_last_used_needs_stamp_unit() {
        assert!(last_used_needs_stamp(None));
        assert!(last_used_needs_stamp(Some(&"malformed_date".to_string())));

        let old_time = (chrono::Utc::now() - chrono::Duration::seconds(120))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        assert!(last_used_needs_stamp(Some(&old_time)));

        let recent_time = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        assert!(!last_used_needs_stamp(Some(&recent_time)));
    }

    #[tokio::test]
    async fn test_verify_key_credentials_stamps_and_throttles() {
        let state = make_test_state().await;

        // Install an UPDATE trigger on `api_keys` to count real disk/SQL writes to `last_used_at`
        state
            .db_pool()
            .spawn_write(|conn| {
                conn.execute_batch(
                    "CREATE TABLE test_touch_counter (count INTEGER NOT NULL);\
                     INSERT INTO test_touch_counter (count) VALUES (0);\
                     CREATE TRIGGER test_count_last_used_touch AFTER UPDATE OF last_used_at ON api_keys \
                     BEGIN \
                         UPDATE test_touch_counter SET count = count + 1; \
                     END;",
                )
                .map_err(openproxy_db::error::map_db_error)
            })
            .await
            .expect("setup touch counter trigger");

        let input = core_api_keys::CreateApiKeyInput {
            label: Some("test-key".into()),
            scopes: vec!["chat".into()],
            ..Default::default()
        };
        let (key_row, token) = state
            .db_pool()
            .spawn_write(move |conn| core_api_keys::create(conn, input, "test_admin"))
            .await
            .expect("create key");
        assert!(key_row.last_used_at.is_none());

        // Initial trigger count must be 0
        let initial_count: i64 = state
            .db_pool()
            .spawn_read(|conn| {
                conn.query_row("SELECT count FROM test_touch_counter", [], |row| row.get(0))
                    .map_err(openproxy_db::error::map_db_error)
            })
            .await
            .expect("read initial count");
        assert_eq!(initial_count, 0);

        // First verification: stamps last_used_at via awaited spawn_write and caches it
        let verified = verify_key_credentials(&state, &token, "chat")
            .await
            .expect("first verification succeeds");
        assert_eq!(verified.id, key_row.id);

        // Verify the DB row has the new timestamp
        let key_id = key_row.id;
        let in_db = state
            .db_pool()
            .spawn_read(move |conn| {
                core_api_keys::get_by_id(conn, key_id)?
                    .ok_or_else(|| CoreError::Internal("key not found in db".into()))
            })
            .await
            .expect("db read");
        assert!(
            in_db.last_used_at.is_some(),
            "DB row must have last_used_at updated"
        );

        // Verify trigger count is exactly 1
        let count_after_first: i64 = state
            .db_pool()
            .spawn_read(|conn| {
                conn.query_row("SELECT count FROM test_touch_counter", [], |row| row.get(0))
                    .map_err(openproxy_db::error::map_db_error)
            })
            .await
            .expect("read count after first verify");
        assert_eq!(
            count_after_first, 1,
            "First verification must trigger exactly 1 write"
        );

        // Second verification immediately: hits cached key and throttle skips DB write
        let cached = verify_key_credentials(&state, &token, "chat")
            .await
            .expect("second verification succeeds");
        assert_eq!(cached.id, key_row.id);
        assert!(cached.last_used_at.is_some());

        // Verify trigger count is STILL 1, proving NO second write occurred!
        let count_after_second: i64 = state
            .db_pool()
            .spawn_read(|conn| {
                conn.query_row("SELECT count FROM test_touch_counter", [], |row| row.get(0))
                    .map_err(openproxy_db::error::map_db_error)
            })
            .await
            .expect("read count after second verify");
        assert_eq!(
            count_after_second, 1,
            "Second verification inside throttle window must NOT perform any second write"
        );
    }

    #[tokio::test]
    async fn test_verify_key_credentials_scope_and_inactive() {
        let state = make_test_state().await;

        let input = core_api_keys::CreateApiKeyInput {
            label: Some("scope-test".into()),
            scopes: vec!["chat".into()],
            ..Default::default()
        };
        let (key_row, token) = state
            .db_pool()
            .spawn_write(move |conn| core_api_keys::create(conn, input, "test_admin"))
            .await
            .expect("create test key");

        // Missing required scope
        let err = verify_key_credentials(&state, &token, "manage")
            .await
            .unwrap_err();
        match err.0 {
            CoreError::Auth(msg) => assert!(msg.contains("lacks required scope")),
            other => panic!("expected auth error, got: {other:?}"),
        }

        // Revoke the key via spawn_write
        let key_id = key_row.id;
        state
            .db_pool()
            .spawn_write(move |conn| core_api_keys::revoke(conn, key_id))
            .await
            .expect("revoke key");

        // Invalidate cache
        state.invalidate_api_key_cache(None);

        // Inactive key rejected
        let err = verify_key_credentials(&state, &token, "chat")
            .await
            .unwrap_err();
        match err.0 {
            CoreError::Auth(msg) => assert!(msg.contains("revoked or inactive")),
            other => panic!("expected auth error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_verify_key_credentials_survives_shutdown() {
        let state = make_test_state().await;
        let input = core_api_keys::CreateApiKeyInput {
            label: Some("shutdown-test".into()),
            scopes: vec!["chat".into()],
            ..Default::default()
        };
        let (_key_row, token) = state
            .db_pool()
            .spawn_write(move |conn| core_api_keys::create(conn, input, "test_admin"))
            .await
            .expect("create test key");

        state
            .shutdown_usage_worker()
            .await
            .expect("shutdown usage worker");

        let verified = verify_key_credentials(&state, &token, "chat")
            .await
            .expect("verification succeeds during shutdown");
        assert!(verified.last_used_at.is_some());
    }
}
