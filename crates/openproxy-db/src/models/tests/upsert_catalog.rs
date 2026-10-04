use super::*;
use crate::models::upsert::{DISCOVERED_MODEL_UPSERT_SQL, prune_obsolete_models};
use openproxy_types::{DiscoveredModel, ModelId, TargetFormat};
use rusqlite::params;

fn test_discovered(id: &str) -> DiscoveredModel {
    DiscoveredModel {
        model_id: ModelId::new(id),
        display_name: Some(id.to_string()),
        target_format: TargetFormat::Openai,
        context_length: None,
        max_output_tokens: None,
        input_modalities: None,
        output_modalities: None,
        model_type: None,
        family: None,
        capabilities: None,
    }
}

#[test]
fn test_discovered_model_upsert_sql_defaults_missing_model_type_to_chat() {
    let (pool, _path) = fresh_pool();
    let conn = pool.open_connection().expect("open connection");
    let provider = CoreProviderId::new("prov_default_type");
    seed_provider(&conn, &provider);
    seed_models(&conn, &provider, &["m_missing_type"]);

    let model_type: String = conn
        .query_row(
            "SELECT model_type FROM models WHERE provider_id = ?1 AND model_id = ?2",
            params![provider.as_str(), "m_missing_type"],
            |r| r.get(0),
        )
        .expect("query model_type");
    assert_eq!(
        model_type, "chat",
        "missing model_type must coalesce to 'chat'"
    );
}

#[test]
fn test_discovered_model_upsert_sql_direct_statement_preparation() {
    let (pool, _path) = fresh_pool();
    let conn = pool.open_connection().expect("open connection");
    let provider = CoreProviderId::new("prov_sql_direct");
    seed_provider(&conn, &provider);

    let tx = conn.unchecked_transaction().expect("tx");
    let mut stmt = tx
        .prepare(DISCOVERED_MODEL_UPSERT_SQL)
        .expect("prepare sql");
    let changed = stmt
        .execute(params![
            provider.as_str(),
            "direct-model",
            "Direct Model",
            "openai",
            3600i64,
            8192i64,
            2048i64,
            None::<&str>,
            None::<&str>,
            None::<&str>,
            None::<&str>,
            None::<&str>,
            "direct-model",
        ])
        .expect("execute");
    assert_eq!(changed, 1);
    drop(stmt);
    tx.commit().expect("commit");

    let model_type: String = conn
        .query_row(
            "SELECT model_type FROM models WHERE provider_id = ?1 AND model_id = 'direct-model'",
            [provider.as_str()],
            |r| r.get(0),
        )
        .expect("query model_type");
    assert_eq!(model_type, "chat");
}

#[test]
fn test_custom_model_survives_empty_pruning() {
    let (pool, _path) = fresh_pool();
    let conn = pool.open_connection().expect("open connection");
    let provider = CoreProviderId::new("prov_custom_survives");
    seed_provider(&conn, &provider);
    create_custom(
        &conn,
        &provider,
        &ModelId::new("custom-1"),
        Some("Custom Model 1"),
        TargetFormat::Openai,
        0,
        None,
    )
    .expect("create custom");
    seed_models(&conn, &provider, &["standard-1"]);

    let tx = conn.unchecked_transaction().expect("tx");
    prune_obsolete_models(&tx, &provider, &[]).expect("prune empty");
    tx.commit().expect("commit");

    let (has_std, has_cust): (bool, bool) = conn
        .query_row(
            "SELECT \
                EXISTS(SELECT 1 FROM models WHERE provider_id = ?1 AND model_id = 'standard-1'), \
                EXISTS(SELECT 1 FROM models WHERE provider_id = ?1 AND model_id = 'custom-1')",
            [provider.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("query exists");
    assert!(
        !has_std && has_cust,
        "standard must be pruned, custom must survive"
    );
}

#[test]
fn test_shared_prune_obsolete_models_contract_with_retained_and_obsolete() {
    let (pool, _path) = fresh_pool();
    let conn = pool.open_connection().expect("open connection");
    let provider = CoreProviderId::new("prov_prune_contract");
    seed_provider(&conn, &provider);
    seed_models(&conn, &provider, &["m_retained", "m_obsolete"]);
    create_custom(
        &conn,
        &provider,
        &ModelId::new("m_custom"),
        Some("Custom Model"),
        TargetFormat::Openai,
        0,
        None,
    )
    .expect("create custom");

    let tx = conn.unchecked_transaction().expect("tx");
    prune_obsolete_models(&tx, &provider, &[test_discovered("m_retained")]).expect("prune");
    tx.commit().expect("commit");

    let (has_ret, has_obs, has_cust): (bool, bool, bool) = conn
        .query_row(
            "SELECT \
                EXISTS(SELECT 1 FROM models WHERE provider_id = ?1 AND model_id = 'm_retained'), \
                EXISTS(SELECT 1 FROM models WHERE provider_id = ?1 AND model_id = 'm_obsolete'), \
                EXISTS(SELECT 1 FROM models WHERE provider_id = ?1 AND model_id = 'm_custom')",
            [provider.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("query exists");
    assert!(
        has_ret && !has_obs && has_cust,
        "retained & custom exist, obsolete pruned"
    );
}
