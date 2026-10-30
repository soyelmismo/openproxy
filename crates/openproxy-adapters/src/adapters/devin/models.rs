//! Devin `chat_model_uid` resolution and builtin model listing.
//!
//! Ported from CLIProxyAPI's `ResolveDevinChatModelUID` + Devin model registry:
//! the wire model identifier carries the thinking effort as a suffix, so the
//! resolver combines the requested model, an optional `:effort` marker and the
//! explicit thinking level/budget into the catalog's canonical spelling. The
//! static catalog table lives in `specs.rs`; the live feed in `catalog.rs`
//! supersedes it once a `GetUserStatus` response arrives.

use super::catalog::{self, DevinLiveModelRow};
use super::specs::{self, CATALOG, DEVIN_MAX_COMPLETION_TOKENS, DevinModelSpec, TEXT_ONLY_BASES};
use crate::adapters::{DiscoveredModel, ModelId, TargetFormat};

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

/// Strip `-lightning` and every known effort/variant suffix, leaving the
/// catalog base id (`swe-2-high` → `swe-2`, `glm-5-2-max-1m` → `glm-5-2-1m`
/// only when the 1m form is its own base — callers probe both spellings).
fn uid_stem(uid: &str) -> &str {
    let lower = uid.trim();
    let stem = lower.strip_suffix("-lightning").unwrap_or(lower);
    KNOWN_SUFFIXES
        .iter()
        .find_map(|s| stem.strip_suffix(s))
        .unwrap_or(stem)
}

/// Completion-token ceiling for a resolved `chat_model_uid` (or bare model
/// id). Live `model_info.max_output_tokens` wins; the static table then
/// reports its flat catalog cap. `None` for unknown models — callers keep
/// the protocol default there.
pub fn devin_max_completion_tokens_for(uid: &str) -> Option<u64> {
    let lower = uid.trim().to_ascii_lowercase();
    if let Some(cat) = catalog::live_catalog() {
        if let Some(row) = cat.row_for_uid(&lower)
            && row.max_output_tokens > 0
        {
            return Some(row.max_output_tokens as u64);
        }
        if let Some((_, max_out)) = cat.base_limits(uid_stem(&lower))
            && max_out > 0
        {
            return Some(max_out as u64);
        }
    }
    catalog_lookup(uid_stem(&lower)).map(|_| DEVIN_MAX_COMPLETION_TOKENS as u64)
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

/// Strip a `devin/` provider prefix (case-insensitive), preserving the
/// original spelling of what follows.
fn strip_devin_prefix(trimmed: &str) -> &str {
    let has_prefix = trimmed
        .get(..6)
        .is_some_and(|head| head.eq_ignore_ascii_case("devin/"));
    if has_prefix {
        trimmed
            .get(6..)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(trimmed)
    } else {
        trimmed
    }
}

/// CPA effort conventions: `model(level)` wins over `model:level`; either
/// overrides the body's thinking_level.
fn split_effort_suffix(stripped: &str) -> (&str, &str) {
    match stripped.rfind('(') {
        Some(open) if stripped.ends_with(')') => {
            (&stripped[..open], &stripped[open + 1..stripped.len() - 1])
        }
        _ => match stripped.rsplit_once(':') {
            Some((base, level)) => (base, level),
            None => (stripped, ""),
        },
    }
}

/// Effort the request actually asked for, merging the model suffix with
/// `thinking_level`/`budget_tokens` exactly like the static resolver.
fn requested_effort(stripped: &str, thinking_level: &str, budget_tokens: i64) -> String {
    let (_, level_from_model) = split_effort_suffix(stripped);
    normalize_thinking_level(
        if level_from_model.is_empty() {
            thinking_level
        } else {
            level_from_model
        },
        budget_tokens,
    )
}

/// Resolve the wire `chat_model_uid` from the requested model plus thinking
/// hints. Port of upstream `ResolveDevinChatModelUID`, with a live-catalog
/// correction pass on top: whatever the static rules emit is returned
/// verbatim when the live feed confirms it, and misses are re-resolved
/// against the live row set of the base (covers bases absent from the
/// static table entirely, like `gpt-6-1-terra`).
pub fn resolve_devin_chat_model_uid(
    model: &str,
    thinking_level: &str,
    budget_tokens: i64,
) -> String {
    let resolved = resolve_devin_chat_model_uid_static(model, thinking_level, budget_tokens);
    live_corrected_uid(model, &resolved, thinking_level, budget_tokens)
}

/// Static resolution — everything upstream's `devin_models.go` +
/// `devin_models.json` encode: special aliases, effort spelling, lightning
/// and thinking variants. Returns the uid the curated tables produce,
/// regardless of what the live feed says.
fn resolve_devin_chat_model_uid_static(
    model: &str,
    thinking_level: &str,
    budget_tokens: i64,
) -> String {
    // `:free` is a listing-time marker (quota exemption), never wire data.
    let trimmed = model.trim().strip_suffix(":free").unwrap_or(model.trim());
    if trimmed.is_empty() {
        return "swe-2-high".to_string();
    }

    let stripped = strip_devin_prefix(trimmed);
    let lower = stripped.to_ascii_lowercase();

    // Already carries a known effort suffix → fully qualified.
    if ends_with_known_suffix(&lower) {
        return stripped.to_string();
    }

    let (base_raw, _) = split_effort_suffix(stripped);
    let effort = requested_effort(stripped, thinking_level, budget_tokens);

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

/// Live-catalog correction pass over the static resolution.
///
/// - `resolved` is a live uid → returned verbatim.
/// - A request already fully qualified (`swe-2-low-lightning`) is trusted
///   verbatim: the client named a concrete variant and the live snapshot
///   may simply lag behind it.
/// - Otherwise the live row set of the resolved/request stem picks a uid:
///   single-row bases return their bare uid, labeled rows match the
///   requested effort clamped onto the live level set.
/// - No live base found → the static answer stands (enum aliases and
///   models the account's feed does not carry).
fn live_corrected_uid(model: &str, resolved: &str, level: &str, budget: i64) -> String {
    let Some(cat) = catalog::live_catalog() else {
        return resolved.to_string();
    };
    if cat.is_live_uid(resolved) {
        return resolved.to_string();
    }
    let request = strip_devin_prefix(model.trim());
    let lower_request = request.to_ascii_lowercase();
    if ends_with_known_suffix(&lower_request) {
        return resolved.to_string();
    }
    let effort = requested_effort(request, level, budget);
    for cand in [resolved, request] {
        let normalized = catalog::normalize_base_id(strip_devin_prefix(cand.trim()));
        let stem = uid_stem(&normalized);
        if cat.rows_for_base(stem).is_none() {
            continue;
        }
        let levels = cat.base_effort_levels(stem);
        let level_refs: Vec<&str> = levels.iter().map(String::as_str).collect();
        let default = select_default_effort(stem, &level_refs);
        let chosen = clamp_effort(&effort, &level_refs, &default);
        if let Some(uid) = cat.pick_uid(stem, &chosen, &default) {
            return uid;
        }
    }
    resolved.to_string()
}

/// Owner label for a live-only base id — prefix inference matching the
/// families the static catalog encodes.
fn owner_for(base_id: &str) -> &'static str {
    const OWNERS: &[(&str, &str)] = &[
        ("claude", "anthropic"),
        ("gpt", "openai"),
        ("gemini", "google"),
        ("kimi", "moonshot"),
        ("glm", "zhipu"),
        ("deepseek", "deepseek"),
        ("grok", "xai"),
        ("swe", "cognition"),
        ("nemotron", "nvidia"),
        ("muse", "devin"),
    ];
    OWNERS
        .iter()
        .find(|(prefix, _)| base_id.starts_with(prefix))
        .map_or("devin", |(_, owner)| *owner)
}

fn builtin_model(
    id: &str,
    display_name: &str,
    owner: &str,
    context_length: i64,
    max_output_tokens: i64,
    images: bool,
    is_free: bool,
) -> DiscoveredModel {
    // The `:free` suffix follows the CodeBuddy convention: it marks the model
    // as quota-exempt in listings and is stripped back off during uid
    // resolution, so it never reaches the wire.
    let listed_id = if is_free && !id.ends_with(":free") {
        format!("{id}:free")
    } else {
        id.to_string()
    };
    DiscoveredModel {
        model_id: ModelId::new(listed_id),
        display_name: Some(display_name.to_string()),
        target_format: TargetFormat::Devin,
        context_length: Some(context_length),
        max_output_tokens: Some(max_output_tokens),
        input_modalities: Some(if images {
            Box::new(["text".to_string(), "image".to_string()])
        } else {
            Box::new(["text".to_string()])
        }),
        output_modalities: Some(Box::new(["text".to_string()])),
        model_type: Some("chat".to_string()),
        family: Some(owner.to_string()),
        capabilities: None,
    }
}

/// Whether `base` currently bills zero: live `model_cost_tier`/`promo_status`
/// wins; the static fallback list covers the cold-start window before the
/// first `GetUserStatus` (swe-2 is free as of 2026-10 — the wire carries
/// `MODEL_COST_TIER_FREE` plus an active promo).
pub fn is_devin_model_free(base: &str) -> bool {
    let base = base.trim().trim_end_matches(":free");
    if let Some(free) = catalog::live_catalog().and_then(|cat| cat.base_is_free(base)) {
        return free;
    }
    specs::STATIC_FREE_BASES
        .iter()
        .any(|b| b.eq_ignore_ascii_case(base))
}

/// Devin catalog as `DiscoveredModel`s: the static table overlaid with the
/// live `client_model_configs` feed once a `GetUserStatus` has delivered it.
/// Live rows refresh context/completion ceilings and image support per uid
/// or base, and bases absent from the static table are appended.
pub fn devin_builtin_models() -> Vec<DiscoveredModel> {
    let live = catalog::live_catalog();
    let mut out: Vec<DiscoveredModel> = CATALOG
        .iter()
        .map(|spec| {
            let (ctx, max_out) = live
                .as_ref()
                .and_then(|cat| {
                    cat.row_for_uid(spec.id)
                        .map(|r| (r.context_length, r.max_output_tokens))
                        .or_else(|| cat.base_limits(spec.id))
                })
                .unwrap_or((spec.context_length, DEVIN_MAX_COMPLETION_TOKENS));
            let images = live
                .as_ref()
                .and_then(|cat| {
                    cat.row_for_uid(spec.id)
                        .map(|r| r.supports_images)
                        .or_else(|| cat.base_supports_images(spec.id))
                })
                .unwrap_or_else(|| !TEXT_ONLY_BASES.contains(&spec.id));
            let is_free = live
                .as_ref()
                .and_then(|cat| {
                    cat.row_for_uid(spec.id)
                        .map(DevinLiveModelRow::is_free)
                        .or_else(|| cat.base_is_free(spec.id))
                })
                .unwrap_or_else(|| is_devin_model_free(spec.id));
            builtin_model(
                spec.id,
                spec.display_name,
                spec.owner,
                ctx,
                max_out.max(1),
                images,
                is_free,
            )
        })
        .collect();
    if let Some(cat) = live.as_ref() {
        for base_id in cat.base_ids() {
            if CATALOG.iter().any(|s| s.id == base_id) {
                continue;
            }
            let (ctx, max_out) = cat
                .base_limits(base_id)
                .unwrap_or((1_048_576, DEVIN_MAX_COMPLETION_TOKENS));
            let images = cat.base_supports_images(base_id).unwrap_or(true);
            let display = cat
                .base_display_name(base_id)
                .unwrap_or_else(|| base_id.clone());
            let is_free = cat.base_is_free(base_id).unwrap_or(false);
            out.push(builtin_model(
                base_id,
                &display,
                owner_for(base_id),
                ctx,
                max_out.max(1),
                images,
                is_free,
            ));
        }
    }
    out
}
