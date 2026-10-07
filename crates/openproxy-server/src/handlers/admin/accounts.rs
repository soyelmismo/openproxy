use super::{
    AccountId, ApiError, AppState, CoreError, Deserialize, ProviderId, ProviderRefreshQuery,
    Serialize,
};
use crate::extractors::DbReader;
use axum::{
    Json,
    extract::{Path, Query, State},
};
use openproxy_core::account_scanner as core_account_scanner;
use openproxy_core::accounts as core_accounts;
use openproxy_core::admin as core_admin;
use openproxy_core::providers as core_providers;

/// Query string for `GET /admin/accounts` — supports `?provider_id=...`.
#[derive(Debug, Default, Deserialize)]
pub struct AccountListQuery {
    pub provider_id: Option<String>,
}

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/", axum::routing::get(list_accounts).post(create_account))
        .route("/bulk", axum::routing::post(bulk_create_accounts))
        .route("/scan", axum::routing::post(scan_accounts))
        .route("/{id}", axum::routing::delete(delete_account))
        .route("/{id}/health", axum::routing::post(set_account_health))
        .route(
            "/{id}/api-key",
            axum::routing::get(get_account_api_key).put(update_account_api_key),
        )
        .route("/{id}/label", axum::routing::patch(update_account_label))
        .route(
            "/{id}/refresh-quota",
            axum::routing::post(refresh_account_quota),
        )
        .route(
            "/{id}/redeem-codex-reset",
            axum::routing::post(redeem_codex_reset),
        )
        .route("/{id}/codex-resets", axum::routing::get(list_codex_resets))
        .route(
            "/{id}/apply-local-cli",
            axum::routing::post(apply_account_local_cli),
        )
}

pub async fn list_accounts(
    State(s): State<AppState>,
    Query(q): Query<AccountListQuery>,
) -> Result<Json<Vec<core_accounts::Account>>, ApiError> {
    crate::error::run_blocking(move || {
        let provider = q.provider_id.map(ProviderId::new);
        let list = s
            .services()
            .accounts
            .list(provider.as_ref(), s.master_key().as_ref())?;
        Ok(Json(list))
    })
    .await
}

pub async fn create_account(
    State(s): State<AppState>,
    Json(input): Json<core_admin::CreateAccountInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let provider_id = input.provider_id.clone();
    let worker_state = s.clone();
    let id = crate::error::run_blocking(move || {
        Ok(worker_state
            .services()
            .accounts
            .create(worker_state.master_key().as_ref(), input)?)
    })
    .await?;

    super::providers::spawn_background_provider_refresh(s, provider_id, Some(id.0));

    Ok(Json(serde_json::json!({ "id": id.0 })))
}

pub async fn bulk_create_accounts(
    State(s): State<AppState>,
    Json(input): Json<core_admin::BulkCreateAccountsInput>,
) -> Result<Json<core_admin::BulkCreateAccountsResponse>, ApiError> {
    let provider_id = input.provider_id.clone();
    let worker_state = s.clone();
    let ids = crate::error::run_blocking(move || {
        Ok(worker_state
            .services()
            .accounts
            .bulk_create(worker_state.master_key().as_ref(), input)?)
    })
    .await?;

    if let Some(first_id) = ids.first() {
        super::providers::spawn_background_provider_refresh(s, provider_id, Some(first_id.0));
    }

    Ok(Json(core_admin::BulkCreateAccountsResponse {
        created: ids.len(),
        ids,
    }))
}

crate::admin_entity_action_handler! {
    pub async fn delete_account(
        State(s): State<AppState>,
        Path(id): Path<i64>,
    ) -> Result<Json<serde_json::Value>, ApiError> {
        let id = AccountId::new(id);
        s.db_pool().spawn_write(move |w| core_admin::delete_account(w, id)).await?;
        Ok(Json(serde_json::json!({ "deleted": id.0 })))
    }
}

pub async fn set_account_health(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::error::run_blocking(move || {
        let health_str = body
            .get("health")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::Validation("missing 'health' string".into()))?;
        let health =
            core_accounts::HealthStatus::parse(health_str).map_err(CoreError::Validation)?;
        s.services()
            .accounts
            .set_health(AccountId::new(id), health)?;
        Ok(Json(serde_json::json!({
            "id": id,
            "health": health_str,
        })))
    })
    .await
}

pub async fn update_account_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<core_admin::UpdateAccountApiKeyInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let acc_id = AccountId::new(id);
    let worker_state = s.clone();
    let provider_id = crate::error::run_blocking(move || {
        let s = worker_state;
        let provider_id = {
            let r = s.db_pool().reader();
            core_accounts::get(&r, acc_id, s.master_key().as_ref())
                .ok()
                .flatten()
                .map(|a| a.provider_id.to_string())
        };
        s.services()
            .accounts
            .update_api_key(s.master_key().as_ref(), acc_id, body)?;
        Ok(provider_id)
    })
    .await?;

    if let Some(pid) = provider_id {
        super::providers::spawn_background_provider_refresh(s, pid, Some(id));
    }

    Ok(Json(serde_json::json!({ "id": id })))
}

pub async fn get_account_api_key(
    State(s): State<AppState>,
    identity: super::auth::Identity,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let key = crate::error::run_blocking(move || {
        Ok(s.services()
            .accounts
            .get_api_key(s.master_key().as_ref(), AccountId::new(id))?)
    })
    .await?;
    super::auth::audit_secret_read(&identity, "account_api_key", &format!("account:{id}"));
    Ok(Json(serde_json::json!({ "api_key": key })))
}

pub async fn update_account_label(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<core_admin::UpdateAccountLabelInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::error::run_blocking(move || {
        s.services()
            .accounts
            .update_label(AccountId::new(id), body)?;
        Ok(Json(serde_json::json!({ "id": id })))
    })
    .await
}

pub async fn refresh_account_quota(
    State(s): State<AppState>,
    Path(account_id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    tracing::info!(account_id = account_id, "refresh_account_quota: start");
    let result: Result<Json<serde_json::Value>, ApiError> = async move {
        let account_id = AccountId::new(account_id);

        let adapters = s.adapters();
        let supported_providers: Vec<&str> = adapters
            .iter()
            .filter(|a| a.metadata().quota_refresh_supported)
            .map(|a| a.id().as_str())
            .collect();

        let q_opt = openproxy_core::quota_sync::refresh_single_account_quota(
            account_id,
            s.db_pool(),
            s.master_key(),
            &supported_providers,
            s.upstream_client(),
            &s.oauth_provider_registry(),
        )
        .await?;

        if let Some(q) = q_opt {
            Ok(Json(serde_json::json!({
                "account_id": account_id.0,
                "supported": true,
                "session_used": q.session_used,
                "session_limit": q.session_limit,
                "session_reset_at": q.session_reset_at,
                "weekly_used": q.weekly_used,
                "weekly_limit": q.weekly_limit,
                "weekly_reset_at": q.weekly_reset_at,
                "last_fetched_at": q.last_fetched_at,
                "fetch_error": q.fetch_error,
            })))
        } else {
            Ok(Json(serde_json::json!({
                "account_id": account_id.0,
                "supported": false,
            })))
        }
    }
    .await;
    result
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct RedeemCodexResetInput {
    pub credit_id: Option<String>,
}

pub async fn redeem_codex_reset(
    State(s): State<AppState>,
    Path(account_id): Path<i64>,
    body: Option<Json<RedeemCodexResetInput>>,
) -> Result<Json<openproxy_core::codex_resets::CodexResetResult>, ApiError> {
    tracing::info!(account_id = account_id, "redeem_codex_reset: start");
    let credit_id = body.and_then(|Json(b)| b.credit_id);
    let account_id = AccountId::new(account_id);
    let result = openproxy_core::codex_resets::redeem_codex_account_reset(
        account_id,
        credit_id.as_deref(),
        s.db_pool(),
        s.master_key(),
        s.upstream_client(),
        &s.oauth_provider_registry(),
    )
    .await
    .map_err(ApiError)?;

    Ok(Json(result))
}

pub async fn list_codex_resets(
    State(s): State<AppState>,
    Path(account_id): Path<i64>,
) -> Result<Json<openproxy_core::codex_resets::CodexResetListResponse>, ApiError> {
    let account_id = AccountId::new(account_id);
    let result = openproxy_core::codex_resets::list_codex_account_resets(
        account_id,
        s.db_pool(),
        s.master_key(),
        s.upstream_client(),
        &s.oauth_provider_registry(),
    )
    .await
    .map_err(ApiError)?;

    Ok(Json(result))
}

fn find_candidate_account_for_refresh(
    accounts: &[core_accounts::Account],
    requested_id: Option<i64>,
) -> Option<AccountId> {
    if let Some(aid) = requested_id {
        return Some(AccountId::new(aid));
    }
    accounts
        .iter()
        .find(|a| a.health_status == core_accounts::HealthStatus::Healthy)
        .or_else(|| {
            accounts
                .iter()
                .find(|a| a.health_status == core_accounts::HealthStatus::Degraded)
        })
        .map(|a| a.id)
}

pub(crate) async fn resolve_refresh_account(
    s: &AppState,
    provider: &ProviderId,
    q: &ProviderRefreshQuery,
) -> Result<(Option<AccountId>, String), ApiError> {
    let pool = std::sync::Arc::clone(s.db_pool());
    let provider = provider.clone();
    let master_key = std::sync::Arc::clone(s.master_key());
    let account_id_input = q.account_id;
    tokio::task::spawn_blocking(move || {
        let w = pool
            .try_writer_for(std::time::Duration::from_secs(5))
            .ok_or_else(|| ApiError(CoreError::Internal("writer lock timeout".into())))?;
        let provider_row = core_providers::get(&w, &provider).map_err(ApiError)?;
        let accounts_list =
            core_accounts::list(&w, Some(&provider), &master_key).map_err(ApiError)?;

        let is_auth_none = provider_row
            .as_ref()
            .is_some_and(|p| matches!(p.auth_type, core_providers::AuthType::None));

        if is_auth_none || accounts_list.is_empty() {
            return Ok((None, String::new()));
        }

        let account_id = find_candidate_account_for_refresh(&accounts_list, account_id_input);
        match account_id {
            Some(id) => Ok((Some(id), String::new())),
            None => Err(ApiError(CoreError::NoHealthyTargets(0))),
        }
    })
    .await
    .map_err(|e| ApiError(CoreError::Internal(format!("spawn failed: {e}"))))?
}

pub async fn apply_account_local_cli(
    State(s): State<AppState>,
    identity: super::auth::Identity,
    DbReader(r): DbReader,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::error::run_blocking(move || {
        let r = r.reader();
        let account_id = AccountId::new(id);

        let account = core_accounts::get(&r, account_id, s.master_key().as_ref())?
            .ok_or_else(|| CoreError::AccountNotFound(account_id.0))?;
        let mut access_token =
            core_accounts::decrypt_access_token(&r, account_id, s.master_key().as_ref())?;
        let mut refresh_token =
            core_accounts::decrypt_refresh_token(&r, account_id, s.master_key().as_ref())?;

        let provider = account.provider_id.as_str();

        if let Some(disc) = core_account_scanner::check_local_cli_updated_tokens(
            provider,
            &account,
            Some(&access_token),
            refresh_token.as_deref(),
        ) && disc.refresh_token.as_deref() != refresh_token.as_deref()
        {
            tracing::info!(
                account = account_id.0,
                provider = provider,
                "apply_local_cli: local CLI has newer refresh token on disk; updating DB"
            );
            let w = s
                .db_pool()
                .try_writer_for(std::time::Duration::from_secs(5))
                .ok_or_else(|| CoreError::Internal("writer lock timeout".into()))?;
            core_accounts::store_oauth_tokens(
                &w,
                account_id,
                s.master_key().as_ref(),
                core_accounts::StoreOAuthTokensParams {
                    access_token: &disc.access_token,
                    refresh_token: disc.refresh_token.as_deref(),
                    token_type: "Bearer",
                    expires_at: disc.expires_at.as_deref(),
                    scope: None,
                    provider_specific: disc.oauth_provider_specific.as_deref(),
                    email: disc.email.as_deref(),
                },
            )?;
            drop(w);
            access_token = disc.access_token;
            refresh_token = disc.refresh_token;
        }

        let token_file = if provider == "antigravity" {
            core_account_scanner::write_antigravity_credentials(
                core_account_scanner::AntigravityWriteOptions {
                    access_token: &access_token,
                    refresh_token: refresh_token.as_deref(),
                    expires_at: account.expires_at.as_deref(),
                    email: account.email.as_deref(),
                },
            )?
        } else if provider == "claude-code" || provider == "claude" {
            let (account_uuid, org_uuid, sub_type, rate_tier) = account
                .oauth_provider_specific
                .as_deref()
                .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
                .map_or((None, None, None, None), |val| {
                    let acc_uuid = val
                        .get("account_uuid")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    let org_uuid = val
                        .get("organization_uuid")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    let sub = val
                        .get("subscription_type")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    let tier = val
                        .get("rate_limit_tier")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    (acc_uuid, org_uuid, sub, tier)
                });

            let inferred_sub_type = sub_type.as_deref().or_else(|| {
                account.quota_plan_name.as_deref().and_then(|p| {
                    let p_low = p.to_lowercase();
                    if p_low.contains("pro") {
                        Some("pro")
                    } else if p_low.contains("max") {
                        Some("max")
                    } else if p_low.contains("team") {
                        Some("team")
                    } else {
                        None
                    }
                })
            });

            core_account_scanner::write_claude_code_credentials(
                core_account_scanner::ClaudeCodeWriteOptions {
                    access_token: &access_token,
                    refresh_token: refresh_token.as_deref(),
                    expires_at: account.expires_at.as_deref(),
                    email: account.email.as_deref(),
                    account_uuid: account_uuid.as_deref(),
                    org_uuid: org_uuid.as_deref(),
                    subscription_type: inferred_sub_type,
                    rate_limit_tier: rate_tier.as_deref(),
                },
            )?
        } else {
            return Err(CoreError::Validation(
                "Only antigravity and claude-code accounts can be injected into local CLI".into(),
            )
            .into());
        };
        super::auth::audit_secret_read(
            &identity,
            "oauth_tokens_written_to_cli",
            &format!("account:{id} path:{}", token_file.display()),
        );

        Ok(Json(serde_json::json!({
            "success": true,
            "path": token_file.to_string_lossy(),
        })))
    })
    .await
}

// ==========
// GAP-7: POST /admin/api/accounts/scan (docs/specs/antigravity-gaps-p3.md §7)
// ==========

#[derive(Debug, Default, Deserialize)]
pub struct ScanQuery {
    #[serde(default)]
    pub auto_import: bool,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Serialize)]
pub struct ScanResponse {
    pub scanned: Vec<core_account_scanner::DiscoveredAccount>,
    pub imported: Vec<ImportSummary>,
}

#[derive(Debug, Serialize)]
pub struct ImportSummary {
    pub provider_id: String,
    pub label: String,
    pub account_id: AccountId,
}

/// `POST /admin/api/accounts/scan`
///
/// Descubre credenciales OAuth de CLIs locales (hoy: solo antigravity-cli) y,
/// opcionalmente, las importa como accounts.
///
/// Body (todos los campos opcionales):
/// * `auto_import: bool` — si true, crea accounts vía
///   `services().accounts.create` y dispara `spawn_background_provider_refresh`.
/// * `dry_run: bool`     — si true (o `auto_import=false`), devuelve la lista de
///   discoveries sin tocar la DB.
///
/// AGENTS §4.3: el scan offline corre en `spawn_blocking`; el writer guard de
/// SQLite se libera antes de cualquier `.await`.
pub async fn scan_accounts(
    State(s): State<AppState>,
    identity: super::auth::Identity,
    Json(q): Json<ScanQuery>,
) -> Result<Json<ScanResponse>, ApiError> {
    super::auth::audit_secret_read(&identity, "host_cli_oauth_tokens_scan", "accounts:scan");
    // Scan offline, sin tomar ningún lock de DB.
    let discovered = tokio::task::spawn_blocking(core_account_scanner::scan_external_accounts)
        .await
        .map_err(|e| ApiError(CoreError::Internal(format!("scan join error: {e}"))))?;

    // dry_run o sin auto_import: devolver sin tocar la DB.
    if q.dry_run || !q.auto_import {
        // Security (OP-18): metadata only — strip the raw OAuth tokens from
        // the response; they are imported encrypted via auto_import instead.
        let scanned = discovered.iter().map(|d| d.redacted()).collect();
        return Ok(Json(ScanResponse {
            scanned,
            imported: Vec::new(),
        }));
    }

    // auto_import: mismo path OAuth que `resolve_or_create_oauth_account`
    // (handlers/admin/oauth.rs).
    let mut imported = Vec::with_capacity(discovered.len());
    for entry in discovered {
        let pool = std::sync::Arc::clone(s.db_pool());
        let master_key = std::sync::Arc::clone(s.master_key());
        let accounts_service = std::sync::Arc::clone(&s.services().accounts);
        let entry_clone = entry.clone();

        let id = tokio::task::spawn_blocking(move || -> Result<AccountId, CoreError> {
            let id = accounts_service.create(
                master_key.as_ref(),
                core_admin::CreateAccountInput {
                    provider_id: entry_clone.provider_id.clone(),
                    api_key: None, // OAuth: el token va en store_oauth_tokens
                    label: Some(entry_clone.label.clone()),
                    priority: Some(100),
                    extra_config_json: None,
                },
            )?;

            let w = pool
                .try_writer_for(std::time::Duration::from_secs(5))
                .ok_or_else(|| CoreError::Internal("writer lock timeout".into()))?;
            core_accounts::store_oauth_tokens(
                &w,
                id,
                &master_key,
                core_accounts::StoreOAuthTokensParams {
                    access_token: &entry_clone.access_token,
                    refresh_token: entry_clone.refresh_token.as_deref(),
                    token_type: "Bearer",
                    expires_at: None,
                    scope: None,
                    provider_specific: entry_clone.oauth_provider_specific.as_deref(),
                    email: entry_clone.email.as_deref(),
                },
            )?;

            Ok(id)
        })
        .await
        .map_err(|e| ApiError(CoreError::Internal(format!("spawn failed: {e}"))))??;

        // Refresca metadata/quota del provider en background — idéntico a
        // `create_account` (accounts.rs:62).
        super::providers::spawn_background_provider_refresh(
            s.clone(),
            entry.provider_id.clone(),
            Some(id.0),
        );

        imported.push(ImportSummary {
            provider_id: entry.provider_id,
            label: entry.label,
            account_id: id,
        });
    }

    Ok(Json(ScanResponse {
        scanned: Vec::new(),
        imported,
    }))
}
