use super::fresh_pool;
use crate::admin::models::{
    CreateCustomModelInput, UpdateModelInput, create_custom_model, refresh_models, update_model,
};
use crate::admin::providers::{CreateProviderInput, create_provider};
use crate::error::CoreError;
use crate::ids::ProviderId;
use crate::models;
use std::time::Duration;

fn seed_prov(conn: &rusqlite::Connection, id: &str) {
    create_provider(
        conn,
        CreateProviderInput {
            rate_limit_scope: None,
            id: id.into(),
            name: id.into(),
            base_url: "https://x".into(),
            auth_type: "bearer".into(),
            format: "openai".into(),
            extra_headers_json: None,
        },
    )
    .expect("seed");
}

#[tokio::test]
async fn refresh_models_with_invalid_provider_fails() {
    let (pool, _path) = fresh_pool();
    let adapter = openproxy_adapters::adapters::ProviderAdapterEnum::Mock(Box::new(
        openproxy_adapters::adapters::MockAdapter::new(
            "stub",
            "",
            openproxy_adapters::adapters::AdapterFormat::Openai,
        ),
    ));
    let upstream = openproxy_adapters::upstream::UpstreamClient::new();
    let res = refresh_models(
        &pool,
        &ProviderId::new("does-not-exist"),
        "sk-x",
        &adapter,
        &upstream,
        3600,
        "",
    )
    .await;
    match res {
        Err(CoreError::ProviderNotFound(id)) => assert_eq!(id, "does-not-exist"),
        other => panic!("expected ProviderNotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn refresh_models_with_empty_catalog_fails_and_preserves_models() {
    let (pool, _path) = fresh_pool();
    let pid = ProviderId::new("prov-preserve");
    {
        let conn = pool.writer();
        seed_prov(&conn, pid.as_str());
        models::upsert_many(
            &conn,
            &pid,
            &[models::DiscoveredModel {
                model_id: openproxy_types::ModelId::new("model-original"),
                display_name: Some("Original Model".into()),
                target_format: openproxy_types::TargetFormat::Openai,
                context_length: Some(4096),
                max_output_tokens: Some(2048),
                input_modalities: None,
                output_modalities: None,
                model_type: None,
                family: None,
                capabilities: None,
            }],
            Duration::from_secs(3600),
        )
        .expect("seed");
    }

    let adapter = openproxy_adapters::adapters::ProviderAdapterEnum::Mock(Box::new(
        openproxy_adapters::adapters::MockAdapter::new(
            "prov-preserve",
            "",
            openproxy_adapters::adapters::AdapterFormat::Openai,
        ),
    ));
    let upstream = openproxy_adapters::upstream::UpstreamClient::new();
    let res = refresh_models(&pool, &pid, "sk-test", &adapter, &upstream, 3600, "").await;
    assert!(matches!(res, Err(CoreError::UpstreamConnection(_))));

    let models_left = models::list_all(&pool.reader()).expect("list");
    assert_eq!(models_left.len(), 1);
    assert_eq!(models_left[0].model_id.as_str(), "model-original");
}

#[test]
fn create_custom_model_wraps_validation() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    seed_prov(&conn, "p");

    let mut bad = CreateCustomModelInput {
        provider_id: "p".into(),
        model_id: "m".into(),
        display_name: None,
        target_format: "xml".into(),
        ttl_seconds: 60,
        model_type: None,
    };
    assert!(matches!(
        create_custom_model(&conn, bad.clone()).expect_err("bad"),
        CoreError::Validation(_)
    ));

    bad.model_id = "my-model".into();
    bad.display_name = Some("Display".into());
    bad.target_format = "openai".into();
    bad.model_type = Some("image".into());
    let row_id = create_custom_model(&conn, bad).expect("create custom");
    let m = models::get_by_row_id(&conn, row_id)
        .unwrap()
        .expect("present");
    assert!(m.custom && m.active && m.model_type.as_ref() == "image");

    update_model(
        &conn,
        row_id,
        UpdateModelInput {
            display_name: Some("Updated Display".into()),
            model_type: Some("embedding".into()),
            target_format: None,
        },
    )
    .expect("update");
    let updated_m = models::get_by_row_id(&conn, row_id)
        .unwrap()
        .expect("present");
    assert_eq!(updated_m.display_name.as_deref(), Some("Updated Display"));
    assert_eq!(&*updated_m.model_type, "embedding");
}
