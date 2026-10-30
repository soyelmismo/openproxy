//! Static Devin catalog data — the curated fallback ported from upstream
//! `devin_models.json` and corrected against the live
//! `client_model_configs` feed (`catalog.rs`).
//!
//! This table stays as the cold-start/failed-refresh fallback and as the
//! source for legacy enum aliases the live feed never carries
//! (`MODEL_PRIVATE_*`, `MODEL_GPT_5_2_*`, …). Once a `GetUserStatus`
//! response arrives, the live snapshot supersedes every value here.

/// Max completion tokens every catalog entry advertises. Live rows carry
/// the real per-model ceiling (swe-2: 128k, nemotron-3-ultra: 32k).
pub const DEVIN_MAX_COMPLETION_TOKENS: i64 = 64_000;

pub const DEFAULT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
pub const MED_HI_MAX: &[&str] = &["medium", "high", "max"];
pub const NONE_TO_MAX: &[&str] = &["none", "low", "medium", "high", "xhigh", "max"];
pub const NONE_MAX: &[&str] = &["none", "max"];
pub const MEDIUM_ONLY: &[&str] = &["medium"];
pub const THINKING_ONLY: &[&str] = &["thinking"];
// Per-model effort vocabularies from upstream `devin_models.json` and the
// live `client_model_configs` feed — both agree on these restricted sets.
pub const LOW_HI_MAX: &[&str] = &["low", "high", "max"];
pub const LOW_MED_HI: &[&str] = &["low", "medium", "high"];
pub const MIN_LOW_MED_HI: &[&str] = &["minimal", "low", "medium", "high"];
pub const LOW_MED_HI_XHI: &[&str] = &["low", "medium", "high", "xhigh"];
pub const NONE_LOW_MED_HI_XHI: &[&str] = &["none", "low", "medium", "high", "xhigh"];
pub const MIN_LOW_MED_HI_XHI_MAX: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];
pub const HI_MAX: &[&str] = &["high", "max"];
pub const NONE_LOW_HI_MAX: &[&str] = &["none", "low", "high", "max"];
pub const NONE_MED_HI: &[&str] = &["none", "medium", "high"];
/// Models with `thinking: null` — the wire uid is the bare id, no suffix.
pub const NO_LEVELS: &[&str] = &[];

pub struct DevinModelSpec {
    pub id: &'static str,
    pub display_name: &'static str,
    pub owner: &'static str,
    pub context_length: i64,
    /// Allowed thinking-effort levels for the uid suffix.
    pub thinking_levels: &'static [&'static str],
}

/// Catalog row: (id, display name, owner, context length, thinking levels).
pub const fn spec(
    id: &'static str,
    display_name: &'static str,
    owner: &'static str,
    context_length: i64,
    thinking_levels: &'static [&'static str],
) -> DevinModelSpec {
    DevinModelSpec {
        id,
        display_name,
        owner,
        context_length,
        thinking_levels,
    }
}

/// Static Devin catalog, ported from upstream `devin_models.json` (46 models).
/// No upstream models-list RPC exists: the Codeium backend only serves
/// `GetChatMessage`/`GetUserStatus`, so the catalog is maintained here and in
/// upstream's embedded JSON.
pub const CATALOG: &[DevinModelSpec] = &[
    spec("swe-2", "SWE-2", "cognition", 262_000, MED_HI_MAX),
    spec(
        "claude-fable-5-1",
        "Claude Fable 5.1",
        "anthropic",
        1_000_000,
        DEFAULT_LEVELS,
    ),
    spec(
        "claude-opus-5-5",
        "Claude Opus 5.5",
        "anthropic",
        1_000_000,
        DEFAULT_LEVELS,
    ),
    spec(
        "gpt-6-astra",
        "GPT-6 Astra",
        "openai",
        1_000_000,
        DEFAULT_LEVELS,
    ),
    spec("gpt-6-sol", "GPT-6 Sol", "openai", 1_000_000, NONE_TO_MAX),
    spec("gpt-6-luna", "GPT-6 Luna", "openai", 1_000_000, NONE_TO_MAX),
    spec(
        "gemini-4-argon",
        "Gemini 4 Argon",
        "google",
        1_048_576,
        LOW_MED_HI,
    ),
    spec("kimi-k3", "Kimi K3", "moonshot", 1_048_576, LOW_HI_MAX),
    spec("glm-5-2", "GLM 5.2", "zhipu", 200_000, NONE_MAX),
    spec("glm-5-3", "GLM 5.3", "zhipu", 1_048_576, LOW_HI_MAX),
    spec(
        "claude-sonnet-5-5",
        "Claude Sonnet 5.5",
        "anthropic",
        1_000_000,
        DEFAULT_LEVELS,
    ),
    spec(
        "gemini-3-8-flash",
        "Gemini 3.8 Flash",
        "google",
        1_048_576,
        LOW_MED_HI,
    ),
    spec(
        "claude-opus-4-7",
        "Claude Opus 4.7",
        "anthropic",
        1_000_000,
        DEFAULT_LEVELS,
    ),
    spec(
        "claude-opus-4-8",
        "Claude Opus 4.8",
        "anthropic",
        1_000_000,
        DEFAULT_LEVELS,
    ),
    spec(
        "claude-opus-5",
        "Claude Opus 5",
        "anthropic",
        1_000_000,
        DEFAULT_LEVELS,
    ),
    spec(
        "claude-5-fable",
        "Claude 5 Fable",
        "anthropic",
        1_000_000,
        DEFAULT_LEVELS,
    ),
    spec(
        "claude-sonnet-5",
        "Claude Sonnet 5",
        "anthropic",
        1_000_000,
        DEFAULT_LEVELS,
    ),
    spec(
        "claude-haiku-5-5",
        "Claude Haiku 5.5",
        "anthropic",
        1_000_000,
        DEFAULT_LEVELS,
    ),
    spec(
        "gemini-3-5-flash",
        "Gemini 3.5 Flash",
        "google",
        1_048_576,
        MIN_LOW_MED_HI,
    ),
    spec(
        "gemini-3-6-flash",
        "Gemini 3.6 Flash",
        "google",
        1_048_576,
        MIN_LOW_MED_HI,
    ),
    spec(
        "gemini-3-7-flash",
        "Gemini 3.7 Flash",
        "google",
        1_048_576,
        LOW_MED_HI,
    ),
    spec(
        "gpt-5-6-sol",
        "GPT 5.6 Sol",
        "openai",
        1_000_000,
        DEFAULT_LEVELS,
    ),
    spec(
        "gpt-5-6-terra",
        "GPT 5.6 Terra",
        "openai",
        1_000_000,
        DEFAULT_LEVELS,
    ),
    spec(
        "gpt-5-6-luna",
        "GPT 5.6 Luna",
        "openai",
        1_000_000,
        DEFAULT_LEVELS,
    ),
    spec(
        "gpt-6-1-sol",
        "GPT 6.1 Sol",
        "openai",
        1_000_000,
        DEFAULT_LEVELS,
    ),
    spec("glm-5-2-1m", "GLM 5.2 1M", "zhipu", 1_000_000, NONE_MAX),
    spec("grok-4-6", "Grok 4.6", "xai", 500_000, LOW_MED_HI_XHI),
    spec("grok-4-7", "Grok 4.7", "xai", 500_000, LOW_MED_HI_XHI),
    spec(
        "muse-wonder-1-0",
        "Muse Wonder 1.0",
        "devin",
        1_048_576,
        MIN_LOW_MED_HI_XHI_MAX,
    ),
    spec(
        "glm-5-3-flash",
        "GLM 5.3 Flash",
        "zhipu",
        1_000_000,
        LOW_HI_MAX,
    ),
    spec(
        "deepseek-v4-flash",
        "DeepSeek V4 Flash",
        "deepseek",
        1_048_576,
        HI_MAX,
    ),
    spec(
        "deepseek-v4-1-flash-1017",
        "DeepSeek V4.1 Flash 1017",
        "deepseek",
        1_048_576,
        NONE_LOW_HI_MAX,
    ),
    spec("swe-1-7", "SWE 1.7", "cognition", 262_000, MEDIUM_ONLY),
    spec(
        "swe-2-lightning",
        "SWE 2 Lightning",
        "cognition",
        262_144,
        MED_HI_MAX,
    ),
    spec(
        "claude-opus-4-6",
        "Claude Opus 4.6",
        "anthropic",
        200_000,
        THINKING_ONLY,
    ),
    spec(
        "claude-opus-4-6-1m",
        "Claude Opus 4.6 1M",
        "anthropic",
        1_000_000,
        THINKING_ONLY,
    ),
    spec("gpt-5-4", "GPT 5.4", "openai", 272_000, NONE_LOW_MED_HI_XHI),
    spec("gpt-5-5", "GPT 5.5", "openai", 272_000, NONE_LOW_MED_HI_XHI),
    spec(
        "gpt-5-4-mini",
        "GPT 5.4 Mini",
        "openai",
        400_000,
        LOW_MED_HI_XHI,
    ),
    spec(
        "claude-sonnet-4-6",
        "Claude Sonnet 4.6",
        "anthropic",
        200_000,
        THINKING_ONLY,
    ),
    spec(
        "claude-sonnet-4-6-1m",
        "Claude Sonnet 4.6 1M",
        "anthropic",
        1_000_000,
        THINKING_ONLY,
    ),
    spec("kimi-k2-6", "Kimi K2.6", "moonshot", 262_144, NO_LEVELS),
    spec("kimi-k2-7", "Kimi K2.7", "moonshot", 262_144, NO_LEVELS),
    spec(
        "nemotron-3-ultra",
        "Nemotron 3 Ultra",
        "nvidia",
        1_000_000,
        NONE_MED_HI,
    ),
    spec("swe-1-6", "SWE 1.6", "cognition", 200_000, NO_LEVELS),
    spec(
        "deepseek-v4-pro",
        "DeepSeek V4 Pro",
        "deepseek",
        1_048_576,
        HI_MAX,
    ),
];

/// Bases Devin serves as text-only (no `supports_images` in the live feed,
/// matching upstream's curated modality list). The live catalog overrides
/// this once a `GetUserStatus` response arrives.
pub const TEXT_ONLY_BASES: &[&str] = &[
    "deepseek-v4-flash",
    "deepseek-v4-pro",
    "glm-5-2",
    "glm-5-2-1m",
    "glm-5-3",
    "nemotron-3-ultra",
];
