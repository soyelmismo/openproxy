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

struct DevinModelSpec {
    id: &'static str,
    display_name: &'static str,
    owner: &'static str,
    context_length: i64,
    /// Allowed thinking-effort levels for the uid suffix.
    thinking_levels: &'static [&'static str],
}

const CATALOG: &[DevinModelSpec] = &[
    DevinModelSpec {
        id: "swe-2",
        display_name: "SWE-2",
        owner: "cognition",
        context_length: 262_000,
        thinking_levels: &["medium", "high", "max"],
    },
    DevinModelSpec {
        id: "claude-fable-5-1",
        display_name: "Claude Fable 5.1",
        owner: "anthropic",
        context_length: 1_000_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "claude-opus-5-5",
        display_name: "Claude Opus 5.5",
        owner: "anthropic",
        context_length: 1_000_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "gpt-6-astra",
        display_name: "GPT-6 Astra",
        owner: "openai",
        context_length: 1_000_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "gpt-6-sol",
        display_name: "GPT-6 Sol",
        owner: "openai",
        context_length: 1_000_000,
        thinking_levels: &["none", "low", "medium", "high", "xhigh", "max"],
    },
    DevinModelSpec {
        id: "gpt-6-luna",
        display_name: "GPT-6 Luna",
        owner: "openai",
        context_length: 1_000_000,
        thinking_levels: &["none", "low", "medium", "high", "xhigh", "max"],
    },
    DevinModelSpec {
        id: "gemini-4-argon",
        display_name: "Gemini 4 Argon",
        owner: "google",
        context_length: 1_048_576,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "kimi-k3",
        display_name: "Kimi K3",
        owner: "moonshot",
        context_length: 1_048_576,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "glm-5-2",
        display_name: "GLM 5.2",
        owner: "zhipu",
        context_length: 200_000,
        thinking_levels: &["none", "max"],
    },
    DevinModelSpec {
        id: "glm-5-3",
        display_name: "GLM 5.3",
        owner: "zhipu",
        context_length: 1_048_576,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "claude-sonnet-5-5",
        display_name: "Claude Sonnet 5.5",
        owner: "anthropic",
        context_length: 1_000_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "gemini-3-8-flash",
        display_name: "Gemini 3.8 Flash",
        owner: "google",
        context_length: 1_048_576,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "claude-opus-4-7",
        display_name: "Claude Opus 4.7",
        owner: "anthropic",
        context_length: 1_000_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "claude-opus-4-8",
        display_name: "Claude Opus 4.8",
        owner: "anthropic",
        context_length: 1_000_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "claude-opus-5",
        display_name: "Claude Opus 5",
        owner: "anthropic",
        context_length: 1_000_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "claude-5-fable",
        display_name: "Claude 5 Fable",
        owner: "anthropic",
        context_length: 1_000_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "claude-sonnet-5",
        display_name: "Claude Sonnet 5",
        owner: "anthropic",
        context_length: 1_000_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "claude-haiku-5-5",
        display_name: "Claude Haiku 5.5",
        owner: "anthropic",
        context_length: 1_000_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "gemini-3-5-flash",
        display_name: "Gemini 3.5 Flash",
        owner: "google",
        context_length: 1_048_576,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "gemini-3-6-flash",
        display_name: "Gemini 3.6 Flash",
        owner: "google",
        context_length: 1_048_576,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "gemini-3-7-flash",
        display_name: "Gemini 3.7 Flash",
        owner: "google",
        context_length: 1_048_576,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "gpt-5-6-sol",
        display_name: "GPT 5.6 Sol",
        owner: "openai",
        context_length: 1_000_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "gpt-5-6-terra",
        display_name: "GPT 5.6 Terra",
        owner: "openai",
        context_length: 1_000_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "gpt-5-6-luna",
        display_name: "GPT 5.6 Luna",
        owner: "openai",
        context_length: 1_000_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "gpt-6-1-sol",
        display_name: "GPT 6.1 Sol",
        owner: "openai",
        context_length: 1_000_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "glm-5-2-1m",
        display_name: "GLM 5.2 1M",
        owner: "zhipu",
        context_length: 1_000_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "grok-4-6",
        display_name: "Grok 4.6",
        owner: "xai",
        context_length: 500_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "grok-4-7",
        display_name: "Grok 4.7",
        owner: "xai",
        context_length: 500_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "muse-wonder-1-0",
        display_name: "Muse Wonder 1.0",
        owner: "devin",
        context_length: 1_048_576,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "glm-5-3-flash",
        display_name: "GLM 5.3 Flash",
        owner: "zhipu",
        context_length: 1_000_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "deepseek-v4-flash",
        display_name: "DeepSeek V4 Flash",
        owner: "deepseek",
        context_length: 1_048_576,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "deepseek-v4-1-flash-1017",
        display_name: "DeepSeek V4.1 Flash 1017",
        owner: "deepseek",
        context_length: 1_048_576,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "swe-1-7",
        display_name: "SWE 1.7",
        owner: "cognition",
        context_length: 262_000,
        thinking_levels: &["medium"],
    },
    DevinModelSpec {
        id: "swe-2-lightning",
        display_name: "SWE 2 Lightning",
        owner: "cognition",
        context_length: 262_144,
        thinking_levels: &["minimal", "low", "medium", "high", "xhigh", "max", "none"],
    },
    DevinModelSpec {
        id: "claude-opus-4-6",
        display_name: "Claude Opus 4.6",
        owner: "anthropic",
        context_length: 200_000,
        thinking_levels: &["thinking"],
    },
    DevinModelSpec {
        id: "claude-opus-4-6-1m",
        display_name: "Claude Opus 4.6 1M",
        owner: "anthropic",
        context_length: 1_000_000,
        thinking_levels: &["thinking"],
    },
    DevinModelSpec {
        id: "gpt-5-4",
        display_name: "GPT 5.4",
        owner: "openai",
        context_length: 272_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "gpt-5-5",
        display_name: "GPT 5.5",
        owner: "openai",
        context_length: 272_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "gpt-5-4-mini",
        display_name: "GPT 5.4 Mini",
        owner: "openai",
        context_length: 400_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "claude-sonnet-4-6",
        display_name: "Claude Sonnet 4.6",
        owner: "anthropic",
        context_length: 200_000,
        thinking_levels: &["thinking"],
    },
    DevinModelSpec {
        id: "claude-sonnet-4-6-1m",
        display_name: "Claude Sonnet 4.6 1M",
        owner: "anthropic",
        context_length: 1_000_000,
        thinking_levels: &["thinking"],
    },
    DevinModelSpec {
        id: "kimi-k2-6",
        display_name: "Kimi K2.6",
        owner: "moonshot",
        context_length: 262_144,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "kimi-k2-7",
        display_name: "Kimi K2.7",
        owner: "moonshot",
        context_length: 262_144,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "nemotron-3-ultra",
        display_name: "Nemotron 3 Ultra",
        owner: "nvidia",
        context_length: 1_000_000,
        thinking_levels: DEFAULT_LEVELS,
    },
    DevinModelSpec {
        id: "swe-1-6",
        display_name: "SWE 1.6",
        owner: "cognition",
        context_length: 200_000,
        thinking_levels: &["fast"],
    },
    DevinModelSpec {
        id: "deepseek-v4-pro",
        display_name: "DeepSeek V4 Pro",
        owner: "deepseek",
        context_length: 1_048_576,
        thinking_levels: DEFAULT_LEVELS,
    },
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
