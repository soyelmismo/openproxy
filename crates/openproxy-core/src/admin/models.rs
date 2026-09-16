//! Model administration service layer.

use crate::error::{CoreError, Result};
use crate::ids::{ModelId, ModelRowId, ProviderId};
use crate::models;
use crate::providers;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// List all known models for a provider, optionally filtered.
///
/// When `provider` is `None`, every row in the `models` table is returned.
pub fn list_models(conn: &Connection, provider: Option<&ProviderId>) -> Result<Vec<models::Model>> {
    match provider {
        Some(p) => Ok(models::list_all(conn)?
            .into_iter()
            .filter(|m| &m.provider_id == p)
            .collect()),
        None => models::list_all(conn),
    }
}

/// Inputs for [`create_custom_model`]. Distinct from the adapter-driven
/// [`refresh_models`] path: the operator hand-picks the `(provider_id,
/// model_id)` pair, the optional human-readable `display_name`, the
/// output `target_format` (the wire format the upstream speaks), and a
/// `ttl_seconds` cache lifetime (`0` means "never expire").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateCustomModelInput {
    pub provider_id: String,
    pub model_id: String,
    pub display_name: Option<String>,
    /// `"openai"` or `"anthropic"`. Anything else surfaces as
    /// [`CoreError::Validation`].
    pub target_format: String,
    pub ttl_seconds: i64,
    #[serde(default)]
    pub model_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateModelInput {
    pub display_name: Option<String>,
    pub model_type: Option<String>,
    pub target_format: Option<String>,
}

/// Create a hand-picked model row. See [`models::create_custom`] for the
/// SQL semantics. Returns the row id of the new (or upserted) row.
pub fn create_custom_model(conn: &Connection, input: CreateCustomModelInput) -> Result<ModelRowId> {
    let provider = ProviderId::new(input.provider_id);
    let model = ModelId::new(input.model_id);
    let target_format = models::TargetFormat::parse(&input.target_format)?;
    models::create_custom(
        conn,
        &provider,
        &model,
        input.display_name.as_deref(),
        target_format,
        input.ttl_seconds,
        input.model_type.as_deref(),
    )
}

/// Update details (display_name, model_type, target_format) for an existing model row.
pub fn update_model(conn: &Connection, id: ModelRowId, input: UpdateModelInput) -> Result<()> {
    let target_format = if let Some(tf) = input.target_format.as_deref() {
        Some(models::TargetFormat::parse(tf)?)
    } else {
        None
    };
    models::update_model_details(
        conn,
        id,
        input.display_name.as_deref(),
        input.model_type.as_deref(),
        target_format,
    )
}

/// Refresh the model list for a provider by calling the adapter's
/// `fetch_models` and upserting the results.
///
/// The caller is responsible for:
/// - resolving the right adapter for `provider`,
/// - decrypting an account's API key and passing it in plaintext,
/// - supplying the shared [`openproxy_adapters::upstream::UpstreamClient`] (the
///   hyper-based client, with per-phase timeouts driven by
///   `TimeoutProfile::ModelDiscovery`),
/// - choosing `ttl_seconds` (typically the duration after which rows
///   should be re-discovered).
///
/// On success, returns an [`models::UpsertResult`] with the touched
/// count and the list of `model_id`s that were newly inserted (i.e.
/// not present in the table for this provider before the call). On
/// failure, returns an [`CoreError`] describing the upstream or DB
/// failure.
/// ## Concurrency and Connection Safety
///
/// Verifies the provider exists in SQLite without holding the writer lock,
/// fetches models asynchronously over HTTP with no database locks or connections held,
/// and then persists the discovered models in `spawn_blocking` via the pool's writer.
pub async fn refresh_models<A: openproxy_adapters::adapters::ProviderAdapter>(
    pool: &openproxy_db::DbPool,
    provider: &ProviderId,
    api_key: &str,
    adapter: &A,
    upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
    ttl_seconds: i64,
    account_label: &str,
) -> Result<models::UpsertResult> {
    let pool_reader = pool.clone();
    let provider_clone = provider.clone();
    let provider_row = tokio::task::spawn_blocking(move || {
        let r = pool_reader.reader();
        providers::get(&r, &provider_clone)
    })
    .await
    .map_err(|e| CoreError::Internal(format!("join error: {e}")))??;

    if provider_row.is_none() {
        return Err(CoreError::ProviderNotFound(provider.to_string()));
    }

    let discovered = adapter
        .fetch_models_for_account(upstream_client, api_key, account_label)
        .await?;
    if discovered.is_empty() {
        return Err(CoreError::UpstreamConnection(format!(
            "provider {provider} returned 0 models on /models; skipping update to preserve existing catalog"
        )));
    }
    let ttl = Duration::from_secs(ttl_seconds.max(0) as u64);
    let pool_writer = pool.clone();
    let provider_clone = provider.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool_writer.writer();
        if providers::get(&conn, &provider_clone)?.is_none() {
            return Err(CoreError::ProviderNotFound(provider_clone.to_string()));
        }
        models::upsert_many(&conn, &provider_clone, &discovered, ttl)
    })
    .await
    .map_err(|e| CoreError::Internal(format!("join error: {e}")))?
}

/// Inputs for [`set_active_bulk`]. The dashboard sends one of these from
/// the "Enable all" / "Disable all" buttons; the handler does a single
/// SQL UPDATE over every non-custom row of the given provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulkToggleInput {
    pub provider_id: String,
    pub active: bool,
}

/// Bulk set `active` for all non-custom models of a provider. Atomic
/// at the SQL level: a single `UPDATE ... WHERE provider_id = ? AND
/// custom = 0` statement flips every row in one shot, so a concurrent
/// `apply_auto_activation` cannot interleave and leave the table
/// half-toggled (the writer mutex on the pool already serializes the
/// two statements against each other).
///
/// Returns the number of rows updated. Missing provider is a no-op
/// (the WHERE clause just doesn't match anything).
pub fn set_active_bulk(conn: &Connection, input: BulkToggleInput) -> Result<u64> {
    let provider = ProviderId::new(input.provider_id);
    models::set_active_bulk(conn, &provider, input.active)
}
