use crate::adapters::*;
use openproxy_types::{ModelId, ProviderId, TargetFormat};

fn make_custom_provider(
    id: &str,
    base_url: &str,
    auth_type: openproxy_types::AuthType,
    format: openproxy_types::ProviderFormat,
) -> openproxy_types::Provider {
    openproxy_types::Provider {
        id: ProviderId::new(id),
        name: format!("Test {id}").into(),
        base_url: base_url.into(),
        auth_type,
        format,
        extra_headers_json: None,
        auto_activate_keyword: None,
        notif_keyword_only: false,
        rate_limit_scope: openproxy_types::RateLimitScope::Account,
        active: true,
        created_at: "2026-01-01T00:00:00Z".into(),
        use_proxies: false,
        current_proxy_id: None,
        proxy_rotation_errors: "429,connect_error,timeout".into(),
        proxy_rotation_mode: "global".into(),
        has_favicon: false,
    }
}

#[test]
fn custom_openai_adapter_builds_correct_url() {
    let p = make_custom_provider(
        "zenmux",
        "https://zenmux.example.com/v1",
        openproxy_types::AuthType::Bearer,
        openproxy_types::ProviderFormat::Openai,
    );
    let a = CustomAdapter::from_provider_row(&p);
    let url = a.build_chat_url(TargetFormat::Openai, &ModelId::new("gpt-4o"));
    assert_eq!(url, "https://zenmux.example.com/v1/chat/completions");
}

#[test]
fn custom_anthropic_adapter_builds_correct_url() {
    let p = make_custom_provider(
        "my-anthropic",
        "https://api.example.com",
        openproxy_types::AuthType::XApiKey,
        openproxy_types::ProviderFormat::Anthropic,
    );
    let a = CustomAdapter::from_provider_row(&p);
    let url = a.build_chat_url(TargetFormat::Anthropic, &ModelId::new("claude-4"));
    assert_eq!(url, "https://api.example.com/messages");
}

#[test]
fn custom_gemini_adapter_builds_correct_url() {
    let p = make_custom_provider(
        "my-gemini",
        "https://gemini.example.com/v1beta",
        openproxy_types::AuthType::GoogApiKey,
        openproxy_types::ProviderFormat::Gemini,
    );
    let a = CustomAdapter::from_provider_row(&p);
    let url = a.build_chat_url(TargetFormat::Gemini, &ModelId::new("gemini-2.5-pro"));
    assert_eq!(
        url,
        "https://gemini.example.com/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
    );
}

#[test]
fn custom_mixed_adapter_routes_by_target_format() {
    let p = make_custom_provider(
        "my-aggregator",
        "https://agg.example.com/v1",
        openproxy_types::AuthType::Bearer,
        openproxy_types::ProviderFormat::Mixed,
    );
    let a = CustomAdapter::from_provider_row(&p);
    let openai_url = a.build_chat_url(TargetFormat::Openai, &ModelId::new("gpt-4o"));
    assert_eq!(openai_url, "https://agg.example.com/v1/chat/completions");
    let anthropic_url = a.build_chat_url(TargetFormat::Anthropic, &ModelId::new("claude-4"));
    assert_eq!(anthropic_url, "https://agg.example.com/v1/messages");
}

#[test]
fn custom_bearer_auth_header() {
    let p = make_custom_provider(
        "zenmux",
        "https://zenmux.example.com/v1",
        openproxy_types::AuthType::Bearer,
        openproxy_types::ProviderFormat::Openai,
    );
    let a = CustomAdapter::from_provider_row(&p);
    let (name, value) = a.build_auth_header("sk-test-123").unwrap();
    assert_eq!(name, "Authorization");
    assert_eq!(value, "Bearer sk-test-123");
}

#[test]
fn custom_x_api_key_auth_header() {
    let p = make_custom_provider(
        "my-anthropic",
        "https://api.example.com",
        openproxy_types::AuthType::XApiKey,
        openproxy_types::ProviderFormat::Anthropic,
    );
    let a = CustomAdapter::from_provider_row(&p);
    let (name, value) = a.build_auth_header("sk-ant-test").unwrap();
    assert_eq!(name, "x-api-key");
    assert_eq!(value, "sk-ant-test");
}

#[test]
fn custom_no_auth_header() {
    let p = make_custom_provider(
        "local-ollama",
        "http://localhost:11434/v1",
        openproxy_types::AuthType::None,
        openproxy_types::ProviderFormat::Openai,
    );
    let a = CustomAdapter::from_provider_row(&p);
    assert_eq!(a.build_auth_header(""), None);
}

#[test]
fn custom_models_url() {
    let p = make_custom_provider(
        "zenmux",
        "https://zenmux.example.com/v1",
        openproxy_types::AuthType::Bearer,
        openproxy_types::ProviderFormat::Openai,
    );
    let a = CustomAdapter::from_provider_row(&p);
    assert_eq!(
        a.models_url(),
        Some("https://zenmux.example.com/v1/models".into())
    );
}

#[test]
fn custom_adapter_id_matches_provider() {
    let p = make_custom_provider(
        "zenmux",
        "https://zenmux.example.com/v1",
        openproxy_types::AuthType::Bearer,
        openproxy_types::ProviderFormat::Openai,
    );
    let a = CustomAdapter::from_provider_row(&p);
    assert_eq!(a.id().as_str(), "zenmux");
}

#[test]
fn custom_adapter_includes_extra_headers() {
    let mut p = make_custom_provider(
        "zenmux",
        "https://zenmux.example.com/v1",
        openproxy_types::AuthType::Bearer,
        openproxy_types::ProviderFormat::Openai,
    );
    p.extra_headers_json = Some(r#"{"X-Custom":"value1"}"#.into());
    let a = CustomAdapter::from_provider_row(&p);
    let headers = a.build_headers("sk-test", TargetFormat::Openai, &ModelId::new("gpt-4o"));
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "X-Custom" && v == "value1")
    );
}

#[test]
fn custom_adapter_embedding_url_and_format() {
    let p = make_custom_provider(
        "zenmux",
        "https://zenmux.example.com/v1",
        openproxy_types::AuthType::Bearer,
        openproxy_types::ProviderFormat::Openai,
    );
    let a = CustomAdapter::from_provider_row(&p);
    assert_eq!(
        a.build_embeddings_url(),
        "https://zenmux.example.com/v1/embeddings"
    );

    let req = openproxy_types::embeddings::EmbeddingRequest {
        model: "text-embedding-3-small".into(),
        input: openproxy_types::embeddings::EmbeddingInput::Single("hello".into()),
        encoding_format: None,
        dimensions: None,
        user: None,
    };
    let formatted = a
        .format_embedding_request(&req, "text-embedding-3-small")
        .unwrap();
    let val: serde_json::Value = serde_json::from_slice(&formatted).unwrap();
    assert_eq!(val["model"], "text-embedding-3-small");
    assert_eq!(val["input"], "hello");
}

#[test]
fn custom_adapter_image_urls_and_format() {
    let p = make_custom_provider(
        "zenmux",
        "https://zenmux.example.com/v1",
        openproxy_types::AuthType::Bearer,
        openproxy_types::ProviderFormat::Openai,
    );
    let a = CustomAdapter::from_provider_row(&p);
    assert_eq!(
        a.build_image_url(),
        "https://zenmux.example.com/v1/images/generations"
    );
    assert_eq!(
        a.build_image_edits_url(),
        "https://zenmux.example.com/v1/images/edits"
    );
    assert_eq!(
        a.build_image_variations_url(),
        "https://zenmux.example.com/v1/images/variations"
    );

    let req = openproxy_types::images::ImageGenerationRequest {
        prompt: "a landscape".into(),
        model: "flux-pro".into(),
        n: Some(1),
        size: Some("1024x1024".into()),
        quality: None,
        response_format: None,
        style: None,
        user: None,
        aspect_ratio: Some("16:9".into()),
        seed: Some(12345),
        negative_prompt: Some("blurry".into()),
        post_processing: None,
    };
    let formatted = a.format_image_request(&req, "flux-pro-v1").unwrap();
    let val: serde_json::Value = serde_json::from_slice(&formatted).unwrap();
    assert_eq!(val["model"], "flux-pro-v1");
    assert_eq!(val["aspect_ratio"], "16:9");
    assert_eq!(val["seed"], 12345);
    assert_eq!(val["negative_prompt"], "blurry");
}
