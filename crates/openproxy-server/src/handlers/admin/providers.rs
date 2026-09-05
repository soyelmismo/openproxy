use super::{
    AccountId, ApiError, AppState, CoreError, Deserialize, ProviderId, refresh_oauth_if_needed,
    resolve_adapter, seed,
};
use axum::{
    Json,
    extract::{Path, Query, State},
};
use openproxy_core::accounts as core_accounts;
use openproxy_core::admin as core_admin;
use openproxy_core::oauth::OAuthProvider;
use openproxy_core::providers as core_providers;

#[derive(serde::Serialize)]
pub struct ProviderWithOAuth {
    #[serde(flatten)]
    pub provider: core_providers::Provider,
    pub oauth_flows: Option<Vec<String>>,
    pub metadata: openproxy_core::providers::ProviderMetadata,
    pub active_models: i64,
    pub total_models: i64,
}

pub const PROVIDER_REFRESH_DEFAULT_TTL_SECS: i64 = 3_600;

/// Query string for `POST /admin/providers/:id/refresh`.
#[derive(Debug, Default, Deserialize)]
pub struct ProviderRefreshQuery {
    /// Cache TTL in seconds for the discovered rows. Defaults to 1 hour.
    pub ttl_seconds: Option<i64>,
    /// Account id whose API key will be used. Required when the provider
    /// has more than one account; otherwise the first *healthy* account
    /// wins.
    pub account_id: Option<i64>,
}

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route(
            "/",
            axum::routing::get(list_providers).post(create_provider),
        )
        .route(
            "/{id}/refresh",
            axum::routing::post(refresh_provider_models),
        )
        .route("/{id}/active", axum::routing::post(set_provider_active))
        .route(
            "/{id}",
            axum::routing::get(get_provider)
                .delete(delete_provider)
                .patch(update_provider),
        )
}

pub async fn list_providers(
    State(s): State<AppState>,
) -> Result<Json<Vec<ProviderWithOAuth>>, ApiError> {
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

/// Run a sync SQLite write against `db_pool` off the async runtime
/// worker, then trigger an in-memory adapter registry reload.
///
/// Mirrors the previous sync macro signature: the closure body
/// receives a borrowed `&Connection` via the binding `$w` and must
/// return `Result<_, CoreError>`. Internally the closure is moved
/// into a blocking task, so the body is run on a thread-pool worker
/// rather than the Tokio runtime.
///
/// The macro clones the `Arc<DbPool>` so the writer mutex can be
/// acquired off the async runtime worker (AGENTS §4.3). `JoinError`
/// is mapped to `CoreError::Internal` so the operator sees a real
/// 500 instead of a panic. On success the adapter registry is also
/// rebuilt off-thread via a separate `spawn_blocking` call.
macro_rules! with_adapter_reload {
    ($state:expr, $pid:expr, $action:literal, |$w:ident| $body:expr) => {{
        let pool = std::sync::Arc::clone($state.db_pool());
        // Clone to `String` so we don't hold a borrow on `$pid` while the
        // `move ||` closure below captures the same local by value.
        let pid_for_log = $pid.to_string();
        let join_err_msg = concat!(
            "spawn_blocking join error after ",
            $action,
            ":"
        );
        let res: Result<_, $crate::error::ApiError> = tokio::task::spawn_blocking(move || {
            let $w = pool.writer();
            $body
        })
        .await
        .map_err(|e| {
            $crate::error::ApiError(openproxy_types::CoreError::Internal(format!(
                "{join_err_msg} {e}"
            )))
        })
        .and_then(|inner| inner.map_err($crate::error::ApiError));

        let res = res?;

        if let Err(e) = $state.rebuild_adapters().await {
            tracing::warn!(
                provider_id = %pid_for_log,
                error = %e,
                concat!("failed to reload adapter registry after ", $action)
            );
        } else {
            tracing::info!(
                provider_id = %pid_for_log,
                concat!("reloaded adapter registry after ", $action)
            );
        }
        res
    }};
}

pub async fn create_provider(
    State(s): State<AppState>,
    Json(input): Json<core_admin::CreateProviderInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let is_anonymous = input.auth_type.eq_ignore_ascii_case("none");
    let provider_id = input.id.clone();
    let id = with_adapter_reload!(s, &provider_id, "create_provider", |w| {
        core_admin::create_provider(&w, input)
    });

    if is_anonymous {
        spawn_background_provider_refresh(s, id.to_string(), None);
    }

    Ok(Json(serde_json::json!({ "id": id.as_str() })))
}

pub async fn get_provider(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ProviderWithOAuth>, ApiError> {
    // Read-only SELECT — use the READER.
    let r = s.db_pool().reader();
    let id = ProviderId::new(id);
    let provider =
        core_providers::get(&r, &id)?.ok_or_else(|| CoreError::ProviderNotFound(id.to_string()))?;
    let registry = s.oauth_provider_registry();
    let adapters = s.adapters();
    let enriched = enrich_provider_with_oauth(provider, registry.as_ref(), &adapters, &r);
    Ok(Json(enriched))
}

pub async fn delete_provider(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Fast-fail on built-in ids before opening a writer. The
    // message is the same one the service layer would produce
    // so the dashboard's error toast is consistent regardless
    // of which path the rejection took.
    if seed::is_builtin(&id) {
        return Err(ApiError(CoreError::Validation(format!(
            "provider '{id}' is a built-in and cannot be deleted. Use POST \
             /admin/providers/{id}/active with {{\"active\": false}} to \
             deactivate it instead."
        ))));
    }
    let pid = ProviderId::new(&id);
    // Clone `pid` so the `spawn_blocking` closure can move its own copy;
    // the outer `pid` is needed afterwards to render the success response.
    let pid_for_body = pid.clone();
    with_adapter_reload!(s, pid.as_str(), "delete_provider", |w| {
        core_admin::delete_provider(&w, &pid_for_body)
    });
    Ok(Json(serde_json::json!({ "deleted": pid.as_str() })))
}

pub async fn set_provider_active(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let active = body
        .get("active")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| CoreError::Validation("missing 'active' bool".into()))?;
    let provider_id = ProviderId::new(&id);
    with_adapter_reload!(s, id.as_str(), "set_provider_active", |w| {
        core_admin::set_provider_active(&w, &provider_id, active)
    });
    Ok(Json(serde_json::json!({ "id": id, "active": active })))
}

pub async fn update_provider(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<core_admin::UpdateProviderInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let provider_id = ProviderId::new(&id);
    with_adapter_reload!(s, id.as_str(), "update_provider", |w| {
        core_admin::update_provider(&w, &provider_id, &body)
    });
    Ok(Json(serde_json::json!({ "id": id })))
}

pub(crate) fn spawn_background_provider_refresh(
    s: AppState,
    provider_id: String,
    account_id: Option<i64>,
) {
    tokio::spawn(async move {
        let q = ProviderRefreshQuery {
            account_id,
            ttl_seconds: None,
        };
        let _ = run_provider_refresh(s, &provider_id, q).await;
    });
}

async fn resolve_refresh_key_and_label(
    s: &AppState,
    provider: &ProviderId,
    selected_account_id: Option<AccountId>,
) -> Result<(String, String), ApiError> {
    let Some(account_id) = selected_account_id else {
        return Ok((String::new(), String::new()));
    };

    let (account, api_key_res) = {
        let r = s.db_pool().reader();
        let a = core_accounts::get(&r, account_id, s.master_key().as_ref())
            .map_err(ApiError)?
            .ok_or_else(|| ApiError(CoreError::AccountNotFound(account_id.0)))?;
        let key_res = if a.auth_type.as_ref() != "oauth" {
            core_accounts::decrypt_api_key(&r, account_id, s.master_key().as_ref())
                .map_err(ApiError)
        } else {
            Ok(String::new())
        };
        (a, key_res)
    };

    let label = account.label.as_deref().unwrap_or_default().to_string();
    let api_key = if account.auth_type.as_ref() == "oauth" {
        refresh_oauth_if_needed(s, account, provider).await
    } else {
        api_key_res?
    };

    Ok((api_key, label))
}

async fn apply_provider_auto_activation(
    s: &AppState,
    provider: &ProviderId,
) -> Result<u64, ApiError> {
    let pool = std::sync::Arc::clone(s.db_pool());
    let pid = provider.clone();
    tokio::task::spawn_blocking(move || {
        let w = pool.writer();
        let p = core_providers::get(&w, &pid)?;
        let keyword = p.and_then(|pp| pp.auto_activate_keyword);
        let n =
            openproxy_db::models::apply_auto_activation_with_retry(&w, &pid, keyword.as_deref())?;
        Ok(n)
    })
    .await
    .map_err(|e| ApiError(CoreError::Internal(format!("join error: {e}"))))?
}

fn spawn_favicon_fetch_if_needed(s: &AppState, provider: &ProviderId) {
    let pid_clone = provider.clone();
    let upstream_clone = std::sync::Arc::clone(s.upstream_client());
    let pool_clone = std::sync::Arc::clone(s.db_pool());
    tokio::spawn(async move {
        // Read the provider row on a blocking thread so the synchronous
        // SQLite call never lands on a Tokio worker.
        let p_opt = {
            let pool = std::sync::Arc::clone(&pool_clone);
            let pid = pid_clone.clone();
            tokio::task::spawn_blocking(move || {
                core_providers::get(&pool.reader(), &pid).ok().flatten()
            })
            .await
            .ok()
            .flatten()
        };
        if let Some(p) = p_opt
            && p.favicon_base64.is_none()
        {
            let _ = core_providers::fetch_and_cache_favicon(
                &pool_clone,
                &pid_clone,
                &p.base_url,
                &upstream_clone,
            )
            .await;
        }
    });
}

pub(crate) async fn run_provider_refresh(
    s: AppState,
    provider_id_str: &str,
    q: ProviderRefreshQuery,
) -> Result<Json<serde_json::Value>, ApiError> {
    let provider = ProviderId::new(provider_id_str);
    let ttl_seconds = q.ttl_seconds.unwrap_or(PROVIDER_REFRESH_DEFAULT_TTL_SECS);

    let adapter = match resolve_adapter(&s, &provider, s.adapters().as_slice()) {
        Ok(a) => a,
        Err(e) => return Err(ApiError(e)),
    };

    let (selected_account_id, _) =
        crate::handlers::admin::accounts::resolve_refresh_account(&s, &provider, &q)?;

    let (api_key, account_label) =
        resolve_refresh_key_and_label(&s, &provider, selected_account_id).await?;

    let upsert = match core_admin::refresh_models(
        s.db_pool(),
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
        Err(e) => return Err(ApiError(e)),
    };

    spawn_favicon_fetch_if_needed(&s, &provider);
    let activated = apply_provider_auto_activation(&s, &provider).await?;

    Ok(Json(serde_json::json!({
        "provider": provider_id_str,
        "models_refreshed": upsert.touched,
        "new_model_ids": upsert.new_model_ids,
        "models_activated": activated,
    })))
}

fn enrich_provider_with_oauth(
    p: core_providers::Provider,
    registry: &openproxy_core::oauth::OAuthProviderRegistry,
    adapters: &[openproxy_adapters::adapters::ProviderAdapterEnum],
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

    let metadata = adapters.iter().find(|a| a.id() == &p.id).map_or_else(
        || {
            // Fallback for custom providers that aren't loaded in the adapter registry yet
            let built_in = openproxy_core::providers::is_builtin(p.id.as_str());
            let mut meta = openproxy_core::providers::ProviderMetadata::custom_default();
            meta.built_in = built_in;
            meta.deletable = !built_in;
            meta
        },
        openproxy_adapters::ProviderAdapterEnum::metadata,
    );

    let model_counts = openproxy_db::models::count_by_provider(r, &p.id).unwrap_or_default();

    ProviderWithOAuth {
        provider: p,
        oauth_flows: flows,
        metadata,
        active_models: model_counts.active_models,
        total_models: model_counts.total_models,
    }
}

pub async fn refresh_provider_models(
    State(s): State<AppState>,
    Path(provider_id): Path<String>,
    Query(q): Query<ProviderRefreshQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    run_provider_refresh(s, &provider_id, q).await
}
