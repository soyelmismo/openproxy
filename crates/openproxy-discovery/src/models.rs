//! Persistent model registry, owning the `models` table (mvp-spec §8) for the
//! discovery loop, the `/v1/models` admin endpoint and request routing.
//!
//! # Visibility semantic: presence-in-last-refresh
//!
//! A row is live iff the last successful refresh of its provider listed it. The
//! only hot-path filter is `active = 1` in [`list_active`] / [`list_active_all`].
//! `expires_at` stays in the schema for diagnostics but is no longer a gate: the
//! [`crate::discovery_scheduler`] calls [`upsert_many`] every tick, and an upsert
//! whose `discovered` list omits a model hard-deletes its non-custom row. "Expired"
//! therefore means the upstream stopped listing the model.
//!
//! Hard delete over an `expires_at` filter:
//!   - the registry mirrors upstream truth with no `datetime('now')` math at query
//!     time;
//!   - hand-curated `custom = 1` rows survive, since the delete is gated on
//!     `custom = 0`;
//!   - `combo_targets` rows pointing at a vanished model are orphaned harmlessly,
//!     since routing already filters on `model_row_id IN (live models)`.
//!
//! # Manual cleanup: `mark_expired`
//!
//! [`mark_expired`] is a manual orphan-row cleanup (provider deleted while models
//! still pointed at it, or a crash mid-upsert), not part of the hot path: that role
//! belongs to [`upsert_many`]'s hard delete. Its threshold is intentionally long
//! (>7 days) so it never races the scheduler. A NULL `expires_at` is a legitimate
//! "no expiry set" state (e.g. `create_custom` with `ttl_seconds = 0`) and is
//! never deleted.
//!
//! Submodules: `crud` holds the SQL operations behind [`SqliteModelRepository`],
//! `sync` the diff/upsert/notification path, `repository` the trait, and
//! `discovery` the [`DiscoveryService`].
pub use openproxy_types::{
    DiscoveredModel, Model, ModelsRefreshedEvent, TargetFormat, UpsertResult,
    publish_models_refreshed,
};

pub mod discovery;
pub mod sync;

#[cfg(test)]
mod tests;

pub use openproxy_db::models::{
    ModelRepository, SqliteModelRepository, apply_auto_activation,
    apply_auto_activation_with_retry, create_custom, delete, find_active_by_name,
    find_active_by_provider_and_name, get_by_row_id, get_by_row_ids, list_active, list_active_all,
    list_all, mark_expired, set_active, set_active_bulk, set_test_status, update_model_details,
    update_model_type,
};

pub use discovery::DiscoveryService;

pub static MODELS_REFRESHED_SENDER: std::sync::OnceLock<
    tokio::sync::broadcast::Sender<ModelsRefreshedEvent>,
> = std::sync::OnceLock::new();

pub fn init_models_refreshed_broadcast() -> tokio::sync::broadcast::Sender<ModelsRefreshedEvent> {
    let (tx, _rx) = tokio::sync::broadcast::channel(64);
    let _ = MODELS_REFRESHED_SENDER.set(tokio::sync::broadcast::Sender::clone(&tx));
    let _ = openproxy_types::models::MODELS_REFRESHED_PUBLISHER
        .set(Box::new(publish_models_refreshed_global));
    tx
}

fn publish_models_refreshed_global(event: ModelsRefreshedEvent) {
    if let Some(tx) = MODELS_REFRESHED_SENDER.get() {
        let _ = tx.send(event);
    }
}

pub fn upsert_many(
    conn: &rusqlite::Connection,
    provider: &crate::ids::ProviderId,
    discovered: &[DiscoveredModel],
    ttl: std::time::Duration,
) -> crate::error::Result<UpsertResult> {
    let diff = sync::compute_diff(conn, provider, discovered)?;
    let (upsert_result, events) =
        sync::execute_sync_transaction(conn, provider, discovered, &diff, ttl)?;
    sync::broadcast_notifications(conn, &events);
    Ok(upsert_result)
}

pub async fn refresh_models<A: openproxy_adapters::adapters::ProviderAdapter>(
    pool: &openproxy_db::DbPool,
    provider: &crate::ids::ProviderId,
    api_key: &str,
    adapter: &A,
    upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
    ttl_seconds: i64,
    account_label: &str,
) -> crate::error::Result<UpsertResult> {
    let pool_reader = pool.clone();
    let provider_clone = provider.clone();
    let provider_row = tokio::task::spawn_blocking(move || {
        let r = pool_reader.reader();
        crate::providers::get(&r, &provider_clone)
    })
    .await
    .map_err(|e| crate::error::CoreError::Internal(format!("join error: {e}")))??;

    if provider_row.is_none() {
        return Err(crate::error::CoreError::ProviderNotFound(
            provider.to_string(),
        ));
    }

    let discovered = adapter
        .fetch_models_for_account(upstream_client, api_key, account_label)
        .await?;
    if discovered.is_empty() {
        return Err(crate::error::CoreError::UpstreamConnection(format!(
            "provider {provider} returned 0 models on /models; skipping update to preserve existing catalog"
        )));
    }
    let ttl = std::time::Duration::from_secs(ttl_seconds.max(0) as u64);
    let pool_writer = pool.clone();
    let provider_clone = provider.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool_writer.writer();
        if crate::providers::get(&conn, &provider_clone)?.is_none() {
            return Err(crate::error::CoreError::ProviderNotFound(
                provider_clone.to_string(),
            ));
        }
        upsert_many(&conn, &provider_clone, &discovered, ttl)
    })
    .await
    .map_err(|e| crate::error::CoreError::Internal(format!("join error: {e}")))?
}
