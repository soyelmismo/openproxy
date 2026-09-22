#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod adapters;
pub mod antigravity_headers;
pub mod schema_cleaner;
pub mod spoofer;
pub mod upstream;

pub use spoofer::{
    AntigravitySpoofer, ClientSpoofer, ClineSpoofer, CodeBuddySpoofer, CodexSpoofer, CommandCodeSpoofer,
    KilocodeSpoofer, KiroSpoofer, MiniMaxSpoofer, OpenCodeSpoofer,
};

#[cfg(any(test, feature = "test-utils"))]
pub use adapters::MockAdapter;
#[cfg(feature = "laya-engine")]
pub use adapters::laya_engine;
pub use adapters::{
    AdapterAuthType, AdapterFactory, AdapterFormat, ProviderAdapter, ProviderAdapterConfig,
    ProviderAdapterEnum, antigravity::AntigravityAdapter, atomesus::AtomesusAdapter,
    build_discovered_model_full, build_discovered_model_with, builtin_adapters,
    cline::ClineAdapter, cloudflare_workers_ai::CloudflareWorkersAIAdapter,
    codebuddy::CodeBuddyAdapter, codebuddy::apply_codebuddy_spoofing_headers, codex::CodexAdapter,
    commandcode::CommandCodeGoAdapter, commandcode::apply_commandcode_cli_headers,
    custom_adapter::CustomAdapter, gemini::GeminiAdapter, horde::HordeAdapter,
    kilocode::KilocodeAdapter, kiro_ai::KiroAdapter, laya::LayaAdapter, minimax::MiniMaxAdapter,
    nous_research::NousResearchAdapter, nvidia_nim::NvidiaNimAdapter,
    ollama_cloud::OllamaCloudAdapter, opencode_common::OpenCodeGoAdapter,
    opencode_common::OpenCodeZenAdapter, openrouter::OpenRouterAdapter, typesafe::TypeSafeAdapter,
    zai::ZaiAdapter,
};
#[cfg(any(test, feature = "test-utils"))]
pub use upstream::load_upstream_source;

pub use upstream::{
    CancellationToken, HostKey, NON_STREAMING_BODY_LIMIT_BYTES, PhasedConnector,
    PhasedConnectorError, PhasedTimeouts, ResolvedPhaseDeadlines, ResolvedTimeouts,
    STREAMING_BODY_LIMIT_BYTES, Scheme, TimeoutProfile, UpstreamBodyStream, UpstreamClient,
    UpstreamConnectionPool, UpstreamError, UpstreamPhase, UpstreamRequest, UpstreamResponse,
    UpstreamResult,
};
