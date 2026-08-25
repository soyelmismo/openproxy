#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod adapters;
pub mod antigravity_headers;
pub mod spoofer;
pub mod upstream;

pub use spoofer::{AntigravitySpoofer, ClientSpoofer, ClineSpoofer, OpenCodeSpoofer};

#[cfg(any(test, feature = "test-utils"))]
pub use adapters::MockAdapter;
pub use adapters::{
    AdapterAuthType, AdapterFactory, AdapterFormat, ProviderAdapter, ProviderAdapterConfig,
    ProviderAdapterEnum, antigravity::AntigravityAdapter, atomesus::AtomesusAdapter,
    build_discovered_model_full, build_discovered_model_with, builtin_adapters,
    cline::ClineAdapter, cloudflare_workers_ai::CloudflareWorkersAIAdapter, codex::CodexAdapter,
    custom_adapter::CustomAdapter, gemini::GeminiAdapter, horde::HordeAdapter,
    kilocode::KilocodeAdapter, kiro_ai::KiroAdapter, minimax::MiniMaxAdapter,
    nous_research::NousResearchAdapter, nvidia_nim::NvidiaNimAdapter,
    ollama_cloud::OllamaCloudAdapter, opencode_common::OpenCodeGoAdapter,
    opencode_common::OpenCodeZenAdapter, openrouter::OpenRouterAdapter,
};

pub use upstream::{
    CancellationToken, HostKey, PhasedConnector, PhasedConnectorError, PhasedTimeouts,
    ResolvedPhaseDeadlines, ResolvedTimeouts, Scheme, TimeoutProfile, UpstreamBodyStream,
    UpstreamClient, UpstreamConnectionPool, UpstreamError, UpstreamPhase, UpstreamRequest,
    UpstreamResponse, UpstreamResult,
};
