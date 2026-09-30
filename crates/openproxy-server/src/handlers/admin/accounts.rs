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
use std::io::Write;

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
    let provider = q.provider_id.map(ProviderId::new);
    let list = s
        .services()
        .accounts
        .list(provider.as_ref(), s.master_key().as_ref())?;
    Ok(Json(list))
}

pub async fn create_account(
    State(s): State<AppState>,
    Json(input): Json<core_admin::CreateAccountInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let provider_id = input.provider_id.clone();
    let id = s
        .services()
        .accounts
        .create(s.master_key().as_ref(), input)?;

    super::providers::spawn_background_provider_refresh(s, provider_id, Some(id.0));

    Ok(Json(serde_json::json!({ "id": id.0 })))
}

pub async fn bulk_create_accounts(
    State(s): State<AppState>,
    Json(input): Json<core_admin::BulkCreateAccountsInput>,
) -> Result<Json<core_admin::BulkCreateAccountsResponse>, ApiError> {
    let provider_id = input.provider_id.clone();
    let ids = s
        .services()
        .accounts
        .bulk_create(s.master_key().as_ref(), input)?;

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
        s.services().accounts.delete(id)?;
        Ok(Json(serde_json::json!({ "deleted": id.0 })))
    }
}

pub async fn set_account_health(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let health_str = body
        .get("health")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CoreError::Validation("missing 'health' string".into()))?;
    let health = core_accounts::HealthStatus::parse(health_str).map_err(CoreError::Validation)?;
    s.services()
        .accounts
        .set_health(AccountId::new(id), health)?;
    Ok(Json(serde_json::json!({
        "id": id,
        "health": health_str,
    })))
}

pub async fn update_account_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<core_admin::UpdateAccountApiKeyInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let acc_id = AccountId::new(id);
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
    let key = s
        .services()
        .accounts
        .get_api_key(s.master_key().as_ref(), AccountId::new(id))?;
    super::auth::audit_secret_read(&identity, "account_api_key", &format!("account:{id}"));
    Ok(Json(serde_json::json!({ "api_key": key })))
}

pub async fn update_account_label(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<core_admin::UpdateAccountLabelInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.services()
        .accounts
        .update_label(AccountId::new(id), body)?;
    Ok(Json(serde_json::json!({ "id": id })))
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

fn write_antigravity_token_file(
    payload_str: &str,
    access_token: &str,
    refresh_token: Option<&str>,
    expires_at: Option<&str>,
    email: Option<&str>,
) -> Result<std::path::PathBuf, CoreError> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .ok_or_else(|| CoreError::Validation("Could not determine home directory".into()))?;
    let gemini_dir = home.join(".gemini");
    let cli_dir = gemini_dir.join("antigravity-cli");

    std::fs::create_dir_all(&cli_dir).map_err(|e| {
        CoreError::Validation(format!("Failed to create ~/.gemini/antigravity-cli: {e}"))
    })?;

    let token_file = cli_dir.join("antigravity-oauth-token");

    let mut open_options = std::fs::OpenOptions::new();
    open_options.write(true).create(true).truncate(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_options.mode(0o600);
    }

    let mut file = open_options.open(&token_file).map_err(|e| {
        CoreError::Validation(format!("Failed to open {}: {}", token_file.display(), e))
    })?;

    file.write_all(payload_str.as_bytes()).map_err(|e| {
        CoreError::Validation(format!(
            "Failed to write to {}: {}",
            token_file.display(),
            e
        ))
    })?;

    // Gemini CLI también lee ~/.gemini/oauth_creds.json (SSH y contenedores).
    let creds_file = gemini_dir.join("oauth_creds.json");
    let expiry_ms = expires_at
        .and_then(|exp| chrono::DateTime::parse_from_rfc3339(exp).ok())
        .map_or_else(
            || (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp_millis(),
            |dt| dt.timestamp_millis(),
        );
    let creds_payload = serde_json::json!({
        "access_token": access_token,
        "refresh_token": refresh_token.unwrap_or_default(),
        "token_type": "Bearer",
        "expiry_date": expiry_ms,
        "scope": "https://www.googleapis.com/auth/userinfo.email openid https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.profile"
    });
    if let Ok(json_str) = serde_json::to_string_pretty(&creds_payload) {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        if let Ok(mut f) = opts.open(&creds_file) {
            let _ = f.write_all(json_str.as_bytes());
        }
    }

    if let Some(em) = email.filter(|s| !s.trim().is_empty()) {
        let accounts_file = gemini_dir.join("google_accounts.json");
        let accounts_payload = serde_json::json!({
            "active": em,
            "old": []
        });
        if let Ok(json_str) = serde_json::to_string_pretty(&accounts_payload) {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            if let Ok(mut f) = opts.open(&accounts_file) {
                let _ = f.write_all(json_str.as_bytes());
            }
        }
    }

    Ok(token_file)
}

pub async fn apply_account_local_cli(
    State(s): State<AppState>,
    identity: super::auth::Identity,
    DbReader(r): DbReader,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let account_id = AccountId::new(id);

    let account = core_accounts::get(&r, account_id, s.master_key().as_ref())?
        .ok_or_else(|| CoreError::AccountNotFound(account_id.0))?;

    if account.provider_id.as_str() != "antigravity" {
        return Err(CoreError::Validation(
            "Only antigravity accounts can be injected into agy-cli".into(),
        )
        .into());
    }

    let access_token =
        core_accounts::decrypt_access_token(&r, account_id, s.master_key().as_ref())?;
    let refresh_token =
        core_accounts::decrypt_refresh_token(&r, account_id, s.master_key().as_ref())?;

    let payload = serde_json::json!({
        "token": {
            "access_token": access_token,
            "token_type": "Bearer",
            "refresh_token": refresh_token.as_deref().unwrap_or_default(),
            "expiry": account.expires_at.as_deref().unwrap_or_default(),
        },
        "auth_method": "consumer"
    });

    let payload_str = serde_json::to_string(&payload)
        .map_err(|e| CoreError::Validation(format!("Failed to serialize payload: {e}")))?;

    let token_file = write_antigravity_token_file(
        &payload_str,
        &access_token,
        refresh_token.as_deref(),
        account.expires_at.as_deref(),
        account.email.as_deref(),
    )?;
    super::auth::audit_secret_read(
        &identity,
        "oauth_tokens_written_to_cli",
        &format!("account:{id} path:{}", token_file.display()),
    );

    Ok(Json(serde_json::json!({
        "success": true,
        "path": token_file.to_string_lossy(),
    })))
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
                    provider_specific: None,
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
