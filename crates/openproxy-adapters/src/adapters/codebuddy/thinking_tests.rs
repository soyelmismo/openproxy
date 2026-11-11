use super::*;
use crate::adapters::ProviderAdapter;
use openproxy_types::{ModelId, TargetFormat};

fn test_codebuddy_target_with_effort(
    model_name: &str,
    effort: Option<&str>,
) -> openproxy_types::context::ResolvedTarget {
    openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ComboTargetId(1),
            combo_id: openproxy_types::ComboId(1),
            provider_id: openproxy_types::ProviderId::new("codebuddy"),
            account_id: None,
            model_row_id: Some(openproxy_types::ModelRowId(1)),
            sub_combo_id: None,
            priority_order: 0,
            weight: 100,
            active: true,
            rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: effort.map(ToString::to_string),
            description: None,
        },
        model: openproxy_types::Model {
            row_id: openproxy_types::ModelRowId(1),
            provider_id: openproxy_types::ProviderId::new("codebuddy"),
            target_format: openproxy_types::TargetFormat::Openai,
            discovered_at: openproxy_types::now_unix_secs_str().into_boxed_str(),
            expires_at: None,
            model_id: openproxy_types::ModelId::new(model_name),
            display_name: None,
            context_length: None,
            max_output_tokens: None,
            model_type: "chat".into(),
            family: None,
            input_modalities_json: None,
            output_modalities_json: None,
            capabilities_json: None,
            timeout_overrides_json: None,
            active: true,
            last_test_status: None,
            last_test_at: None,
            custom: false,
            ..Default::default()
        },
        api_key: "test-token".to_string(),
        api_key_label: None,
        custom_meta: None,
    }
}

#[test]
fn test_is_codebuddy_deepseek_model() {
    assert!(is_codebuddy_deepseek_model("deepseek-v3-2-volc"));
    assert!(is_codebuddy_deepseek_model("DeepSeek-V3.2"));
    assert!(is_codebuddy_deepseek_model("deepseek-r1"));
    assert!(is_codebuddy_deepseek_model("deepseek-chat"));
    assert!(!is_codebuddy_deepseek_model("gpt-5.5"));
    assert!(!is_codebuddy_deepseek_model("hy3"));
    assert!(!is_codebuddy_deepseek_model("minimax-m3"));
    assert!(!is_codebuddy_deepseek_model("gemini-3.1-pro"));
}

#[test]
fn test_patch_codebuddy_deepseek_thinking_default_enabled() {
    let mut obj = serde_json::Map::new();
    obj.insert("model".into(), serde_json::json!("deepseek-v3-2-volc"));
    patch_codebuddy_deepseek_thinking(&mut obj);

    assert_eq!(
        obj.get("thinking"),
        Some(&serde_json::json!({ "type": "enabled" }))
    );
    assert!(!obj.contains_key("reasoning_effort"));
}

#[test]
fn test_patch_codebuddy_deepseek_thinking_preserves_reasoning_effort() {
    let mut obj = serde_json::Map::new();
    obj.insert("model".into(), serde_json::json!("deepseek-v3-2-volc"));
    obj.insert("reasoning_effort".into(), serde_json::json!("high"));
    patch_codebuddy_deepseek_thinking(&mut obj);

    assert_eq!(
        obj.get("thinking"),
        Some(&serde_json::json!({ "type": "enabled" }))
    );
    assert_eq!(
        obj.get("reasoning_effort"),
        Some(&serde_json::json!("high"))
    );
}

#[test]
fn test_patch_codebuddy_deepseek_thinking_disabled_via_effort_none() {
    let mut obj = serde_json::Map::new();
    obj.insert("model".into(), serde_json::json!("deepseek-v3-2-volc"));
    obj.insert("reasoning_effort".into(), serde_json::json!("none"));
    patch_codebuddy_deepseek_thinking(&mut obj);

    assert_eq!(
        obj.get("thinking"),
        Some(&serde_json::json!({ "type": "disabled" }))
    );
    assert!(!obj.contains_key("reasoning_effort"));
}

#[test]
fn test_patch_codebuddy_deepseek_thinking_disabled_via_thinking_object() {
    let mut obj = serde_json::Map::new();
    obj.insert("model".into(), serde_json::json!("deepseek-v3-2-volc"));
    obj.insert("thinking".into(), serde_json::json!({ "type": "disabled" }));
    patch_codebuddy_deepseek_thinking(&mut obj);

    assert_eq!(
        obj.get("thinking"),
        Some(&serde_json::json!({ "type": "disabled" }))
    );
}

#[test]
fn test_patch_codebuddy_deepseek_thinking_extracts_effort_from_thinking_obj() {
    let mut obj = serde_json::Map::new();
    obj.insert("model".into(), serde_json::json!("deepseek-v3-2-volc"));
    obj.insert(
        "thinking".into(),
        serde_json::json!({ "type": "enabled", "reasoning_effort": "medium" }),
    );
    patch_codebuddy_deepseek_thinking(&mut obj);

    assert_eq!(
        obj.get("thinking"),
        Some(&serde_json::json!({ "type": "enabled" }))
    );
    assert_eq!(
        obj.get("reasoning_effort"),
        Some(&serde_json::json!("medium"))
    );
}

#[test]
fn test_wrap_request_body_deepseek_with_target_effort() {
    let adapter = CodeBuddyAdapter::new();
    let raw_unary_json = r#"{"messages":[{"role":"user","content":"hello"}]}"#;
    let resolved_target =
        test_codebuddy_target_with_effort("deepseek-v3-2-volc", Some("high"));

    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from(raw_unary_json),
            TargetFormat::Openai,
            &ModelId::new("deepseek-v3-2-volc"),
            &resolved_target,
        )
        .expect("wrap succeeds");

    let val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    assert_eq!(
        val.get("stream").and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        val.get("thinking"),
        Some(&serde_json::json!({ "type": "enabled" }))
    );
    assert_eq!(
        val.get("reasoning_effort"),
        Some(&serde_json::json!("high"))
    );
}

#[test]
fn test_wrap_request_body_non_deepseek_does_not_inject_thinking() {
    let adapter = CodeBuddyAdapter::new();
    let raw_unary_json = r#"{"messages":[{"role":"user","content":"hello"}]}"#;
    let resolved_target = test_codebuddy_target_with_effort("gpt-5.5", None);

    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from(raw_unary_json),
            TargetFormat::Openai,
            &ModelId::new("gpt-5.5"),
            &resolved_target,
        )
        .expect("wrap succeeds");

    let val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    assert_eq!(
        val.get("stream").and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(val.get("thinking"), None);
    assert_eq!(val.get("reasoning_effort"), None);
}

#[test]
fn test_normalize_openai_request_deepseek_thinking() {
    let adapter = CodeBuddyAdapter::new();
    let req = openproxy_types::OpenAIRequest {
        model: "deepseek-v3-2-volc".into(),
        stream: false,
        ..Default::default()
    };
    let mut view =
        openproxy_types::OpenAIRequestView::new(&req, "deepseek-v3-2-volc", &[], false);

    adapter.normalize_openai_request(&mut view);
    assert!(view.stream);
    assert_eq!(
        view.extra.get("thinking"),
        Some(&serde_json::json!({ "type": "enabled" }))
    );
}
