use crate::adapters::{
    CodexAdapter, CustomAdapter, ProviderAdapter, ProviderAdapterEnum, builtin_adapters,
};
use openproxy_types::ProviderId;

fn snapshot<A: ProviderAdapter>(adapter: &A) -> (serde_json::Value, &'static [&'static str]) {
    let meta = serde_json::to_value(adapter.metadata()).expect("serialize metadata");
    (meta, adapter.models_dev_canonical_ids())
}

fn make_custom_enum() -> ProviderAdapterEnum {
    let mut cfg = CodexAdapter::new().config().clone();
    cfg.id = ProviderId::new("custom-test");
    cfg.name = "Custom Test".to_string();
    cfg.extra_headers.clear();
    ProviderAdapterEnum::Custom(Box::new(CustomAdapter::from_config(cfg)))
}

fn mutate_via_trait<A: ProviderAdapter>(adapter: &mut A, k: &str, v: &str) -> bool {
    if let Some(cfg) = adapter.config_mut() {
        cfg.extra_headers.push((k.to_string(), v.to_string()));
        true
    } else {
        false
    }
}

#[test]
fn test_trait_parity_metadata_and_canonical_ids_all_adapters() {
    let mut adapters = builtin_adapters();
    assert_eq!(adapters.len(), 22);
    adapters.push(make_custom_enum());

    for adapter in &adapters {
        let inherent_meta = serde_json::to_value(adapter.metadata()).expect("serialize inherent");
        let (trait_meta, trait_canonical) = snapshot(adapter);

        assert_eq!(inherent_meta, trait_meta, "id: {}", adapter.id());
        assert_eq!(adapter.models_dev_canonical_ids(), trait_canonical);
    }
}

#[test]
fn test_trait_parity_concrete_overrides_non_default() {
    let codex = ProviderAdapterEnum::from_provider_id("codex").unwrap();
    assert_eq!(snapshot(&codex).0["requires_oauth"], true);

    let custom = make_custom_enum();
    assert_eq!(snapshot(&custom).0["deletable"], true);

    let gemini = ProviderAdapterEnum::from_provider_id("gemini").unwrap();
    assert!(snapshot(&gemini).1.contains(&"google"));
}

#[test]
fn test_trait_parity_config_mut_generic_mutation() {
    let cases = [
        (
            ProviderAdapterEnum::from_provider_id("codex").unwrap(),
            true,
        ),
        (make_custom_enum(), true),
        (
            ProviderAdapterEnum::from_provider_id("gemini").unwrap(),
            false,
        ),
    ];

    for (mut adapter, should_mutate) in cases {
        let mutated = mutate_via_trait(&mut adapter, "x-parity", "val");
        assert_eq!(mutated, should_mutate, "id: {}", adapter.id());

        let has_hdr =
            |hdrs: &[(String, String)]| hdrs.iter().any(|(k, v)| k == "x-parity" && v == "val");

        if should_mutate {
            assert!(has_hdr(&adapter.config().extra_headers));
            assert!(has_hdr(&ProviderAdapter::config(&adapter).extra_headers));
        } else {
            assert!(ProviderAdapterEnum::config_mut(&mut adapter).is_none());
            assert!(ProviderAdapter::config_mut(&mut adapter).is_none());
        }
    }
}

#[test]
fn test_trait_parity_serde_roundtrip_offline() {
    let cases = [
        make_custom_enum(),
        ProviderAdapterEnum::from_provider_id("codex").unwrap(),
    ];

    for mut adapter in cases {
        mutate_via_trait(&mut adapter, "x-serde", "val");
        let before_snap = snapshot(&adapter);

        let json = serde_json::to_string(&adapter).expect("serialize");
        let deserialized: ProviderAdapterEnum = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(snapshot(&deserialized), before_snap);
        assert!(
            deserialized
                .config()
                .extra_headers
                .iter()
                .any(|(k, v)| k == "x-serde" && v == "val")
        );
    }
}
