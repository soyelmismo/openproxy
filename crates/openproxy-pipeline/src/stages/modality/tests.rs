use super::*;
use openproxy_types::context::ResolvedTarget;

fn target_with_modalities(model: &str, modalities: Option<&str>) -> ResolvedTarget {
    let mut rt = ResolvedTarget::test("prov", model);
    rt.model.input_modalities_json = modalities.map(Box::from);
    rt
}

fn image_request() -> openproxy_types::OpenAIRequest {
    serde_json::from_value(serde_json::json!({
        "model": "combo",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "what is this"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}}
            ]
        }]
    }))
    .expect("valid request")
}

fn text_request() -> openproxy_types::OpenAIRequest {
    serde_json::from_value(serde_json::json!({
        "model": "combo",
        "messages": [{"role": "user", "content": "hello"}]
    }))
    .expect("valid request")
}

#[test]
fn detects_openai_image_url_parts() {
    assert!(request_requires_image_input(&image_request()));
    assert!(!request_requires_image_input(&text_request()));
}

#[test]
fn detects_input_image_and_anthropic_image_parts() {
    let responses_style = serde_json::from_value(serde_json::json!({
        "model": "combo",
        "messages": [{
            "role": "user",
            "content": [{"type": "input_image", "image_url": "https://example.com/a.png"}]
        }]
    }))
    .expect("valid request");
    assert!(request_requires_image_input(&responses_style));

    let anthropic_style = serde_json::from_value(serde_json::json!({
        "model": "combo",
        "messages": [{
            "role": "user",
            "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAA"}}]
        }]
    }))
    .expect("valid request");
    assert!(request_requires_image_input(&anthropic_style));
}

#[test]
fn text_only_request_keeps_every_target() {
    let targets = vec![
        target_with_modalities("text-model", Some(r#"["text"]"#)),
        target_with_modalities("vision-model", Some(r#"["text","image"]"#)),
    ];
    let out = filter_targets_for_input(
        targets,
        request_requires_image_input(&text_request()),
        ModalityRoutingConfig::default(),
    );
    assert_eq!(out.len(), 2);
}

#[test]
fn image_request_drops_text_only_targets() {
    let targets = vec![
        target_with_modalities("text-a", Some(r#"["text"]"#)),
        target_with_modalities("vision", Some(r#"["text","image"]"#)),
        target_with_modalities("text-b", Some(r#"["text"]"#)),
    ];
    let out = filter_targets_for_input(
        targets,
        request_requires_image_input(&image_request()),
        ModalityRoutingConfig::default(),
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].model.model_id.as_str(), "vision");
}

#[test]
fn unknown_modalities_are_treated_as_compatible() {
    let targets = vec![
        target_with_modalities("unknown", None),
        target_with_modalities("text", Some(r#"["text"]"#)),
    ];
    let out = filter_targets_for_input(targets, true, ModalityRoutingConfig::default());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].model.model_id.as_str(), "unknown");
}

#[test]
fn disabled_gate_keeps_every_target() {
    let config = ModalityRoutingConfig {
        enabled: false,
        ..ModalityRoutingConfig::default()
    };
    let targets = vec![target_with_modalities("text", Some(r#"["text"]"#))];
    let out = filter_targets_for_input(targets, true, config);
    assert_eq!(out.len(), 1);
}

#[test]
fn fallback_keeps_targets_when_none_accept_images() {
    let config = ModalityRoutingConfig::default();
    assert!(config.fallback_to_text_only);
    let targets = vec![
        target_with_modalities("text-a", Some(r#"["text"]"#)),
        target_with_modalities("text-b", Some(r#"["text"]"#)),
    ];
    let out = filter_targets_for_input(targets, true, config);
    assert_eq!(out.len(), 2);
}

#[test]
fn strict_mode_returns_empty_when_none_accept_images() {
    let config = ModalityRoutingConfig {
        fallback_to_text_only: false,
        ..ModalityRoutingConfig::default()
    };
    let targets = vec![target_with_modalities("text", Some(r#"["text"]"#))];
    let out = filter_targets_for_input(targets, true, config);
    assert!(out.is_empty());
}
