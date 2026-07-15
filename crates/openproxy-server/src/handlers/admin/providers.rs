use super::*;
use axum::{
    Json,
    extract::{Path, Query, State},
};
use openproxy_core::accounts as core_accounts;
use openproxy_core::admin as core_admin;
use openproxy_core::oauth::OAuthProvider;
use openproxy_core::providers as core_providers;

pub async fn list_providers(State(s): State<AppState>) -> ApiResult<Json<Vec<ProviderWithOAuth>>> {
    crate::api_try! {
        // Read-only SELECT — use the READER so the dashboard's catalog
        // polling doesn't serialize through the writer mutex.
        let r = s.db_pool().reader();
        let list = core_admin::list_providers(&r)?;
        let registry = s.oauth_provider_registry();
        let adapters = s.adapters();
        let enriched = list
            .into_iter()
            .map(|p| enrich_provider_with_oauth(p, registry.as_ref(), &adapters, &r))
            .collect();
        Ok(Json(enriched))
    }
}

pub async fn create_provider(
    State(s): State<AppState>,
    Json(input): Json<core_admin::CreateProviderInput>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        // Scope the writer guard so it is dropped BEFORE
        // rebuild_adapters re-acquires the same non-reentrant
        // parking_lot::Mutex. Holding the guard across
        // rebuild_adapters deadlocks the Tokio worker thread.
        let id = {
            let w = s.db_pool().writer();
            core_admin::create_provider(&w, input)?
        };
        // Hot-reload the in-memory adapter registry so the chat
        // pipeline can dispatch to the new provider without a
        // process restart. A failure here is logged but does NOT
        // roll back the DB write — the operator's intent to add
        // the provider has already been recorded; the next admin
        // action (or the next chat request, which already has
        // DB-fallback via `resolve_adapter`) will pick up the new
        // adapter on the next `rebuild_adapters`.
        if let Err(e) = s.rebuild_adapters() {
            tracing::warn!(
                provider_id = id.as_str(),
                error = %e,
                "failed to reload adapter registry after create_provider; \
                 chat pipeline may still fall through to DB lookup"
            );
        } else {
            tracing::info!(
                provider_id = id.as_str(),
                "reloaded adapter registry after creating provider"
            );
        }
        Ok(Json(serde_json::json!({ "id": id.as_str() })))
    }
}

pub async fn get_provider(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ProviderWithOAuth>> {
    crate::api_try! {
        // Read-only SELECT — use the READER.
        let r = s.db_pool().reader();
        let id = ProviderId::new(id);
        let provider = core_providers::get(&r, &id)?
            .ok_or_else(|| CoreError::ProviderNotFound(id.to_string()))?;
        let registry = s.oauth_provider_registry();
        let adapters = s.adapters();
        let enriched = enrich_provider_with_oauth(provider, registry.as_ref(), &adapters, &r);
        Ok(Json(enriched))
    }
}

pub async fn delete_provider(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        // Fast-fail on built-in ids before opening a writer. The
        // message is the same one the service layer would produce
        // so the dashboard's error toast is consistent regardless
        // of which path the rejection took.
        if seed::is_builtin(&id) {
            return Err(ApiError(CoreError::Validation(format!(
                "provider '{}' is a built-in and cannot be deleted. Use POST \
                 /admin/providers/{}/active with {{\"active\": false}} to \
                 deactivate it instead.",
                id, id
            ))));
        }
        // Scope the writer guard so it is dropped BEFORE
        // rebuild_adapters re-acquires the same non-reentrant
        // parking_lot::Mutex. Holding the guard across
        // rebuild_adapters deadlocks the Tokio worker thread.
        let pid = ProviderId::new(id.clone());
        {
            let w = s.db_pool().writer();
            core_admin::delete_provider(&w, &pid)?;
        }
        // Hot-reload so the chat pipeline drops the
        // `CustomAdapter` for this provider. For built-in ids we
        // never get here (the fast-fail above rejects them), so
        // this branch only fires for custom providers. A failure
        // here is logged-and-continued: the DB delete has already
        // committed, and the next admin action or DB-fallback
        // lookup will pick up the new state.
        if let Err(e) = s.rebuild_adapters() {
            tracing::warn!(
                provider_id = pid.as_str(),
                error = %e,
                "failed to reload adapter registry after delete_provider"
            );
        } else {
            tracing::info!(
                provider_id = pid.as_str(),
                "reloaded adapter registry after deleting provider"
            );
        }
        Ok(Json(serde_json::json!({ "deleted": pid.as_str() })))
    }
}

pub async fn set_provider_active(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        let active = body
            .get("active")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| CoreError::Validation("missing 'active' bool".into()))?;
        let w = s.db_pool().writer();
        let provider_id = ProviderId::new(id.clone());
        core_admin::set_provider_active(&w, &provider_id, active)?;
        Ok(Json(serde_json::json!({ "id": id, "active": active })))
    }
}

pub async fn update_provider(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<core_admin::UpdateProviderInput>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        // Scope the writer guard so it is dropped BEFORE
        // rebuild_adapters re-acquires the same non-reentrant
        // parking_lot::Mutex. Holding the guard across
        // rebuild_adapters deadlocks the Tokio worker thread.
        let provider_id = ProviderId::new(id.clone());
        {
            let w = s.db_pool().writer();
            core_admin::update_provider(&w, &provider_id, body)?;
        }
        // Hot-reload so the chat pipeline sees the updated
        // `base_url`/`auth_type`/`extra_headers` on the
        // `CustomAdapter` for this provider. See the comment on
        // `create_provider` for why we log-and-continue rather
        // than roll back.
        if let Err(e) = s.rebuild_adapters() {
            tracing::warn!(
                provider_id = id,
                error = %e,
                "failed to reload adapter registry after update_provider"
            );
        } else {
            tracing::info!(
                provider_id = id,
                "reloaded adapter registry after updating provider"
            );
        }
        Ok(Json(serde_json::json!({ "id": id })))
    }
}

async fn run_provider_refresh(
    s: AppState,
    provider_id_str: String,
    q: ProviderRefreshQuery,
) -> ApiResult<Json<serde_json::Value>> {
    let provider = ProviderId::new(provider_id_str.clone());
    let ttl_seconds = q.ttl_seconds.unwrap_or(PROVIDER_REFRESH_DEFAULT_TTL_SECS);

    // 1. Find the adapter. Check built-in adapters first, then
    //    fall back to constructing a CustomAdapter from the DB row.
    let adapter = match resolve_adapter(&s, &provider, s.adapters().as_slice()) {
        Ok(a) => a.clone(),
        Err(e) => return ApiResult::err(ApiError(e)),
    };

    // 2. Provider has no /models endpoint and no custom fetch_models
    //    implementation: no rows to refresh, return an empty result
    //    with a note rather than a 5xx.  Providers like Antigravity
    //    return None for models_url() but override fetch_models() to
    //    discover models via a different API, so we let them through
    //    here and let refresh_models() call fetch_models() directly.
    //    (The guard below is intentionally removed — fetch_models is
    //    always invoked by core_admin::refresh_models regardless.)

    // 3. Resolve a healthy/degraded account for this provider.
    let selected_account_id =
        match crate::handlers::admin::accounts::resolve_refresh_account(&s, &provider, &q).await {
            Ok((Some(account_id), _)) => Some(account_id),
            Ok((None, _)) => None,
            Err(e) => return ApiResult::err(e),
        };

    // 4. Decrypt or refresh the selected credential. Drop DB guards
    //    before awaiting refresh; adapter.fetch_models() below then
    //    receives a plaintext token/key and no SQLite guard crosses await.
    let api_key = match selected_account_id {
        Some(account_id) => {
            let account = {
                let w = s.db_pool().writer();
                match core_accounts::get(&w, account_id, s.master_key().as_ref()) {
                    Ok(Some(a)) => a,
                    Ok(None) => {
                        return ApiResult::err(ApiError(CoreError::AccountNotFound(account_id.0)));
                    }
                    Err(e) => return ApiResult::err(ApiError(e)),
                }
            };
            if account.auth_type == "oauth" {
                refresh_oauth_if_needed(&s, account, &provider).await
            } else {
                let w = s.db_pool().writer();
                match core_accounts::decrypt_api_key(&w, account_id, s.master_key().as_ref()) {
                    Ok(k) => k,
                    Err(e) => return ApiResult::err(ApiError(e)),
                }
            }
        }
        None => String::new(),
    };

    // Resolve account label for CloudFlare / label-based providers.
    let account_label = match selected_account_id {
        Some(account_id) => {
            let w = s.db_pool().writer();
            match core_accounts::get(&w, account_id, s.master_key().as_ref()) {
                Ok(Some(a)) => a.label.unwrap_or_default(),
                _ => String::new(),
            }
        }
        None => String::new(),
    };

    // 5. Open a fresh connection for the upsert. See the doc on
    //    `core_admin::refresh_models` for the `Send` rationale: an owned
    //    `Connection` is the only way to keep the outer future
    //    `Send` across an `await`.
    let conn_for_refresh = match s.db_pool().open_connection() {
        Ok(c) => c,
        Err(e) => return ApiResult::err(ApiError(e)),
    };

    // 6. Run the refresh. This is the only `await` on the upstream
    //    HTTP call; everything else is sync DB work.
    let upsert = match core_admin::refresh_models(
        conn_for_refresh,
        &provider,
        &api_key,
        &adapter,
        s.upstream_client(),
        ttl_seconds,
        &account_label,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return ApiResult::err(ApiError(e)),
    };

    // 7. Auto-activation pass. The provider may have a substring
    //    `auto_activate_keyword` set; if so, every non-custom row
    //    gets `active` flipped to whether its `model_id` contains the
    //    keyword. When no keyword is set, all non-custom rows are
    //    switched on. This is a "refresh also re-applies the rule"
    //    semantic: an operator who disables a non-custom row by hand
    //    and then triggers a refresh will see it come back on, which
    //    matches the spec's expectation.
    let activated = match (|| -> openproxy_core::Result<u64> {
        // Re-load the provider so we see the up-to-date keyword;
        // doing this in a fresh writer keeps the lock short.
        let w = s.db_pool().writer();
        let p = core_providers::get(&w, &provider)?;
        let keyword = p.and_then(|pp| pp.auto_activate_keyword);
        let keyword_ref = keyword.as_deref();
        core_models::crud::apply_auto_activation(&w, &provider, keyword_ref)
    })() {
        Ok(n) => n,
        Err(e) => return ApiResult::err(ApiError(e)),
    };

    ApiResult::ok(Json(serde_json::json!({
        "provider": provider_id_str,
        "models_refreshed": upsert.touched,
        "new_model_ids": upsert.new_model_ids,
        "models_activated": activated,
    })))
}

fn enrich_provider_with_oauth(
    p: core_providers::Provider,
    registry: &openproxy_core::oauth::OAuthProviderRegistry,
    adapters: &[openproxy_core::adapters::ProviderAdapterEnum],
    r: &rusqlite::Connection,
) -> ProviderWithOAuth {
    let flows = if p.auth_type == openproxy_core::providers::AuthType::OAuth {
        if let Some(oauth_impl) = registry.get(p.id.as_str()) {
            let mut f = Vec::new();
            match oauth_impl.flow() {
                openproxy_core::oauth::OAuthFlow::AuthorizationCodePkce => {
                    f.push("pkce".to_string());
                }
                openproxy_core::oauth::OAuthFlow::DeviceCode => {
                    f.push("device".to_string());
                }
                openproxy_core::oauth::OAuthFlow::AuthorizationCode => {
                    f.push("auth_code".to_string());
                }
            }
            Some(f)
        } else {
            None
        }
    } else {
        None
    };

    let metadata = adapters
        .iter()
        .find(|a| a.id() == &p.id)
        .map(|a| a.metadata())
        .unwrap_or_else(|| {
            // Fallback for custom providers that aren't loaded in the adapter registry yet
            let built_in = openproxy_core::providers::is_builtin(p.id.as_str());
            let mut meta = openproxy_core::providers::ProviderMetadata::custom_default();
            meta.built_in = built_in;
            meta.deletable = !built_in;
            meta
        });

    let active_models: i64 = r
        .query_row(
            "SELECT count(*) FROM models WHERE provider_id = ? AND active = 1",
            [p.id.as_str()],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let total_models: i64 = r
        .query_row(
            "SELECT count(*) FROM models WHERE provider_id = ?",
            [p.id.as_str()],
            |row| row.get(0),
        )
        .unwrap_or(0);

    ProviderWithOAuth {
        provider: p,
        oauth_flows: flows,
        metadata,
        active_models,
        total_models,
    }
}

pub async fn refresh_provider_models(
    State(s): State<AppState>,
    Path(provider_id): Path<String>,
    Query(q): Query<ProviderRefreshQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    run_provider_refresh(s, provider_id, q).await
}
