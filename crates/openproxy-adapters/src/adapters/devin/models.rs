//! Devin static model catalog and `chat_model_uid` resolution.
//!
//! Ported from CLIProxyAPI's `ResolveDevinChatModelUID` + Devin model registry:
//! the wire model identifier carries the thinking effort as a suffix, so the
//! resolver combines the requested model, an optional `:effort` marker and the
//! explicit thinking level/budget into the catalog's canonical spelling.

use crate::adapters::{DiscoveredModel, ModelId, TargetFormat};

/// Max completion tokens every catalog entry advertises.
const DEVIN_MAX_COMPLETION_TOKENS: i64 = 64_000;

const DEFAULT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const MED_HI_MAX: &[&str] = &["medium", "high", "max"];
const NONE_TO_MAX: &[&str] = &["none", "low", "medium", "high", "xhigh", "max"];
const NONE_MAX: &[&str] = &["none", "max"];
const MEDIUM_ONLY: &[&str] = &["medium"];
const LIGHTNING_LEVELS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max", "none"];
const THINKING_ONLY: &[&str] = &["thinking"];
const FAST_ONLY: &[&str] = &["fast"];

struct DevinModelSpec {
    id: &'static str,
    display_name: &'static str,
    owner: &'static str,
    context_length: i64,
    /// Allowed thinking-effort levels for the uid suffix.
    thinking_levels: &'static [&'static str],
}

/// Catalog row: (id, display name, owner, context length, thinking levels).
const fn spec(
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
const CATALOG: &[DevinModelSpec] = &[
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
        DEFAULT_LEVELS,
    ),
    spec("kimi-k3", "Kimi K3", "moonshot", 1_048_576, DEFAULT_LEVELS),
    spec("glm-5-2", "GLM 5.2", "zhipu", 200_000, NONE_MAX),
    spec("glm-5-3", "GLM 5.3", "zhipu", 1_048_576, DEFAULT_LEVELS),
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
        DEFAULT_LEVELS,
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
        DEFAULT_LEVELS,
    ),
    spec(
        "gemini-3-6-flash",
        "Gemini 3.6 Flash",
        "google",
        1_048_576,
        DEFAULT_LEVELS,
    ),
    spec(
        "gemini-3-7-flash",
        "Gemini 3.7 Flash",
        "google",
        1_048_576,
        DEFAULT_LEVELS,
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
    spec(
        "glm-5-2-1m",
        "GLM 5.2 1M",
        "zhipu",
        1_000_000,
        DEFAULT_LEVELS,
    ),
    spec("grok-4-6", "Grok 4.6", "xai", 500_000, DEFAULT_LEVELS),
    spec("grok-4-7", "Grok 4.7", "xai", 500_000, DEFAULT_LEVELS),
    spec(
        "muse-wonder-1-0",
        "Muse Wonder 1.0",
        "devin",
        1_048_576,
        DEFAULT_LEVELS,
    ),
    spec(
        "glm-5-3-flash",
        "GLM 5.3 Flash",
        "zhipu",
        1_000_000,
        DEFAULT_LEVELS,
    ),
    spec(
        "deepseek-v4-flash",
        "DeepSeek V4 Flash",
        "deepseek",
        1_048_576,
        DEFAULT_LEVELS,
    ),
    spec(
        "deepseek-v4-1-flash-1017",
        "DeepSeek V4.1 Flash 1017",
        "deepseek",
        1_048_576,
        DEFAULT_LEVELS,
    ),
    spec("swe-1-7", "SWE 1.7", "cognition", 262_000, MEDIUM_ONLY),
    spec(
        "swe-2-lightning",
        "SWE 2 Lightning",
        "cognition",
        262_144,
        LIGHTNING_LEVELS,
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
    spec("gpt-5-4", "GPT 5.4", "openai", 272_000, DEFAULT_LEVELS),
    spec("gpt-5-5", "GPT 5.5", "openai", 272_000, DEFAULT_LEVELS),
    spec(
        "gpt-5-4-mini",
        "GPT 5.4 Mini",
        "openai",
        400_000,
        DEFAULT_LEVELS,
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
    spec(
        "kimi-k2-6",
        "Kimi K2.6",
        "moonshot",
        262_144,
        DEFAULT_LEVELS,
    ),
    spec(
        "kimi-k2-7",
        "Kimi K2.7",
        "moonshot",
        262_144,
        DEFAULT_LEVELS,
    ),
    spec(
        "nemotron-3-ultra",
        "Nemotron 3 Ultra",
        "nvidia",
        1_000_000,
        DEFAULT_LEVELS,
    ),
    spec("swe-1-6", "SWE 1.6", "cognition", 200_000, FAST_ONLY),
    spec(
        "deepseek-v4-pro",
        "DeepSeek V4 Pro",
        "deepseek",
        1_048_576,
        DEFAULT_LEVELS,
    ),
];

/// Effort suffixes that make a model string already fully-qualified.
const KNOWN_SUFFIXES: &[&str] = &[
    "-none",
    "-low",
    "-medium",
    "-high",
    "-xhigh",
    "-max",
    "-fast",
    "-slow",
    "-priority",
    "-low-priority",
    "-medium-priority",
    "-high-priority",
    "-xhigh-priority",
    "-max-priority",
    "-low-fast",
    "-medium-fast",
    "-high-fast",
    "-xhigh-fast",
    "-max-fast",
    "-none-fast",
    "-thinking-1m",
    "-thinking",
    "-max-1m",
    "-none-1m",
    "-minimal-lightning",
    "-low-lightning",
    "-medium-lightning",
    "-high-lightning",
    "-xhigh-lightning",
    "-max-lightning",
    "-none-lightning",
    "_none",
    "_minimal",
    "_low",
    "_medium",
    "_high",
    "_xhigh",
    "_max",
    "_thinking",
];

/// Rank order used by nearest-effort clamping.
const EFFORT_RANK: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

fn catalog_lookup(id: &str) -> Option<&'static DevinModelSpec> {
    CATALOG.iter().find(|spec| spec.id == id)
}

/// Completion-token ceiling for a resolved `chat_model_uid` (or bare model
/// id). `None` when the model is not in the catalog — callers then keep the
/// protocol default.
pub fn devin_max_completion_tokens_for(uid: &str) -> Option<u64> {
    let lower = uid.trim().to_ascii_lowercase();
    let stem = lower.strip_suffix("-lightning").unwrap_or(&lower);
    let base = KNOWN_SUFFIXES
        .iter()
        .find_map(|s| stem.strip_suffix(s))
        .unwrap_or(stem);
    catalog_lookup(base).map(|_| DEVIN_MAX_COMPLETION_TOKENS as u64)
}

/// `NormalizeThinkingLevel`: map free-form level/budget hints onto the
/// canonical effort vocabulary. Returns "" when nothing usable was given.
pub fn normalize_thinking_level(level: &str, budget_tokens: i64) -> String {
    let l = level.trim().to_ascii_lowercase();
    match l.as_str() {
        "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "fast" => l,
        "none" | "off" | "disabled" => "none".to_string(),
        "auto" | "adaptive" => "high".to_string(),
        _ => {
            if budget_tokens > 0 {
                match budget_tokens {
                    ..=4096 => "low".to_string(),
                    4097..=16384 => "medium".to_string(),
                    16385..=32768 => "high".to_string(),
                    _ => "max".to_string(),
                }
            } else {
                String::new()
            }
        }
    }
}

/// Default effort for a catalog base when the request doesn't pick one.
fn select_default_effort(base: &str, levels: &[&str]) -> String {
    if base.contains("swe-2") {
        return "high".to_string();
    }
    if levels.contains(&"none") && levels.contains(&"low") && base.starts_with("gpt-5") {
        return "low".to_string();
    }
    if levels.contains(&"high")
        && ["gemini", "grok", "glm", "deepseek", "kimi", "nemotron"]
            .iter()
            .any(|needle| base.contains(needle))
    {
        return "high".to_string();
    }
    for candidate in ["medium", "high", "low"] {
        if levels.contains(&candidate) {
            return candidate.to_string();
        }
    }
    levels.first().map(ToString::to_string).unwrap_or_default()
}

/// Clamp `requested` onto `allowed`: exact match wins, then nearest rank in
/// `EFFORT_RANK` (ties resolve to the higher index). Unrankable requests and
/// out-of-catalog "none" fall back to `default`.
pub fn clamp_effort(requested: &str, allowed: &[&str], default: &str) -> String {
    if requested.is_empty() {
        return default.to_string();
    }
    if allowed.contains(&requested) {
        return requested.to_string();
    }
    if requested == "none" {
        return default.to_string();
    }
    let Some(req_idx) = EFFORT_RANK.iter().position(|r| *r == requested) else {
        return default.to_string();
    };
    let mut best: Option<(usize, usize)> = None; // (distance, allowed-index-in-rank)
    for (pos, cand) in EFFORT_RANK.iter().enumerate() {
        if !allowed.contains(cand) {
            continue;
        }
        let dist = req_idx.abs_diff(pos);
        let replace = match best {
            Some((bd, bp)) => dist < bd || (dist == bd && pos > bp),
            None => true,
        };
        if replace {
            best = Some((dist, pos));
        }
    }
    best.map_or_else(
        || default.to_string(),
        |(_, pos)| EFFORT_RANK[pos].to_string(),
    )
}

fn ends_with_known_suffix(lower: &str) -> bool {
    KNOWN_SUFFIXES.iter().any(|s| lower.ends_with(s))
}

/// Resolve the wire `chat_model_uid` from the requested model plus thinking
/// hints. Port of upstream `ResolveDevinChatModelUID`.
pub fn resolve_devin_chat_model_uid(
    model: &str,
    thinking_level: &str,
    budget_tokens: i64,
) -> String {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return "swe-2-high".to_string();
    }

    // Strip a "devin/" provider prefix (case-insensitive).
    let has_prefix = trimmed
        .get(..6)
        .is_some_and(|head| head.eq_ignore_ascii_case("devin/"));
    let stripped = if has_prefix {
        trimmed
            .get(6..)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(trimmed)
    } else {
        trimmed
    };
    let lower = stripped.to_ascii_lowercase();

    // Already carries a known effort suffix → fully qualified.
    if ends_with_known_suffix(&lower) {
        return stripped.to_string();
    }

    // CPA suffix conventions: `model(level)` wins over `model:level`, and
    // either overrides the body's thinking_level.
    let (base_raw, level_from_model) = match stripped.rfind('(') {
        Some(open) if stripped.ends_with(')') => {
            (&stripped[..open], &stripped[open + 1..stripped.len() - 1])
        }
        _ => match stripped.rsplit_once(':') {
            Some((base, level)) => (base, level),
            None => (stripped, ""),
        },
    };
    let effort = normalize_thinking_level(
        if level_from_model.is_empty() {
            thinking_level
        } else {
            level_from_model
        },
        budget_tokens,
    );

    let mut canonical_base = base_raw.to_ascii_lowercase().replace('.', "-");

    // Exact special aliases.
    if canonical_base == "claude-haiku-4-5" {
        return "MODEL_PRIVATE_11".to_string();
    }
    if canonical_base == "gpt-4-1" {
        return "MODEL_CHAT_GPT_4_1_2025_04_14".to_string();
    }
    if canonical_base.contains("sonnet-4-5") {
        return if effort.is_empty() || effort == "none" {
            "MODEL_PRIVATE_2".to_string()
        } else {
            "MODEL_PRIVATE_3".to_string()
        };
    }
    if canonical_base == "gemini-3-flash" {
        canonical_base = "gemini-3-8-flash".to_string();
    }

    // Enum-style bases written with underscores.
    let underscore = canonical_base.replace('-', "_");
    if underscore == "model_gpt_5_2" {
        let effort = clamp_effort(&effort, &["none", "low", "medium", "high", "xhigh"], "low");
        return format!("MODEL_GPT_5_2_{}", effort.to_uppercase());
    }
    if underscore == "model_google_gemini_3_0_flash" {
        let effort = clamp_effort(&effort, &["minimal", "low", "medium", "high"], "high");
        return format!("MODEL_GOOGLE_GEMINI_3_0_FLASH_{}", effort.to_uppercase());
    }
    if underscore == "model_claude_4_5_opus" {
        return if effort.is_empty() || effort == "none" {
            "MODEL_CLAUDE_4_5_OPUS".to_string()
        } else {
            "MODEL_CLAUDE_4_5_OPUS_THINKING".to_string()
        };
    }

    let effort_set = !effort.is_empty() && effort != "none";

    // Bases with bespoke uid spellings.
    if canonical_base == "swe-1-7" {
        return if effort == "medium" {
            "swe-1-7-medium".to_string()
        } else {
            canonical_base
        };
    }
    if canonical_base == "swe-1-6" {
        return if effort == "fast" {
            "swe-1-6-fast".to_string()
        } else {
            canonical_base
        };
    }
    if canonical_base == "glm-5-2" {
        return match effort.as_str() {
            "none" => "glm-5-2-none".to_string(),
            "max" => "glm-5-2-max".to_string(),
            _ => canonical_base,
        };
    }
    if canonical_base == "glm-5-2-1m" {
        return match effort.as_str() {
            "none" => "glm-5-2-none-1m".to_string(),
            "max" => "glm-5-2-max-1m".to_string(),
            _ => canonical_base,
        };
    }
    if canonical_base == "claude-opus-4-6" || canonical_base == "claude-sonnet-4-6" {
        return if effort_set {
            format!("{canonical_base}-thinking")
        } else {
            canonical_base
        };
    }
    if canonical_base == "claude-opus-4-6-1m" || canonical_base == "claude-sonnet-4-6-1m" {
        return if effort_set {
            let stem = canonical_base
                .strip_suffix("-1m")
                .unwrap_or(&canonical_base);
            format!("{stem}-thinking-1m")
        } else {
            canonical_base
        };
    }

    // Lightning variants: "<stem>-lightning" resolves the stem's effort and
    // re-appends the marker, unless the stem already carries one.
    if let Some(stem) = canonical_base.strip_suffix("-lightning") {
        if ends_with_known_suffix(stem) {
            return canonical_base;
        }
        let Some(spec) = catalog_lookup(stem) else {
            return canonical_base;
        };
        if spec.thinking_levels.is_empty() {
            return canonical_base;
        }
        let default = select_default_effort(stem, spec.thinking_levels);
        let effort = clamp_effort(&effort, spec.thinking_levels, &default);
        return format!("{stem}-{effort}-lightning");
    }

    // Generic catalog resolution.
    let spec =
        catalog_lookup(&canonical_base).or_else(|| catalog_lookup(&base_raw.to_ascii_lowercase()));
    let Some(spec) = spec else {
        return canonical_base;
    };
    if spec.thinking_levels.is_empty() {
        return spec.id.to_string();
    }
    let default = select_default_effort(spec.id, spec.thinking_levels);
    let effort = clamp_effort(&effort, spec.thinking_levels, &default);
    format!("{}-{effort}", spec.id)
}

/// Static Devin catalog as `DiscoveredModel`s.
pub fn devin_builtin_models() -> Vec<DiscoveredModel> {
    CATALOG
        .iter()
        .map(|spec| DiscoveredModel {
            model_id: ModelId::new(spec.id),
            display_name: Some(spec.display_name.to_string()),
            target_format: TargetFormat::Devin,
            context_length: Some(spec.context_length),
            max_output_tokens: Some(DEVIN_MAX_COMPLETION_TOKENS),
            input_modalities: Some(Box::new(["text".to_string(), "image".to_string()])),
            output_modalities: Some(Box::new(["text".to_string()])),
            model_type: Some("chat".to_string()),
            family: Some(spec.owner.to_string()),
            capabilities: None,
        })
        .collect()
}
