use super::first_header;
use crate::adapters::*;
use crate::upstream::UpstreamClient;
use openproxy_types::{CoreError, ModelId, TargetFormat};
use std::sync::Arc;

// ---- Cloudflare Workers AI ---------------------------------------

#[tokio::test]
async fn cloudflare_fetch_models_for_account_rejects_empty_label() {
    let a = CloudflareWorkersAIAdapter::new();
    let upstream = Arc::new(UpstreamClient::new());
    let res = a
        .fetch_models_for_account(&upstream, "cf-test-key", "")
        .await;
    let CoreError::Validation(msg) = res.unwrap_err() else {
        panic!("expected CoreError::Validation");
    };
    assert!(
        msg.contains("account label is empty"),
        "error message should explain the empty-label root cause, got: {msg}",
    );
}

#[test]
fn cloudflare_models_url_for_account_builds_expected_path() {
    let a = CloudflareWorkersAIAdapter::new();
    assert_eq!(
        a.models_url_for_account("abc123").as_deref(),
        Some("https://api.cloudflare.com/client/v4/accounts/abc123/ai/models/search"),
    );
}

#[test]
fn cloudflare_models_url_for_account_rejects_empty_label() {
    let a = CloudflareWorkersAIAdapter::new();
    assert_eq!(a.models_url_for_account("").as_deref(), None);
}

#[test]
fn cloudflare_build_chat_url_for_account_substitutes_label() {
    let a = CloudflareWorkersAIAdapter::new();
    let url = a.build_chat_url_for_account(
        TargetFormat::Openai,
        &ModelId::new("@cf/meta/llama-3.1-8b-instruct"),
        "abc123",
    );
    assert_eq!(
        url,
        "https://api.cloudflare.com/client/v4/accounts/abc123/ai/v1/chat/completions",
    );
}

// ---- Ollama Cloud ------------------------------------------------

#[test]
fn ollama_cloud_builds_correct_url() {
    let a = OllamaCloudAdapter::new();
    let url = a.build_chat_url(TargetFormat::Openai, &ModelId::new("any"));
    assert_eq!(url, "https://ollama.com/v1/chat/completions");
}

#[test]
fn ollama_cloud_builds_bearer_auth() {
    let a = OllamaCloudAdapter::new();
    let (name, value) = a.build_auth_header("test-key").unwrap();
    assert_eq!(name, "Authorization");
    assert_eq!(value, "Bearer test-key");
}

#[test]
fn ollama_cloud_models_url() {
    let a = OllamaCloudAdapter::new();
    assert_eq!(
        a.models_url().as_deref(),
        Some("https://ollama.com/api/tags")
    );
}

#[test]
fn ollama_cloud_headers() {
    let a = OllamaCloudAdapter::new();
    let headers = a.build_headers("k", TargetFormat::Openai, &ModelId::new("any"));
    assert_eq!(first_header(&headers, "Authorization"), Some("Bearer k"));
    assert_eq!(
        first_header(&headers, "Content-Type"),
        Some("application/json")
    );
}

// ---- Nous Research ------------------------------------------------

#[test]
fn nous_research_builds_correct_url() {
    let a = NousResearchAdapter::new();
    let url = a.build_chat_url(TargetFormat::Openai, &ModelId::new("Hermes-4-405B"));
    assert_eq!(
        url,
        "https://inference-api.nousresearch.com/v1/chat/completions"
    );
}

#[test]
fn nous_research_builds_bearer_auth() {
    let a = NousResearchAdapter::new();
    let (name, value) = a.build_auth_header("nr-key").unwrap();
    assert_eq!(name, "Authorization");
    assert_eq!(value, "Bearer nr-key");
}

#[test]
fn nous_research_models_url() {
    let a = NousResearchAdapter::new();
    assert_eq!(
        a.models_url().as_deref(),
        Some("https://inference-api.nousresearch.com/v1/models")
    );
}

#[test]
fn nous_research_headers() {
    let a = NousResearchAdapter::new();
    let headers = a.build_headers("k", TargetFormat::Openai, &ModelId::new("any"));
    assert_eq!(first_header(&headers, "Authorization"), Some("Bearer k"));
    assert_eq!(
        first_header(&headers, "Content-Type"),
        Some("application/json")
    );
}

// ---- NVIDIA NIM ---------------------------------------------------

#[test]
fn nvidia_nim_builds_correct_url() {
    let a = NvidiaNimAdapter::new();
    let url = a.build_chat_url(
        TargetFormat::Openai,
        &ModelId::new("nvidia/nemotron-3-super-120b-a12b"),
    );
    assert_eq!(url, "https://integrate.api.nvidia.com/v1/chat/completions");
}

#[test]
fn nvidia_nim_builds_bearer_auth() {
    let a = NvidiaNimAdapter::new();
    let (name, value) = a.build_auth_header("nvapi-test").unwrap();
    assert_eq!(name, "Authorization");
    assert_eq!(value, "Bearer nvapi-test");
}

#[test]
fn nvidia_nim_models_url() {
    let a = NvidiaNimAdapter::new();
    assert_eq!(
        a.models_url().as_deref(),
        Some("https://integrate.api.nvidia.com/v1/models")
    );
}

#[test]
fn nvidia_nim_headers() {
    let a = NvidiaNimAdapter::new();
    let headers = a.build_headers("k", TargetFormat::Openai, &ModelId::new("any"));
    assert_eq!(first_header(&headers, "Authorization"), Some("Bearer k"));
    assert_eq!(
        first_header(&headers, "Content-Type"),
        Some("application/json")
    );
}

// ---- Kilocode -----------------------------------------------------

#[test]
fn kilocode_builds_correct_url() {
    let a = KilocodeAdapter::new();
    let url = a.build_chat_url(TargetFormat::Openai, &ModelId::new("openai/gpt-5.5"));
    assert_eq!(
        url,
        "https://api.kilo.ai/api/openrouter/v1/chat/completions"
    );
}

#[test]
fn kilocode_builds_bearer_auth() {
    let a = KilocodeAdapter::new();
    let (name, value) = a.build_auth_header("kl-key").unwrap();
    assert_eq!(name, "Authorization");
    assert_eq!(value, "Bearer kl-key");
}

#[test]
fn kilocode_models_url() {
    let a = KilocodeAdapter::new();
    assert_eq!(
        a.models_url().as_deref(),
        Some("https://api.kilo.ai/api/openrouter/models")
    );
}

#[test]
fn kilocode_headers() {
    let a = KilocodeAdapter::new();
    let headers = a.build_headers("k", TargetFormat::Openai, &ModelId::new("any"));
    assert_eq!(first_header(&headers, "Authorization"), Some("Bearer k"));
    assert_eq!(
        first_header(&headers, "Content-Type"),
        Some("application/json")
    );
}

// ---- Gemini -------------------------------------------------------

#[test]
fn gemini_builds_correct_url() {
    let a = GeminiAdapter::new();
    let url = a.build_chat_url(TargetFormat::Gemini, &ModelId::new("gemini-2.5-flash"));
    assert_eq!(
        url,
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
    );
}

#[test]
fn gemini_builds_goog_api_key_auth() {
    let a = GeminiAdapter::new();
    let (name, value) = a.build_auth_header("AIzaSyTest123").unwrap();
    assert_eq!(name, "x-goog-api-key");
    assert_eq!(value, "AIzaSyTest123");
}

#[test]
fn gemini_models_url() {
    let a = GeminiAdapter::new();
    assert_eq!(
        a.models_url().as_deref(),
        Some("https://generativelanguage.googleapis.com/v1beta/models")
    );
}

#[test]
fn gemini_headers_include_content_type() {
    let a = GeminiAdapter::new();
    let headers = a.build_headers("k", TargetFormat::Gemini, &ModelId::new("any"));
    assert_eq!(first_header(&headers, "x-goog-api-key"), Some("k"));
    assert_eq!(
        first_header(&headers, "Content-Type"),
        Some("application/json")
    );
}

// ---- Atomesus -------------------------------------------------------

#[tokio::test]
async fn atomesus_fetch_models_returns_expected_catalog() {
    let a = AtomesusAdapter::new();
    let upstream = Arc::new(UpstreamClient::new());
    let models = a.fetch_models(&upstream, "dummy-key").await.unwrap();
    assert_eq!(models.len(), 6);
    let ids: Vec<&str> = models.iter().map(|m| m.model_id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "atomesus-1-5-fast",
            "atomesus-1-5-thinking",
            "atomesus-2-fast",
            "atomesus-2-thinking",
            "cipher-fast",
            "cipher-thinking",
        ]
    );
}

// ---- Antigravity ---------------------------------------------------

#[test]
fn antigravity_builds_correct_url() {
    let a = AntigravityAdapter::new();
    let url = a.build_chat_url(TargetFormat::Gemini, &ModelId::new("claude-opus-4-6"));
    assert_eq!(
        url,
        "https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse"
    );
}

#[test]
fn antigravity_builds_bearer_auth() {
    let a = AntigravityAdapter::new();
    let (name, value) = a.build_auth_header("ya29.test-token").unwrap();
    assert_eq!(name, "Authorization");
    assert_eq!(value, "Bearer ya29.test-token");
}

#[test]
fn antigravity_has_no_models_url() {
    let a = AntigravityAdapter::new();
    assert!(a.models_url().is_none());
}
