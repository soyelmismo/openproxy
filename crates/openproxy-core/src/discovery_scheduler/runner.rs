//! Discovery tick execution and credential resolution.

use super::config::DISCOVERY_TTL_SECONDS;
use crate::accounts;
use crate::admin;
use crate::ids::ProviderId;
use crate::models;
use crate::providers::{self, AuthType};
use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use std::sync::Arc;
use std::time::Instant;

pub(crate) async fn run_one_tick(
    provider: ProviderId,
    adapter: openproxy_adapters::adapters::ProviderAdapterEnum,
    db_pool: &Arc<DbPool>,
    master_key: &Arc<MasterKey>,
    upstream_client: &Arc<UpstreamClient>,
) {
    let started = Instant::now();

    let Some((provider_row, accounts_list)) =
        load_provider_snapshot(db_pool, &provider, master_key)
    else {
        return;
    };

    let Some(ref p_row) = provider_row else {
        tracing::debug!(
            provider = %provider,
            "discovery tick: provider row missing; skipping cycle",
        );
        return;
    };

    if !p_row.active {
        tracing::debug!(
            provider = %provider,
            "discovery tick: provider inactive; skipping cycle",
        );
        return;
    }

    let is_anonymous = matches!(p_row.auth_type, AuthType::None);
    let Some((api_key, account_label)) =
        resolve_account_credentials(db_pool, &provider, &accounts_list, is_anonymous, master_key)
            .await
    else {
        return;
    };

    let adapter = match (&adapter, &provider_row) {
        (ProviderAdapterEnum::Custom(_), Some(row)) => ProviderAdapterEnum::Custom(Box::new(
            openproxy_adapters::adapters::CustomAdapter::from_provider_row(row),
        )),
        _ => adapter,
    };

    let result = admin::refresh_models(
        db_pool,
        &provider,
        &api_key,
        &adapter,
        upstream_client,
        DISCOVERY_TTL_SECONDS,
        &account_label,
    )
    .await;

    let duration_ms = started.elapsed().as_millis();
    handle_discovery_outcome(
        db_pool,
        &provider,
        provider_row.as_ref(),
        result,
        duration_ms,
    )
    .await;

    if let Some(ref p) = provider_row
        && p.favicon_base64.is_none()
    {
        let _ =
            providers::fetch_and_cache_favicon(db_pool, &provider, &p.base_url, upstream_client)
                .await;
    }
}

fn load_provider_snapshot(
    db_pool: &Arc<DbPool>,
    provider: &ProviderId,
    master_key: &Arc<MasterKey>,
) -> Option<(Option<providers::Provider>, Vec<accounts::Account>)> {
    let w = db_pool.reader();
    let row = match providers::get(&w, provider) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                provider = %provider,
                error = %e,
                "discovery tick: failed to load provider row; skipping cycle",
            );
            return None;
        }
    };
    let accs = match accounts::list(&w, Some(provider), master_key) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(
                provider = %provider,
                error = %e,
                "discovery tick: failed to list accounts; skipping cycle",
            );
            return None;
        }
    };
    Some((row, accs))
}

async fn resolve_account_credentials(
    db_pool: &Arc<DbPool>,
    provider: &ProviderId,
    accounts_list: &[accounts::Account],
    is_anonymous: bool,
    master_key: &Arc<MasterKey>,
) -> Option<(String, String)> {
    if accounts_list.is_empty() {
        if is_anonymous {
            tracing::debug!(
                provider = %provider,
                "discovery tick: anonymous provider, no accounts; using empty api key",
            );
            return Some((String::new(), String::new()));
        }
        tracing::info!(
            provider = %provider,
            "discovery tick: provider has no accounts; skipping silently",
        );
        return None;
    }

    let Some(acc) = accounts_list.first() else {
        return Some((String::new(), String::new()));
    };

    let label = acc.label.as_deref().unwrap_or_default().to_string();
    if acc.auth_type.as_ref() == "oauth" {
        let decrypt_result = {
            let w = db_pool.reader();
            accounts::decrypt_access_token(&w, acc.id, master_key.as_ref())
        };
        match decrypt_result {
            Ok(k) => Some((k, label)),
            Err(e) => {
                tracing::warn!(
                    provider = %provider,
                    account = acc.id.0,
                    error = %e,
                    "discovery tick: failed to decrypt oauth access token; skipping cycle",
                );
                record_decrypt_failed_notification(db_pool, provider, acc.id.0, &e.to_string())
                    .await;
                None
            }
        }
    } else {
        let decrypt_result = {
            let w = db_pool.reader();
            accounts::decrypt_api_key(&w, acc.id, master_key.as_ref())
        };
        match decrypt_result {
            Ok(k) => Some((k, label)),
            Err(e) => {
                tracing::warn!(
                    provider = %provider,
                    account = acc.id.0,
                    error = %e,
                    "discovery tick: failed to decrypt api key; skipping cycle",
                );
                record_decrypt_failed_notification(db_pool, provider, acc.id.0, &e.to_string())
                    .await;
                None
            }
        }
    }
}

async fn record_decrypt_failed_notification(
    db_pool: &Arc<DbPool>,
    provider: &ProviderId,
    acc_id: i64,
    err_str: &str,
) {
    let db_pool = Arc::clone(db_pool);
    let provider_str = provider.as_str().to_string();
    let err_str = err_str.to_string();
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(notif_conn) = db_pool.open_connection() {
            let _ = crate::notifications::record_system(
                &notif_conn,
                crate::notifications::CODE_ACCOUNT_KEY_DECRYPT_FAILED,
                &format!("account_id={acc_id}: {err_str}"),
                Some(&provider_str),
                None,
            );
        }
    })
    .await;
}

async fn handle_discovery_outcome(
    db_pool: &Arc<DbPool>,
    provider: &ProviderId,
    provider_row: Option<&providers::Provider>,
    result: openproxy_types::Result<models::UpsertResult>,
    duration_ms: u128,
) {
    match result {
        Ok(upsert) => {
            tracing::info!(
                provider = %provider,
                touched = upsert.touched,
                new = upsert.new_model_ids.len(),
                duration_ms,
                "discovery tick: refresh complete",
            );

            openproxy_types::models::publish_models_refreshed(
                openproxy_types::models::ModelsRefreshedEvent {
                    provider_id: provider.clone(),
                    models_refreshed: upsert.touched,
                    new_model_ids: upsert.new_model_ids.iter().map(|id| id.0.clone()).collect(),
                    models_activated: 0,
                },
            );

            let db_pool_clone = Arc::clone(db_pool);
            let provider_clone = provider.clone();
            let keyword = provider_row.and_then(|p| p.auto_activate_keyword.clone());
            let _ = tokio::task::spawn_blocking(move || match db_pool_clone.open_connection() {
                Ok(aa_conn) => {
                    if let Err(e) = models::apply_auto_activation_with_retry(
                        &aa_conn,
                        &provider_clone,
                        keyword.as_deref(),
                    ) {
                        tracing::warn!(
                            provider = %provider_clone,
                            error = %e,
                            "discovery tick: auto-activation failed",
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %provider_clone,
                        error = %e,
                        "discovery tick: failed to open db connection for auto-activation",
                    );
                }
            })
            .await;
        }
        Err(e) => {
            tracing::warn!(
                provider = %provider,
                error = %e,
                duration_ms,
                "discovery tick: refresh failed",
            );
            let db_pool = Arc::clone(db_pool);
            let provider_str = provider.as_str().to_string();
            let err_str = e.to_string();
            let _ = tokio::task::spawn_blocking(move || {
                if let Ok(notif_conn) = db_pool.open_connection() {
                    let _ = crate::notifications::record_system(
                        &notif_conn,
                        crate::notifications::CODE_DISCOVERY_FAILED,
                        &err_str,
                        Some(&provider_str),
                        None,
                    );
                }
            })
            .await;
        }
    }
}
