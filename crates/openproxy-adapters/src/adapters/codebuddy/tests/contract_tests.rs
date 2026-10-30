use super::*;
use openproxy_types::{ModelId, TargetFormat};
use std::sync::Arc;

#[test]
fn test_codebuddy_pinned_unverified_contract_does_not_elevate_headers() {
    let snapshot = crate::client_contract::providers::codebuddy_state::embedded_snapshot();
    assert!(!snapshot.payload.identity_activatable);
    assert!(snapshot.client_identity.version.is_none());
    let baseline = CodeBuddyAdapter::new().build_headers(
        "test-token",
        TargetFormat::Openai,
        &ModelId::new("gpt-5.5"),
    );
    let adapter = CodeBuddyAdapter::new().with_pinned_contract(Arc::new(snapshot));
    let headers =
        adapter.build_headers("test-token", TargetFormat::Openai, &ModelId::new("gpt-5.5"));
    let strip_dynamic = |list: &[(String, String)]| -> Vec<(String, String)> {
        list.iter()
            .filter(|(k, _)| {
                !k.eq_ignore_ascii_case("x-conversation-id")
                    && !k.eq_ignore_ascii_case("x-conversation-request-id")
                    && !k.eq_ignore_ascii_case("x-conversation-message-id")
            })
            .cloned()
            .collect()
    };
    assert_eq!(
        strip_dynamic(&headers),
        strip_dynamic(&baseline),
        "el catálogo no autoriza elevar identidad"
    );
}

#[test]
fn test_codebuddy_pinned_contract_drives_the_announced_headers() {
    // Snapshot sintético activable: comprueba solo el consumo del adapter.
    let mut snapshot = crate::client_contract::providers::codebuddy_state::embedded_snapshot();
    snapshot.payload.identity_activatable = true;
    snapshot.client_identity.version = Some("2.199.9".to_string());
    let version = snapshot.client_identity.version.clone().unwrap();
    let adapter = CodeBuddyAdapter::new().with_pinned_contract(Arc::new(snapshot));

    assert!(
        adapter.pinned_contract().is_some(),
        "el pin debe quedar congelado"
    );

    let headers =
        adapter.build_headers("test-token", TargetFormat::Openai, &ModelId::new("gpt-5.5"));
    let find = |key: &str| find_header(&headers, key);
    // Versión y user-agent salen del MISMO snapshot: una sola generación
    // coherente para toda la petición.
    assert_eq!(find("x-ide-version"), Some(version.as_str()));
    assert_eq!(
        find("user-agent"),
        Some(format!("CLI/{version} CodeBuddy/{version}").as_str())
    );
    // El resto de la identidad anunciada no cambia.
    assert_eq!(find("x-ide-type"), Some("CLI"));
    assert_eq!(find("x-codebuddy-request"), Some("1"));
}

#[tokio::test]
async fn test_codebuddy_discovery_reads_the_pinned_catalog_not_the_static_list() {
    let mut snapshot = crate::client_contract::providers::codebuddy_state::embedded_snapshot();
    let mut product: serde_json::Value =
        serde_json::from_slice(snapshot.catalog_body().unwrap()).unwrap();
    product["models"].as_array_mut().unwrap().truncate(1);
    let mut expected_id = product["models"][0]["id"].as_str().unwrap().to_string();
    if is_codebuddy_model_free(product["models"][0].get("credits").and_then(|c| c.as_str())) {
        expected_id.push_str(":free");
    }
    let expected_input = product["models"][0]["maxInputTokens"].as_i64();
    snapshot.catalog = Some(Arc::from(serde_json::to_vec(&product).unwrap()));
    let adapter = CodeBuddyAdapter::new().with_pinned_contract(Arc::new(snapshot));
    let discovered = adapter
        .fetch_models(&UpstreamClient::new(), "unused")
        .await
        .unwrap();
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].model_id.as_str(), expected_id);
    assert_eq!(discovered[0].context_length, expected_input);
}

#[tokio::test]
async fn test_codebuddy_discovery_falls_back_when_snapshot_has_no_catalog() {
    let mut snapshot = crate::client_contract::providers::codebuddy_state::embedded_snapshot();
    snapshot.catalog = None;
    let adapter = CodeBuddyAdapter::new().with_pinned_contract(Arc::new(snapshot));
    let discovered = adapter
        .fetch_models(&UpstreamClient::new(), "unused")
        .await
        .unwrap();
    assert_eq!(discovered.len(), CODEBUDDY_MODELS.len());
}

#[test]
fn test_codebuddy_config_override_wins_over_the_pinned_contract() {
    let snapshot = crate::client_contract::providers::codebuddy_state::embedded_snapshot();
    let mut adapter = CodeBuddyAdapter::new().with_pinned_contract(Arc::new(snapshot));
    adapter
        .config_mut()
        .expect("config mutable")
        .extra_headers
        .push(("x-ide-version".to_string(), "9.9.9".to_string()));

    let headers =
        adapter.build_headers("test-token", TargetFormat::Openai, &ModelId::new("gpt-5.5"));
    let find = |key: &str| find_header(&headers, key);
    // El override explícito del despliegue gana sobre el contrato.
    assert_eq!(find("x-ide-version"), Some("9.9.9"));
}
