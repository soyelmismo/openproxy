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
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let key = s
        .services()
        .accounts
        .get_api_key(s.master_key().as_ref(), AccountId::new(id))?;
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

pub(crate) fn resolve_refresh_account(
    s: &AppState,
    provider: &ProviderId,
    q: &ProviderRefreshQuery,
) -> Result<(Option<AccountId>, String), ApiError> {
    let w = s.db_pool().writer();
    let provider_row = core_providers::get(&w, provider).map_err(ApiError)?;
    let accounts_list =
        core_accounts::list(&w, Some(provider), s.master_key().as_ref()).map_err(ApiError)?;

    let is_auth_none = provider_row
        .as_ref()
        .is_some_and(|p| matches!(p.auth_type, core_providers::AuthType::None));

    if is_auth_none || accounts_list.is_empty() {
        return Ok((None, String::new()));
    }

    let account_id = find_candidate_account_for_refresh(&accounts_list, q.account_id);
    match account_id {
        Some(id) => Ok((Some(id), String::new())),
        None => Err(ApiError(CoreError::NoHealthyTargets(0))),
    }
}

fn write_antigravity_token_file(payload_str: &str) -> Result<std::path::PathBuf, CoreError> {
    let cli_dir = dirs::home_dir()
        .ok_or_else(|| CoreError::Validation("Could not determine home directory".into()))?
        .join(".gemini")
        .join("antigravity-cli");

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

    Ok(token_file)
}

pub async fn apply_account_local_cli(
    State(s): State<AppState>,
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
            "refresh_token": refresh_token.unwrap_or_default(),
            "expiry": account.expires_at.unwrap_or_default(),
        },
        "auth_method": "consumer"
    });

    let payload_str = serde_json::to_string(&payload)
        .map_err(|e| CoreError::Validation(format!("Failed to serialize payload: {e}")))?;

    let token_file = write_antigravity_token_file(&payload_str)?;

    Ok(Json(serde_json::json!({
        "success": true,
        "path": token_file.to_string_lossy(),
    })))
}

// ==========
// GAP-7: POST /admin/api/accounts/scan
// (docs/specs/antigravity-gaps-p3.md §7)
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
/// * `sources:     Vec<String>` — reservado para follow-up multi-provider;
///   hoy solo se escanea antigravity-cli.
/// * `auto_import: bool`        — si true, crea accounts vía
///   `services().accounts.create` y dispara
///   `spawn_background_provider_refresh`.
/// * `dry_run:     bool`        — si true (o `auto_import=false`), devuelve
///   la lista de discoveries sin tocar la DB.
///
/// AGENTS §4.3: el scan offline corre en `spawn_blocking`; el writer guard
/// de SQLite se libera antes de cualquier `.await`.
pub async fn scan_accounts(
    State(s): State<AppState>,
    Json(q): Json<ScanQuery>,
) -> Result<Json<ScanResponse>, ApiError> {
    // 1. Scan offline (no DB lock tomado).
    let discovered = tokio::task::spawn_blocking(core_account_scanner::scan_external_accounts)
        .await
        .map_err(|e| ApiError(CoreError::Internal(format!("scan join error: {e}"))))?;

    // 2. dry_run / sin auto_import: devolver sin tocar DB.
    if q.dry_run || !q.auto_import {
        return Ok(Json(ScanResponse {
            scanned: discovered,
            imported: Vec::new(),
        }));
    }

    // 3. auto_import: crear accounts vía el mismo path OAuth que
    //    `resolve_or_create_oauth_account` (handlers/admin/oauth.rs).
    let mut imported = Vec::with_capacity(discovered.len());
    for entry in discovered {
        let id = s
            .services()
            .accounts
            .create(
                s.master_key().as_ref(),
                core_admin::CreateAccountInput {
                    provider_id: entry.provider_id.clone(),
                    api_key: None, // OAuth: el token va en store_oauth_tokens
                    label: Some(entry.label.clone()),
                    priority: Some(100),
                    extra_config_json: None,
                },
            )?;

        // Almacena los tokens OAuth leídos del archivo (espejo del path
        // OAuth post-exchange). El writer guard se libera al salir del
        // bloque (AGENTS §4.3: jamás retener locks a través de `.await`).
        {
            let w = s.db_pool().writer();
            core_accounts::store_oauth_tokens(
                &w,
                id,
                s.master_key().as_ref(),
                core_accounts::StoreOAuthTokensParams {
                    access_token: &entry.access_token,
                    refresh_token: entry.refresh_token.as_deref(),
                    token_type: "Bearer",
                    expires_at: None,
                    scope: None,
                    provider_specific: None,
                    email: entry.email.as_deref(),
                },
            )?;
        }

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
