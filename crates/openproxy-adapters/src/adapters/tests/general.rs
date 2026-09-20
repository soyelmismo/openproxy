use super::{first_header, has_header};
use crate::adapters::opencode_common::classify_zen_target_format;
use crate::adapters::*;
use openproxy_types::{ModelId, TargetFormat};

// ---- OpenRouter -----------------------------------------------------

#[test]
fn openrouter_builds_correct_url() {
    let a = OpenRouterAdapter::new();
    let url = a.build_chat_url(TargetFormat::Openai, &ModelId::new("any"));
    assert_eq!(url, "https://openrouter.ai/api/v1/chat/completions");
    // target_format is ignored: still /chat/completions.
    let url2 = a.build_chat_url(TargetFormat::Anthropic, &ModelId::new("any"));
    assert_eq!(url2, url);
}

#[test]
fn openrouter_builds_bearer_auth() {
    let a = OpenRouterAdapter::new();
    let (name, value) = a.build_auth_header("sk-test-123").unwrap();
    assert_eq!(name, "Authorization");
    assert_eq!(value, "Bearer sk-test-123");
}

#[test]
fn openrouter_models_url() {
    let a = OpenRouterAdapter::new();
    assert_eq!(
        a.models_url().as_deref(),
        Some("https://openrouter.ai/api/v1/models")
    );
}

#[test]
fn openrouter_headers_include_referer_and_content_type() {
    let a = OpenRouterAdapter::new();
    let headers = a.build_headers("k", TargetFormat::Openai, &ModelId::new("any"));
    assert_eq!(first_header(&headers, "Authorization"), Some("Bearer k"));
    assert_eq!(
        first_header(&headers, "Content-Type"),
        Some("application/json")
    );
    assert_eq!(
        first_header(&headers, "HTTP-Referer"),
        Some("https://openproxy.local")
    );
    assert_eq!(first_header(&headers, "X-Title"), Some("openproxy"));
}

// ---- MiniMax -----------------------------------------------------

#[test]
fn minimax_builds_messages_url_managed_and_byok() {
    let a = MiniMaxAdapter::new();
    let url = a.build_chat_url(TargetFormat::Anthropic, &ModelId::new("m"));
    assert_eq!(url, "https://agent.minimax.io/mavis/api/v1/llm/v1/messages");

    let mut byok = MiniMaxAdapter::new();
    byok.config.base_url = "https://api.minimax.io".into();
    let byok_url = byok.build_chat_url(TargetFormat::Anthropic, &ModelId::new("m"));
    assert_eq!(
        byok_url,
        "https://api.minimax.io/anthropic/v1/messages?beta=true"
    );
}

#[test]
fn minimax_models_url_points_at_v1_models() {
    let a = MiniMaxAdapter::new();
    assert_eq!(
        a.models_url().as_deref(),
        Some("https://api.minimax.io/v1/models")
    );
}

#[test]
fn minimax_builds_anthropic_headers() {
    let a = MiniMaxAdapter::new();
    let headers = a.build_headers("k", TargetFormat::Anthropic, &ModelId::new("m"));
    assert_eq!(first_header(&headers, "Authorization"), Some("Bearer k"));
    assert_eq!(
        first_header(&headers, "Content-Type"),
        Some("application/json")
    );
    assert_eq!(
        first_header(&headers, "Anthropic-Version"),
        Some("2023-06-01")
    );
}

// ---- OpenCode Zen ------------------------------------------------

#[test]
fn opencode_zen_routes_anthropic_to_messages() {
    let a = OpenCodeZenAdapter::new();
    let url = a.build_chat_url(TargetFormat::Anthropic, &ModelId::new("m"));
    assert_eq!(url, "https://opencode.ai/zen/v1/messages");
}

#[test]
fn opencode_zen_routes_openai_to_chat_completions() {
    let a = OpenCodeZenAdapter::new();
    let url = a.build_chat_url(TargetFormat::Openai, &ModelId::new("m"));
    assert_eq!(url, "https://opencode.ai/zen/v1/chat/completions");
}

#[test]
fn opencode_zen_uses_x_api_key_for_anthropic() {
    let a = OpenCodeZenAdapter::new();
    let headers = a.build_headers("k-anthropic", TargetFormat::Anthropic, &ModelId::new("m"));
    assert_eq!(first_header(&headers, "x-api-key"), Some("k-anthropic"));
    // No Bearer auth on the Anthropic branch.
    assert!(first_header(&headers, "Authorization").is_none());
    // Anthropic-Version must be present.
    assert_eq!(
        first_header(&headers, "Anthropic-Version"),
        Some("2023-06-01")
    );
}

#[test]
fn opencode_zen_uses_bearer_for_openai() {
    let a = OpenCodeZenAdapter::new();
    let headers = a.build_headers("k-openai", TargetFormat::Openai, &ModelId::new("m"));
    assert_eq!(
        first_header(&headers, "Authorization"),
        Some("Bearer k-openai")
    );
    // No x-api-key on the OpenAI branch.
    assert!(first_header(&headers, "x-api-key").is_none());
    // No Anthropic-Version on the OpenAI branch.
    assert!(first_header(&headers, "Anthropic-Version").is_none());
}

#[test]
fn opencode_zen_uses_public_auth_when_key_empty() {
    let a = OpenCodeZenAdapter::new();
    let headers = a.build_headers("", TargetFormat::Openai, &ModelId::new("m"));
    // OpenCode free tier uses "Bearer public" when key is empty.
    assert_eq!(
        first_header(&headers, "Authorization"),
        Some("Bearer public")
    );
    assert!(first_header(&headers, "x-api-key").is_none());
    // Content-Type and User-Agent are still present.
    assert_eq!(
        first_header(&headers, "Content-Type"),
        Some("application/json")
    );
    assert_eq!(
        first_header(&headers, "User-Agent"),
        Some(crate::spoofer::OPENCODE_UA)
    );
    assert_eq!(first_header(&headers, "x-opencode-client"), Some("cli"));
    assert_eq!(first_header(&headers, "x-opencode-project"), Some("global"));
    assert!(first_header(&headers, "opencode-version").is_none());
    assert!(first_header(&headers, "openai-beta").is_none());

    // Anthropic format uses "x-api-key: public"
    let anthropic_headers = a.build_headers("", TargetFormat::Anthropic, &ModelId::new("m"));
    assert_eq!(
        first_header(&anthropic_headers, "x-api-key"),
        Some("public")
    );
    assert_eq!(
        first_header(&anthropic_headers, "Anthropic-Version"),
        Some("2023-06-01")
    );
    assert!(first_header(&anthropic_headers, "Authorization").is_none());
}

#[test]
fn opencode_zen_headers_have_user_agent_and_content_type() {
    let a = OpenCodeZenAdapter::new();
    for fmt in [TargetFormat::Openai, TargetFormat::Anthropic] {
        let headers = a.build_headers("k", fmt, &ModelId::new("m"));
        assert_eq!(
            first_header(&headers, "User-Agent"),
            Some(crate::spoofer::OPENCODE_UA)
        );
        assert_eq!(first_header(&headers, "x-opencode-client"), Some("cli"));
        assert_eq!(first_header(&headers, "x-opencode-project"), Some("global"));
        assert!(first_header(&headers, "opencode-version").is_none());
        assert!(first_header(&headers, "openai-beta").is_none());
        assert_eq!(
            first_header(&headers, "Content-Type"),
            Some("application/json")
        );
        assert!(has_header(&headers, "Content-Type"));
    }
}

#[test]
fn opencode_zen_models_url() {
    let a = OpenCodeZenAdapter::new();
    assert_eq!(
        a.models_url().as_deref(),
        Some("https://opencode.ai/zen/v1/models")
    );
}

#[test]
fn classify_zen_target_format_heuristic() {
    assert_eq!(
        classify_zen_target_format("claude-sonnet-4"),
        TargetFormat::Anthropic
    );
    assert_eq!(
        classify_zen_target_format("MiniMax-M2"),
        TargetFormat::Anthropic
    );
    assert_eq!(classify_zen_target_format("gpt-4o"), TargetFormat::Openai);
    assert_eq!(
        classify_zen_target_format("llama-3.1-70b"),
        TargetFormat::Openai
    );
}

// ---- Factory & Enums -------------------------------------------------

#[test]
fn builtin_adapters_returns_all() {
    let v = builtin_adapters();
    assert_eq!(v.len(), 21);
    let ids: Vec<&str> = v.iter().map(|a| a.id().as_str()).collect();
    assert!(ids.contains(&"atomesus"));
    assert!(ids.contains(&"cline"));
    assert!(ids.contains(&"commandcodego"));
    assert!(ids.contains(&"openrouter"));
    assert!(ids.contains(&"minimax"));
    assert!(ids.contains(&"opencode-zen"));
    assert!(ids.contains(&"opencode-go"));
    assert!(ids.contains(&"ollama-cloud"));
    assert!(ids.contains(&"nous-research"));
    assert!(ids.contains(&"nvidia-nim"));
    assert!(ids.contains(&"kilocode"));
    assert!(ids.contains(&"cloudflare-workers-ai"));
    assert!(ids.contains(&"gemini"));
    assert!(ids.contains(&"horde"));
    assert!(ids.contains(&"antigravity"));
    assert!(ids.contains(&"codex"));
    assert!(ids.contains(&"kiro"));
    assert!(ids.contains(&"vercel-gateway"));
    assert!(ids.contains(&"zai"));
    assert!(ids.contains(&"typesafe"));
    assert!(ids.contains(&"laya"));
}

#[test]
fn every_builtin_variant_resolves_from_provider_id() {
    for adapter in builtin_adapters() {
        let id = adapter.id().as_str();
        assert!(
            ProviderAdapterEnum::from_provider_id(id).is_some(),
            "variant for `{id}` exists in builtin_adapters() but not in from_provider_id()"
        );
    }
}

#[test]
fn test_build_discovered_model_helpers() {
    let m1 = build_discovered_model_full(
        "gemini-1.5-pro".to_string(),
        Some("Gemini 1.5 Pro".to_string()),
        TargetFormat::Gemini,
        Some(2_000_000),
        Some(8192),
    );
    assert_eq!(m1.model_id.as_str(), "gemini-1.5-pro");
    assert_eq!(m1.display_name.as_deref(), Some("Gemini 1.5 Pro"));
    assert_eq!(m1.target_format, TargetFormat::Gemini);
    assert_eq!(m1.context_length, Some(2_000_000));
    assert_eq!(m1.max_output_tokens, Some(8192));
    assert!(m1.capabilities.is_some());

    let m2 = build_discovered_model_with("gpt-4o".to_string(), TargetFormat::Openai);
    assert_eq!(m2.model_id.as_str(), "gpt-4o");
    assert_eq!(m2.display_name.as_deref(), Some("gpt-4o"));
    assert_eq!(m2.target_format, TargetFormat::Openai);
    assert_eq!(m2.context_length, None);
    assert_eq!(m2.max_output_tokens, None);
    assert!(m2.capabilities.is_some());
}

#[test]
fn test_header_name_mapping() {
    assert_eq!(
        header_name("Authorization"),
        Some(http::header::AUTHORIZATION)
    );
    assert_eq!(
        header_name("authorization"),
        Some(http::header::AUTHORIZATION)
    );
    assert_eq!(
        header_name("Content-Type"),
        Some(http::header::CONTENT_TYPE)
    );
    assert_eq!(
        header_name("content-type"),
        Some(http::header::CONTENT_TYPE)
    );
    assert_eq!(header_name("User-Agent"), Some(http::header::USER_AGENT));
    assert_eq!(header_name("user-agent"), Some(http::header::USER_AGENT));
    assert_eq!(
        header_name("X-Api-Key"),
        Some(http::HeaderName::from_static("x-api-key"))
    );
    assert_eq!(
        header_name("x-api-key"),
        Some(http::HeaderName::from_static("x-api-key"))
    );
    assert_eq!(
        header_name("X-Goog-Api-Key"),
        Some(http::HeaderName::from_static("x-goog-api-key"))
    );
    assert_eq!(
        header_name("x-goog-api-key"),
        Some(http::HeaderName::from_static("x-goog-api-key"))
    );
    assert_eq!(header_name("unknown-header"), None);
}

#[test]
fn test_is_anonymous_fallback() {
    assert!(is_anonymous_fallback("horde"));
    assert!(is_anonymous_fallback("opencode-go"));
    assert!(is_anonymous_fallback("opencode-zen"));
    assert!(!is_anonymous_fallback("openrouter"));
    assert!(!is_anonymous_fallback("gemini"));
    assert!(!is_anonymous_fallback("nonexistent"));

    let horde = ProviderAdapterEnum::from_provider_id("horde").unwrap();
    assert!(horde.is_anonymous_fallback());
    let opencode_go = ProviderAdapterEnum::from_provider_id("opencode-go").unwrap();
    assert!(opencode_go.is_anonymous_fallback());
    let opencode_zen = ProviderAdapterEnum::from_provider_id("opencode-zen").unwrap();
    assert!(opencode_zen.is_anonymous_fallback());
    let openrouter = ProviderAdapterEnum::from_provider_id("openrouter").unwrap();
    assert!(!openrouter.is_anonymous_fallback());
}

#[test]
fn test_models_dev_canonical_ids() {
    let gemini = ProviderAdapterEnum::from_provider_id("gemini").unwrap();
    assert_eq!(gemini.models_dev_canonical_ids(), &["google"]);

    let minimax = ProviderAdapterEnum::from_provider_id("minimax").unwrap();
    assert_eq!(minimax.models_dev_canonical_ids(), &["minimax"]);

    let openrouter = ProviderAdapterEnum::from_provider_id("openrouter").unwrap();
    assert!(openrouter.models_dev_canonical_ids().contains(&"openai"));
    assert!(openrouter.models_dev_canonical_ids().contains(&"anthropic"));
    assert!(openrouter.models_dev_canonical_ids().contains(&"meta"));

    let nvidia = ProviderAdapterEnum::from_provider_id("nvidia-nim").unwrap();
    assert_eq!(nvidia.models_dev_canonical_ids(), &["nvidia"]);

    let opencode_zen = ProviderAdapterEnum::from_provider_id("opencode-zen").unwrap();
    assert_eq!(opencode_zen.models_dev_canonical_ids(), &["opencode"]);

    let opencode_go = ProviderAdapterEnum::from_provider_id("opencode-go").unwrap();
    assert_eq!(opencode_go.models_dev_canonical_ids(), &["opencode-go"]);
}

#[test]
fn test_inject_model_and_serialize() {
    let input = serde_json::json!({
        "model": "old-model",
        "prompt": "hello world"
    });
    let bytes = inject_model_and_serialize(&input, "new-model").unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(parsed["model"], "new-model");
    assert_eq!(parsed["prompt"], "hello world");
}
