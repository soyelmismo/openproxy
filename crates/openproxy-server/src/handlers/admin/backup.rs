//! Admin HTTP handlers for backup export, validation, and restore.

use super::{ApiError, AppState, CoreError};
use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use openproxy_types::backup::{
    BackupBundle, BackupValidationSummary, RestoreOptions, RestoreReport,
};
use serde::Deserialize;
use std::sync::Arc;

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/export", axum::routing::get(export_backup_handler))
        .route("/validate", axum::routing::post(validate_backup_handler))
        .route("/restore", axum::routing::post(restore_backup_handler))
        // Per-route override (OP-14): restore bundles legitimately exceed the
        // global `request_max_body_bytes`; this admin-only, authenticated
        // endpoint keeps an explicit 50 MiB ceiling.
        .layer(axum::extract::DefaultBodyLimit::max(50 * 1024 * 1024))
}

#[derive(Debug, Default, Deserialize)]
pub struct ExportQuery {
    pub passphrase: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum BackupRequestBody {
    Envelope {
        passphrase: Option<String>,
        mode: Option<String>,
        bundle: BackupBundle,
    },
    Direct(BackupBundle),
}

impl BackupRequestBody {
    pub fn into_parts(
        self,
        header_passphrase: Option<String>,
    ) -> (BackupBundle, Option<String>, Option<String>) {
        match self {
            Self::Envelope {
                passphrase,
                mode,
                bundle,
            } => {
                let pass = passphrase.or(header_passphrase);
                (bundle, pass, mode)
            }
            Self::Direct(bundle) => (bundle, header_passphrase, None),
        }
    }
}

fn extract_header_passphrase(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-backup-passphrase")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// GET /admin/api/backup/export
///
/// Exports the database state into an `openproxy-backup.json` bundle.
/// If `?passphrase=...` is provided, encrypts the bundle with AES-256-GCM.
pub async fn export_backup_handler(
    State(s): State<AppState>,
    identity: super::auth::Identity,
    Query(q): Query<ExportQuery>,
) -> Result<Response, ApiError> {
    super::auth::audit_secret_read(&identity, "backup_bundle", "export");
    let pool = Arc::clone(s.db_pool());
    let master_key = Arc::clone(s.master_key());
    let passphrase = q.passphrase;

    let bundle = tokio::task::spawn_blocking(move || {
        let r = pool.reader();
        openproxy_core::backup::export_backup(&r, master_key.as_ref(), passphrase.as_deref())
    })
    .await
    .map_err(|e| ApiError::from(CoreError::Internal(format!("export task join failed: {e}"))))??;

    let json_bytes = serde_json::to_vec_pretty(&bundle)
        .map_err(|e| ApiError::from(CoreError::Parse(format!("serialize bundle: {e}"))))?;

    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"openproxy-backup.json\"",
        )
        .body(axum::body::Body::from(json_bytes))
        .map_err(|e| ApiError::from(CoreError::Internal(format!("build response: {e}"))))?;

    Ok(response)
}

/// POST /admin/api/backup/validate
///
/// Validates a backup bundle (and passphrase if encrypted) without altering any state.
pub async fn validate_backup_handler(
    headers: HeaderMap,
    Json(body): Json<BackupRequestBody>,
) -> Result<Json<BackupValidationSummary>, ApiError> {
    let header_pass = extract_header_passphrase(&headers);
    let (bundle, passphrase, _) = body.into_parts(header_pass);

    let summary = openproxy_core::backup::validate_backup(&bundle, passphrase.as_deref())?;

    Ok(Json(summary))
}

/// POST /admin/api/backup/restore
///
/// Restores database state from a backup bundle atomically inside a transaction.
/// Generates a safety backup of the database before applying changes.
pub async fn restore_backup_handler(
    State(s): State<AppState>,
    identity: super::auth::Identity,
    headers: HeaderMap,
    Json(body): Json<BackupRequestBody>,
) -> Result<Json<RestoreReport>, ApiError> {
    super::auth::audit_secret_read(&identity, "backup_bundle", "restore");
    let header_pass = extract_header_passphrase(&headers);
    let (bundle, passphrase, mode) = body.into_parts(header_pass);

    let pool = Arc::clone(s.db_pool());
    let master_key = Arc::clone(s.master_key());
    let opts = RestoreOptions { passphrase, mode };

    let state_clone = s.clone();
    let report = tokio::task::spawn_blocking(move || {
        let mut w = pool.writer();
        let db_path = pool.path();
        let rep = openproxy_core::backup::restore_backup(
            &mut w,
            master_key.as_ref(),
            &bundle,
            &opts,
            Some(db_path),
        )?;
        state_clone.reload_runtime_config(&w);
        Ok::<_, CoreError>(rep)
    })
    .await
    .map_err(|e| {
        ApiError::from(CoreError::Internal(format!(
            "restore task join failed: {e}"
        )))
    })??;

    let _ = s.rebuild_adapters().await;

    Ok(Json(report))
}
