use crate::ids::{AccountId, ModelId, ProviderId as CoreProviderId};
use crate::models::{DiscoveredModel, TargetFormat};
use crate::providers::{self, AuthType};
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

mod auto_activation;
mod lifecycle;

pub(crate) fn fast_config() -> super::config::DiscoverySchedulerConfig {
    super::config::DiscoverySchedulerConfig {
        interval_secs: 1,
        initial_stagger_secs: 0,
    }
}

pub(crate) fn fresh_pool() -> (Arc<DbPool>, PathBuf) {
    let pool = DbPool::test_pool_with_prefix("openproxy-discovery-test").expect("open pool");
    let path = pool.path().to_path_buf();
    (Arc::new(pool), path)
}

pub(crate) fn seed_provider_with_account(
    db_pool: &DbPool,
    master_key: &MasterKey,
    provider_id_str: &str,
) -> AccountId {
    let conn = db_pool.writer();
    let provider_id = CoreProviderId::new(provider_id_str);
    providers::create(
        &conn,
        providers::NewProvider {
            id: &provider_id,
            name: provider_id_str,
            base_url: "https://example.invalid",
            auth_type: AuthType::Bearer,
            format: providers::ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: None,
            rate_limit_scope: crate::providers::RateLimitScope::Account,
        },
    )
    .expect("seed provider");
    crate::accounts::create(
        &conn,
        &provider_id,
        Some("sk-test"),
        master_key,
        Some("test"),
        100,
        None,
    )
    .expect("seed account")
}

pub(crate) fn three_models() -> Vec<DiscoveredModel> {
    (0..3)
        .map(|i| DiscoveredModel {
            model_id: ModelId::new(format!("mock-model-{i}")),
            display_name: Some(format!("Mock Model {i}")),
            target_format: TargetFormat::Openai,
            context_length: None,
            max_output_tokens: None,
            input_modalities: None,
            output_modalities: None,
            model_type: None,
            family: None,
            capabilities: None,
        })
        .collect()
}

pub(crate) fn models_with_provider(
    pool: &DbPool,
    provider_id_str: &str,
) -> Vec<crate::models::Model> {
    let conn = pool.writer();
    let provider_id = CoreProviderId::new(provider_id_str);
    crate::models::list_all(&conn)
        .expect("list all")
        .into_iter()
        .filter(|m| m.provider_id == provider_id)
        .collect()
}

pub(crate) fn seed_three_models(
    conn: &Connection,
    provider: &crate::ids::ProviderId,
    ids: &[&str],
) {
    providers::create(
        conn,
        providers::NewProvider {
            id: provider,
            name: provider.as_str(),
            base_url: "https://example.invalid",
            auth_type: AuthType::Bearer,
            format: providers::ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: None,
            rate_limit_scope: crate::providers::RateLimitScope::Account,
        },
    )
    .expect("seed provider for upsert");
    crate::models::upsert_many(
        conn,
        provider,
        &ids.iter()
            .map(|id| DiscoveredModel {
                model_id: ModelId::new(*id),
                display_name: Some((*id).to_string()),
                target_format: TargetFormat::Openai,
                context_length: None,
                max_output_tokens: None,
                input_modalities: None,
                output_modalities: None,
                model_type: None,
                family: None,
                capabilities: None,
            })
            .collect::<Vec<_>>(),
        Duration::from_hours(1),
    )
    .expect("upsert_many seed");
}

pub(crate) fn active_ids_for(conn: &Connection, provider: &crate::ids::ProviderId) -> Vec<String> {
    crate::models::list_active(conn, provider)
        .expect("list_active")
        .into_iter()
        .map(|m| m.model_id.as_str().to_string())
        .collect()
}
