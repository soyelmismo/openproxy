//! EAC Gateway model definitions.

use crate::adapters::{DiscoveredModel, TargetFormat, discovery::build_discovered_model_full};

/// Built-in catalog of EAC Gateway models.
pub fn eac_builtin_models() -> Vec<DiscoveredModel> {
    vec![
        build_discovered_model_full(
            "z-ai/glm-5.3".into(),
            Some("GLM-5.3".into()),
            TargetFormat::Openai,
            Some(128_000),
            Some(8_192),
        ),
        build_discovered_model_full(
            "z-ai/glm-5.3-flash".into(),
            Some("GLM-5.3 Flash".into()),
            TargetFormat::Openai,
            Some(128_000),
            Some(8_192),
        ),
        build_discovered_model_full(
            "deepseek-ai/deepseek-v4.1-flash".into(),
            Some("DeepSeek V4.1 Flash".into()),
            TargetFormat::Openai,
            Some(128_000),
            Some(8_192),
        ),
        build_discovered_model_full(
            "moonshotai/kimi-k3".into(),
            Some("Kimi K3".into()),
            TargetFormat::Openai,
            Some(256_000),
            Some(8_192),
        ),
        build_discovered_model_full(
            "moonshotai/kimi-k2.6".into(),
            Some("Kimi K2.6".into()),
            TargetFormat::Openai,
            Some(256_000),
            Some(8_192),
        ),
        build_discovered_model_full(
            "openai/gpt-oss-20b".into(),
            Some("GPT OSS 20B".into()),
            TargetFormat::Openai,
            Some(128_000),
            Some(8_192),
        ),
        build_discovered_model_full(
            "opencode/glm-5.3-flash-free".into(),
            Some("GLM-5.3 Flash Free".into()),
            TargetFormat::Openai,
            Some(128_000),
            Some(8_192),
        ),
        build_discovered_model_full(
            "opencode/deepseek-v4.1-flash-free".into(),
            Some("DeepSeek V4.1 Flash Free".into()),
            TargetFormat::Openai,
            Some(128_000),
            Some(8_192),
        ),
    ]
}
