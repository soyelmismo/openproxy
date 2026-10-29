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

#[test]
fn combo_override_forces_gate_off() {
    let global = ModalityRoutingConfig::default();
    let effective = effective_config(global, Some(false));
    assert!(!effective.enabled);
    // Fallback policy stays global: it is not a per-combo routing taste.
    assert_eq!(
        effective.fallback_to_text_only,
        global.fallback_to_text_only
    );
}

#[test]
fn combo_override_forces_gate_on() {
    let global = ModalityRoutingConfig {
        enabled: false,
        ..ModalityRoutingConfig::default()
    };
    let effective = effective_config(global, Some(true));
    assert!(effective.enabled);
}

#[test]
fn combo_without_override_inherits_global() {
    let global = ModalityRoutingConfig {
        enabled: false,
        fallback_to_text_only: false,
    };
    assert_eq!(effective_config(global, None), global);
}

fn media_request(parts: serde_json::Value) -> openproxy_types::OpenAIRequest {
    serde_json::from_value(serde_json::json!({
        "model": "combo",
        "messages": [{ "role": "user", "content": parts }]
    }))
    .expect("valid request")
}

#[test]
fn detects_video_audio_and_file_parts() {
    let video = media_request(serde_json::json!([
        { "type": "video_url", "video_url": { "url": "https://example.com/a.mp4" } }
    ]));
    assert_eq!(
        request_required_modalities(&video),
        RequiredModalities {
            video: true,
            ..Default::default()
        }
    );

    let audio = media_request(serde_json::json!([
        { "type": "input_audio", "input_audio": { "data": "AAA", "format": "wav" } }
    ]));
    assert_eq!(
        request_required_modalities(&audio),
        RequiredModalities {
            audio: true,
            ..Default::default()
        }
    );

    let file = media_request(serde_json::json!([
        { "type": "file", "file": { "file_data": "data:application/pdf;base64,AAA" } }
    ]));
    assert_eq!(
        request_required_modalities(&file),
        RequiredModalities {
            file: true,
            ..Default::default()
        }
    );

    let anthropic_video = media_request(serde_json::json!([
        { "type": "video", "source": { "type": "url", "url": "https://example.com/a.mp4" } }
    ]));
    assert!(request_required_modalities(&anthropic_video).video);

    let anthropic_document = media_request(serde_json::json!([
        { "type": "document", "source": { "type": "base64", "media_type": "application/pdf", "data": "AAA" } }
    ]));
    assert!(request_required_modalities(&anthropic_document).file);
}

#[test]
fn video_request_drops_targets_without_video() {
    let targets = vec![
        target_with_modalities("vision-only", Some(r#"["text","image"]"#)),
        target_with_modalities("video-capable", Some(r#"["text","image","video"]"#)),
        target_with_modalities("text-only", Some(r#"["text"]"#)),
    ];
    let required = RequiredModalities {
        video: true,
        ..Default::default()
    };
    let out = filter_targets_for_modalities(targets, required, ModalityRoutingConfig::default());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].model.model_id.as_str(), "video-capable");
}

#[test]
fn audio_request_drops_targets_without_audio() {
    let targets = vec![
        target_with_modalities("vision-only", Some(r#"["text","image"]"#)),
        target_with_modalities("audio-capable", Some(r#"["text","audio"]"#)),
    ];
    let required = RequiredModalities {
        audio: true,
        ..Default::default()
    };
    let out = filter_targets_for_modalities(targets, required, ModalityRoutingConfig::default());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].model.model_id.as_str(), "audio-capable");
}

#[test]
fn file_request_is_gated_as_image_capability() {
    let targets = vec![
        target_with_modalities("text-only", Some(r#"["text"]"#)),
        target_with_modalities("vision", Some(r#"["text","image"]"#)),
    ];
    let required = RequiredModalities {
        file: true,
        ..Default::default()
    };
    let out = filter_targets_for_modalities(targets, required, ModalityRoutingConfig::default());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].model.model_id.as_str(), "vision");
}

#[test]
fn pdf_only_model_survives_a_document_request() {
    // `["text","pdf"]` parses as text-only because ModalityFlags has no
    // document bit. Dropping it would reject the one model that advertises
    // exactly what the request carries.
    let targets = vec![
        target_with_modalities("text-only", Some(r#"["text"]"#)),
        target_with_modalities("pdf-model", Some(r#"["text","pdf"]"#)),
        target_with_modalities("vision", Some(r#"["text","image"]"#)),
    ];
    let required = RequiredModalities {
        file: true,
        ..Default::default()
    };
    let out = filter_targets_for_modalities(targets, required, ModalityRoutingConfig::default());
    let kept: Vec<&str> = out.iter().map(|t| t.model.model_id.as_str()).collect();
    assert_eq!(kept, vec!["pdf-model", "vision"]);
}

#[test]
fn pdf_only_model_is_still_rejected_for_image_requests() {
    let targets = vec![target_with_modalities(
        "pdf-model",
        Some(r#"["text","pdf"]"#),
    )];
    let required = RequiredModalities {
        image: true,
        ..Default::default()
    };
    let out = filter_targets_for_modalities(targets, required, ModalityRoutingConfig::default());
    // Nothing accepts an image, so the fallback keeps the set unfiltered.
    assert_eq!(out.len(), 1);

    let strict = ModalityRoutingConfig {
        fallback_to_text_only: false,
        ..ModalityRoutingConfig::default()
    };
    let targets = vec![target_with_modalities(
        "pdf-model",
        Some(r#"["text","pdf"]"#),
    )];
    assert!(filter_targets_for_modalities(targets, required, strict).is_empty());
}

#[test]
fn multi_modality_request_needs_every_declared_flag() {
    let targets = vec![
        target_with_modalities("vision", Some(r#"["text","image"]"#)),
        target_with_modalities("full", Some(r#"["text","image","video","audio"]"#)),
    ];
    let required = RequiredModalities {
        image: true,
        video: true,
        ..Default::default()
    };
    let out = filter_targets_for_modalities(targets, required, ModalityRoutingConfig::default());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].model.model_id.as_str(), "full");
}

#[test]
fn text_only_request_never_filters_on_video_or_audio() {
    let targets = vec![target_with_modalities("text", Some(r#"["text"]"#))];
    let out = filter_targets_for_modalities(
        targets,
        RequiredModalities::default(),
        ModalityRoutingConfig::default(),
    );
    assert_eq!(out.len(), 1);
}
