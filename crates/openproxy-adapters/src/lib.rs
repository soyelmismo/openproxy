pub mod adapters;
pub mod upstream;
pub mod antigravity_headers;

pub use adapters::{
    ProviderAdapter, ProviderAdapterConfig, AdapterAuthType, AdapterFormat, ProviderAdapterEnum,
    builtin_adapters, custom_adapter::CustomAdapter, openrouter::OpenRouterAdapter,
    minimax::MiniMaxAdapter, opencode_zen::OpenCodeZenAdapter, ollama_cloud::OllamaCloudAdapter,
    nous_research::NousResearchAdapter, nvidia_nim::NvidiaNimAdapter, kilocode::KilocodeAdapter,
    cloudflare_workers_ai::CloudflareWorkersAIAdapter, gemini::GeminiAdapter,
    antigravity::AntigravityAdapter, kiro_ai::KiroAdapter, codex::CodexAdapter,
};
#[cfg(any(test, feature = "test-utils"))]
pub use adapters::MockAdapter;

pub use upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest, HostKey, Scheme,
    UpstreamConnectionPool, PhasedConnector, PhasedConnectorError, PhasedTimeouts,
    ResolvedPhaseDeadlines, UpstreamPhase, ResolvedTimeouts, UpstreamBodyStream, UpstreamResponse,
    UpstreamError, UpstreamResult,
};
