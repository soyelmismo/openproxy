//! Provider adapter trait + built-in adapters (OpenRouter, MiniMax Coding, OpenCode Zen, etc.).
//!
//! See mvp-spec §3 (per-provider config) and §5 (the trait surface).

#[macro_use]
pub mod macros;
pub mod discovery;
pub mod traits;

#[cfg(test)]
mod tests;

pub use crate::upstream::{CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest};
pub use bytes::Bytes;
pub use http::HeaderValue;
pub use openproxy_types::{
    CoreError, DiscoveredModel, ModelId, ProviderId, ProviderMetadata, Result, TargetFormat,
};
pub use serde::{Deserialize, Serialize};
pub use std::sync::Arc;

pub use discovery::*;
pub use macros::*;
pub use traits::*;

pub mod antigravity;
pub mod atomesus;
pub mod cline;
pub mod cloudflare_workers_ai;
pub mod codex;
pub mod commandcode;
pub mod custom_adapter;
pub mod factory;
pub mod gemini;
pub mod horde;
pub mod kilocode;
pub mod kiro_ai;
pub mod laya;
pub mod minimax;
#[cfg(any(test, feature = "test-utils"))]
pub mod mock;
pub mod nous_research;
pub mod nvidia_nim;
pub mod ollama_cloud;
pub mod opencode_common;
pub mod openrouter;
pub mod typesafe;
pub mod vercel_gateway;
pub mod zai;

#[cfg(any(test, feature = "test-utils"))]
pub use mock::MockAdapter;

pub use custom_adapter::CustomAdapter;
pub use factory::AdapterFactory;

pub fn is_anonymous_fallback(provider_id: &str) -> bool {
    ProviderAdapterEnum::from_provider_id(provider_id)
        .is_some_and(|adapter| adapter.is_anonymous_fallback())
}

define_provider_adapter! {
    pub enum ProviderAdapterEnum {
        builtins {
            "antigravity" => Antigravity(antigravity, AntigravityAdapter),
            "atomesus" => Atomesus(atomesus, AtomesusAdapter),
            "cline" => Cline(cline, ClineAdapter),
            "cloudflare-workers-ai" => CloudflareWorkersAI(cloudflare_workers_ai, CloudflareWorkersAIAdapter),
            "codex" => Codex(codex, CodexAdapter),
            "commandcodego" => CommandCodeGo(commandcode, CommandCodeGoAdapter),
            "gemini" => Gemini(gemini, GeminiAdapter),
            "horde" => Horde(horde, HordeAdapter),
            "kilocode" => Kilocode(kilocode, KilocodeAdapter),
            "kiro" => Kiro(kiro_ai, KiroAdapter),
            "laya" => Laya(laya, LayaAdapter),
            "minimax" => MiniMax(minimax, MiniMaxAdapter),
            "nous-research" => NousResearch(nous_research, NousResearchAdapter),
            "nvidia-nim" => NvidiaNim(nvidia_nim, NvidiaNimAdapter),
            "ollama-cloud" => OllamaCloud(ollama_cloud, OllamaCloudAdapter),
            "opencode-go" => OpenCodeGo(opencode_common, OpenCodeGoAdapter),
            "opencode-zen" => OpenCodeZen(opencode_common, OpenCodeZenAdapter),
            "openrouter" => OpenRouter(openrouter, OpenRouterAdapter),
            "typesafe" => TypeSafe(typesafe, TypeSafeAdapter),
            "vercel-gateway" => VercelGateway(vercel_gateway, VercelGatewayAdapter),
            "zai" => Zai(zai, ZaiAdapter),
        }
        custom {
            Custom(custom_adapter, CustomAdapter),
            #[cfg(any(test, feature = "test-utils"))]
            Mock(mock, MockAdapter),
        }
    }
}
