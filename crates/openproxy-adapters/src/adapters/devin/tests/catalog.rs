//! Live-catalog tests: `cascade_model_config_data` parsing, snapshot
//! lookups, resolver corrections and `devin_builtin_models` overlays.

use super::super::catalog::{self, parse_client_model_configs};
use super::super::models::{
    devin_builtin_models, devin_max_completion_tokens_for, resolve_devin_chat_model_uid,
};
use super::super::protoc;
use super::super::quota::parse_user_status;
use std::sync::Mutex;

/// Live-catalog tests share the process-wide snapshot; serialize them.
static LIVE_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Synthetic ClientModelConfig fixtures (same shapes the wire showed)
// ---------------------------------------------------------------------------

/// `f30` display block: `{1: display, 2: dim {1: "Reasoning Effort", 2: opt {2: label}}}`.
fn display_block(display: &str, effort: &str) -> Vec<u8> {
    let mut option = Vec::new();
    protoc::append_string(&mut option, 2, effort);
    let mut dim = Vec::new();
    protoc::append_string(&mut dim, 1, "Reasoning Effort");
    protoc::append_message(&mut dim, 2, &option);
    let mut out = Vec::new();
    protoc::append_string(&mut out, 1, display);
    if !effort.is_empty() {
        protoc::append_message(&mut out, 2, &dim);
    }
    out
}

/// `f23` model_info: `{13: max_output, 23: base}`.
fn model_info(base: &str, max_output: i64) -> Vec<u8> {
    let mut out = Vec::new();
    protoc::append_varint_field(&mut out, 13, max_output as u64);
    protoc::append_string(&mut out, 23, base);
    out
}

/// One `ClientModelConfig` row.
fn cfg_row(
    uid: &str,
    base: &str,
    display: &str,
    ctx: i64,
    max_out: i64,
    images: bool,
    effort: &str,
) -> Vec<u8> {
    let mut out = Vec::new();
    if images {
        protoc::append_varint_field(&mut out, 5, 1);
    }
    protoc::append_varint_field(&mut out, 18, ctx as u64);
    protoc::append_string(&mut out, 22, uid);
    protoc::append_message(&mut out, 23, &model_info(base, max_out));
    protoc::append_message(&mut out, 30, &display_block(display, effort));
    out
}

/// `cascade_model_config_data`: repeated field 1 of config rows.
fn cfg_blob(rows: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for row in rows {
        protoc::append_message(&mut out, 1, row);
    }
    out
}

/// A GetUserStatus body carrying just `user_status.cascade_model_config_data`.
fn user_status_with_configs(blob: &[u8]) -> Vec<u8> {
    let mut us = Vec::new();
    protoc::append_string(&mut us, 3, "user");
    protoc::append_message(&mut us, 33, blob);
    let mut body = Vec::new();
    protoc::append_message(&mut body, 1, &us);
    body
}

fn store_fixture() {
    let rows = parse_client_model_configs(&cfg_blob(&fixture_rows()));
    catalog::store_live_catalog(rows);
}

fn fixture_rows() -> Vec<Vec<u8>> {
    vec![
        cfg_row(
            "swe-2-medium",
            "swe-2",
            "SWE-2",
            262000,
            128000,
            true,
            "Medium",
        ),
        cfg_row("swe-2-high", "swe-2", "SWE-2", 262000, 128000, true, "High"),
        cfg_row("swe-2-max", "swe-2", "SWE-2", 262000, 128000, true, "Max"),
        cfg_row(
            "swe-2-medium-lightning",
            "swe-2",
            "SWE-2",
            262144,
            128000,
            true,
            "Medium",
        ),
        cfg_row(
            "swe-2-high-lightning",
            "swe-2",
            "SWE-2",
            262144,
            128000,
            true,
            "High",
        ),
        cfg_row(
            "swe-2-max-lightning",
            "swe-2",
            "SWE-2",
            262144,
            128000,
            true,
            "Max",
        ),
        cfg_row(
            "kimi-k3-low",
            "kimi-k3",
            "Kimi K3",
            1048576,
            131072,
            true,
            "Low",
        ),
        cfg_row(
            "kimi-k3-high",
            "kimi-k3",
            "Kimi K3",
            1048576,
            131072,
            true,
            "High",
        ),
        cfg_row(
            "kimi-k3-max",
            "kimi-k3",
            "Kimi K3",
            1048576,
            131072,
            true,
            "Max",
        ),
        cfg_row(
            "kimi-k2-6",
            "kimi-k2.6",
            "Kimi K2.6",
            262144,
            8192,
            true,
            "",
        ),
        cfg_row(
            "glm-5-3-low",
            "glm-5-3",
            "GLM 5.3",
            1048576,
            128000,
            false,
            "Low",
        ),
        cfg_row(
            "glm-5-3-high",
            "glm-5-3",
            "GLM 5.3",
            1048576,
            128000,
            false,
            "High",
        ),
        cfg_row(
            "glm-5-3-max",
            "glm-5-3",
            "GLM 5.3",
            1048576,
            128000,
            false,
            "Max",
        ),
        cfg_row(
            "nemotron-3-ultra-medium",
            "nemotron-3-ultra",
            "Nemotron 3 Ultra",
            1000000,
            32768,
            false,
            "Medium",
        ),
        cfg_row(
            "nemotron-3-ultra-high",
            "nemotron-3-ultra",
            "Nemotron 3 Ultra",
            1000000,
            32768,
            false,
            "High",
        ),
        cfg_row(
            "nemotron-3-ultra-none",
            "nemotron-3-ultra",
            "Nemotron 3 Ultra",
            1000000,
            32768,
            false,
            "None",
        ),
        cfg_row(
            "gpt-6-1-terra-medium",
            "gpt-6.1-terra",
            "GPT-6.1 Terra",
            1000000,
            128000,
            true,
            "Medium",
        ),
        cfg_row(
            "gpt-6-1-terra-high",
            "gpt-6.1-terra",
            "GPT-6.1 Terra",
            1000000,
            128000,
            true,
            "High",
        ),
        cfg_row(
            "claude-opus-4-6",
            "claude-opus-4.6",
            "Claude Opus 4.6",
            200000,
            128000,
            true,
            "High",
        ),
        cfg_row(
            "claude-opus-4-6-1m",
            "claude-opus-4.6",
            "Claude Opus 4.6",
            1000000,
            128000,
            true,
            "High",
        ),
        cfg_row(
            "claude-opus-4-6-thinking",
            "claude-opus-4.6",
            "Claude Opus 4.6",
            200000,
            128000,
            true,
            "High",
        ),
        cfg_row(
            "claude-opus-4-6-thinking-1m",
            "claude-opus-4.6",
            "Claude Opus 4.6",
            1000000,
            128000,
            true,
            "High",
        ),
    ]
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

#[test]
fn parse_client_model_configs_extracts_rows() {
    let rows = parse_client_model_configs(&cfg_blob(&fixture_rows()));
    assert_eq!(rows.len(), fixture_rows().len());
    let swe_high = rows.iter().find(|r| r.uid == "swe-2-high").unwrap();
    assert_eq!(swe_high.base, "swe-2");
    assert_eq!(swe_high.display_name, "SWE-2");
    assert_eq!(swe_high.context_length, 262000);
    assert_eq!(swe_high.max_output_tokens, 128000);
    assert!(swe_high.supports_images);
    assert_eq!(swe_high.effort, "high");

    let bare = rows.iter().find(|r| r.uid == "kimi-k2-6").unwrap();
    assert_eq!(bare.base, "kimi-k2.6");
    assert!(bare.effort.is_empty());

    let glm = rows.iter().find(|r| r.uid == "glm-5-3-high").unwrap();
    assert!(!glm.supports_images);
}

#[test]
fn parse_user_status_reads_field_33() {
    let body = user_status_with_configs(&cfg_blob(&fixture_rows()));
    let us = parse_user_status(&body).unwrap();
    assert_eq!(us.user_name, "user");
    assert_eq!(us.model_configs.len(), fixture_rows().len());
    assert_eq!(us.model_configs[0].uid, "swe-2-medium");
}

#[test]
fn parse_skips_malformed_and_uidless_rows() {
    // A config carrying no uid/base is dropped silently.
    let mut orphan = Vec::new();
    protoc::append_varint_field(&mut orphan, 5, 1);
    let rows = parse_client_model_configs(&cfg_blob(&[
        orphan,
        cfg_row("a-b", "a.b", "A B", 1, 2, true, "High"),
    ]));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].uid, "a-b");
}

// ---------------------------------------------------------------------------
// Catalog lookups
// ---------------------------------------------------------------------------

#[test]
fn catalog_groups_rows_by_normalized_base() {
    let _g = lock();
    store_fixture();
    let cat = catalog::live_catalog().unwrap();
    assert!(cat.is_live_uid("swe-2-high"));
    assert!(!cat.is_live_uid("swe-2-low"));
    assert_eq!(cat.rows_for_base("swe-2").unwrap().len(), 6);
    // Dotted live bases normalize to dashed ids.
    assert_eq!(cat.rows_for_base("claude-opus-4-6").unwrap().len(), 4);
    assert_eq!(cat.rows_for_base("kimi-k2-6").unwrap().len(), 1);
    assert!(cat.rows_for_base("gpt-6-1-terra").is_some());
    // Row lookup by exact uid.
    assert_eq!(
        cat.row_for_uid("kimi-k3-max").unwrap().max_output_tokens,
        131072
    );
    catalog::clear_live_catalog();
}

#[test]
fn catalog_base_metadata_and_uid_picks() {
    let _g = lock();
    store_fixture();
    let cat = catalog::live_catalog().unwrap();
    assert_eq!(cat.base_supports_images("glm-5-3"), Some(false));
    assert_eq!(cat.base_supports_images("swe-2"), Some(true));
    assert_eq!(cat.base_limits("swe-2"), Some((262000, 128000)));
    // Default variant = the row without variant markers.
    assert_eq!(cat.base_limits("claude-opus-4-6"), Some((200000, 128000)));
    assert_eq!(cat.base_effort_levels("swe-2").join(","), "medium,high,max");
    // pick_uid: exact label, canonical spelling, single-row, empty effort.
    assert_eq!(
        cat.pick_uid("swe-2", "max", "high"),
        Some("swe-2-max".to_string())
    );
    assert_eq!(
        cat.pick_uid("kimi-k2-6", "high", ""),
        Some("kimi-k2-6".to_string())
    );
    // A labeled pick must not land on a variant row sharing the label.
    assert_eq!(
        cat.pick_uid("claude-opus-4-6", "high", "high"),
        Some("claude-opus-4-6".to_string())
    );
    catalog::clear_live_catalog();
}

// ---------------------------------------------------------------------------
// Resolver live corrections
// ---------------------------------------------------------------------------

#[test]
fn resolver_clamps_effort_onto_live_levels() {
    let _g = lock();
    store_fixture();
    assert_eq!(
        resolve_devin_chat_model_uid("kimi-k3", "", 0),
        "kimi-k3-high"
    );
    // "medium" is not a live level for kimi-k3 — clamp to the nearest rank.
    assert_eq!(
        resolve_devin_chat_model_uid("kimi-k3:medium", "", 0),
        "kimi-k3-high"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("kimi-k3:low", "", 0),
        "kimi-k3-low"
    );
    catalog::clear_live_catalog();
}

#[test]
fn resolver_handles_live_only_bases() {
    let _g = lock();
    store_fixture();
    // gpt-6-1-terra is absent from the static table: the generic default
    // lands on medium, an explicit effort maps straight onto a live row.
    assert_eq!(
        resolve_devin_chat_model_uid("gpt-6-1-terra", "", 0),
        "gpt-6-1-terra-medium"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("gpt-6-1-terra:high", "", 0),
        "gpt-6-1-terra-high"
    );
    catalog::clear_live_catalog();
}

#[test]
fn resolver_preserves_qualified_and_alias_spellings() {
    let _g = lock();
    store_fixture();
    // Fully-qualified spellings are trusted even when absent from live rows.
    assert_eq!(
        resolve_devin_chat_model_uid("swe-2-low-lightning", "", 0),
        "swe-2-low-lightning"
    );
    // Enum aliases are never in the feed — static answer stands.
    assert_eq!(
        resolve_devin_chat_model_uid("claude-haiku-4-5", "", 0),
        "MODEL_PRIVATE_11"
    );
    // Legacy thinking variants remain live-verbatim.
    assert_eq!(
        resolve_devin_chat_model_uid("claude-opus-4-6", "high", 0),
        "claude-opus-4-6-thinking"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("claude-opus-4-6", "", 0),
        "claude-opus-4-6"
    );
    catalog::clear_live_catalog();
}

// ---------------------------------------------------------------------------
// Builtin listing + completion caps
// ---------------------------------------------------------------------------

#[test]
fn builtin_models_overlay_live_modalities_and_limits() {
    let _g = lock();
    store_fixture();
    let models = devin_builtin_models();
    let swe2 = models
        .iter()
        .find(|m| m.model_id.as_str() == "swe-2")
        .unwrap();
    assert_eq!(swe2.max_output_tokens, Some(128000));
    assert_eq!(swe2.context_length, Some(262000));
    let glm = models
        .iter()
        .find(|m| m.model_id.as_str() == "glm-5-3")
        .unwrap();
    assert_eq!(
        glm.input_modalities.as_deref(),
        Some(&["text".to_string()][..])
    );
    let terra = models
        .iter()
        .find(|m| m.model_id.as_str() == "gpt-6-1-terra")
        .expect("live-only base listed");
    assert_eq!(terra.display_name.as_deref(), Some("GPT-6.1 Terra"));
    assert_eq!(terra.family.as_deref(), Some("openai"));
    catalog::clear_live_catalog();
}

#[test]
fn builtin_models_static_fallback_modalities() {
    let _g = lock();
    catalog::clear_live_catalog();
    let models = devin_builtin_models();
    for id in [
        "glm-5-2",
        "glm-5-3",
        "glm-5-2-1m",
        "deepseek-v4-flash",
        "deepseek-v4-pro",
        "nemotron-3-ultra",
    ] {
        let m = models.iter().find(|m| m.model_id.as_str() == id).unwrap();
        assert_eq!(
            m.input_modalities.as_deref(),
            Some(&["text".to_string()][..]),
            "{id} must be text-only"
        );
    }
    // swe-2 is free (MODEL_COST_TIER_FREE on the wire) → listed as swe-2:free.
    let swe2 = models
        .iter()
        .find(|m| m.model_id.as_str() == "swe-2:free")
        .unwrap();
    assert_eq!(
        swe2.input_modalities.as_deref(),
        Some(&["text".to_string(), "image".to_string()][..])
    );
}

#[test]
fn max_completion_tokens_prefers_live_row() {
    let _g = lock();
    store_fixture();
    assert_eq!(devin_max_completion_tokens_for("swe-2-high"), Some(128000));
    assert_eq!(devin_max_completion_tokens_for("kimi-k2-6"), Some(8192));
    assert_eq!(
        devin_max_completion_tokens_for("nemotron-3-ultra-none"),
        Some(32768)
    );
    catalog::clear_live_catalog();
    assert_eq!(devin_max_completion_tokens_for("swe-2-high"), Some(64000));
}

#[test]
fn empty_parse_never_clobbers_snapshot() {
    let _g = lock();
    catalog::clear_live_catalog();
    catalog::store_live_catalog(Vec::new());
    assert!(catalog::live_catalog().is_none());
    store_fixture();
    catalog::store_live_catalog(Vec::new());
    assert!(catalog::live_catalog().is_some());
    catalog::clear_live_catalog();
}

// ---------- pricing fields (credit_multiplier / model_cost_tier / promo) ----------

/// ClientModelConfig row carrying the pricing fields (fields 3, 19, 24)
/// on top of the identity fields the fixture helper emits.
fn cfg_row_priced(
    uid: &str,
    base: &str,
    credit_multiplier: f32,
    cost_tier: i64,
    promo_active: bool,
) -> Vec<u8> {
    let mut out = cfg_row(uid, base, base, 262_000, 128_000, true, "High");
    protoc::append_fixed32(&mut out, 3, credit_multiplier.to_bits());
    if promo_active {
        let mut promo = Vec::new();
        protoc::append_varint_field(&mut promo, 1, 1);
        protoc::append_message(&mut out, 19, &promo);
    }
    protoc::append_varint_field(&mut out, 24, cost_tier as u64);
    out
}

#[test]
fn parses_pricing_fields_and_marks_free() {
    let _g = lock();
    catalog::clear_live_catalog();
    let rows = parse_client_model_configs(&cfg_blob(&[
        cfg_row_priced("swe-2-high", "swe-2", 9.0, 4, true),
        cfg_row_priced("claude-opus-5-5-medium", "claude-opus-5.5", 45.0, 3, false),
        cfg_row_priced("deepseek-v4-flash-high", "deepseek-v4-flash", 0.5, 1, false),
    ]));
    let swe2 = rows.iter().find(|r| r.uid == "swe-2-high").unwrap();
    assert!(swe2.is_free());
    assert_eq!(swe2.cost_tier, 4);
    assert!(swe2.promo_active);
    assert!((swe2.credit_multiplier - 9.0).abs() < f32::EPSILON);
    assert!(
        !rows
            .iter()
            .find(|r| r.uid == "claude-opus-5-5-medium")
            .unwrap()
            .is_free()
    );
    assert!(
        !rows
            .iter()
            .find(|r| r.uid == "deepseek-v4-flash-high")
            .unwrap()
            .is_free()
    );
}

#[test]
fn promo_alone_does_not_mark_free() {
    // promos ride on paid models (gemini-4-argon: tier HIGH + active promo;
    // swe-2-lightning carries a literal "30% off" promo field).
    let row = parse_client_model_configs(&cfg_blob(&[cfg_row_priced(
        "kimi-k3-high",
        "kimi-k3",
        6.0,
        3,
        true,
    )]));
    assert!(
        !row[0].is_free(),
        "an active promo must not mark the row free"
    );
}

#[test]
fn catalog_base_is_free_and_free_base_ids() {
    let _g = lock();
    catalog::clear_live_catalog();
    catalog::store_live_catalog(parse_client_model_configs(&cfg_blob(&[
        cfg_row_priced("swe-2-high", "swe-2", 9.0, 4, false),
        cfg_row_priced("swe-2-max", "swe-2", 9.0, 4, false),
        cfg_row_priced("glm-5-3-low", "glm-5.3", 1.0, 1, false),
    ])));
    let cat = catalog::live_catalog().unwrap();
    assert_eq!(cat.base_is_free("swe-2"), Some(true));
    assert_eq!(cat.base_is_free("glm-5-3"), Some(false));
    assert_eq!(cat.base_is_free("missing"), None);
    assert_eq!(cat.free_base_ids(), vec!["swe-2".to_string()]);
}

#[test]
fn free_base_lists_with_free_suffix() {
    let _g = lock();
    catalog::clear_live_catalog();
    catalog::store_live_catalog(parse_client_model_configs(&cfg_blob(&[
        // swe-2: free tier AND an active promo — promo is irrelevant.
        cfg_row_priced("swe-2-high", "swe-2", 9.0, 4, true),
        cfg_row_priced("glm-5-3-low", "glm-5.3", 1.0, 1, false),
    ])));
    let models = devin_builtin_models();
    assert!(models.iter().any(|m| m.model_id.as_str() == "swe-2:free"));
    assert!(!models.iter().any(|m| m.model_id.as_str() == "swe-2"));
    assert!(models.iter().any(|m| m.model_id.as_str() == "glm-5-3"));
    assert!(!models.iter().any(|m| m.model_id.as_str() == "glm-5-3:free"));
}

#[test]
fn free_suffix_stripped_during_uid_resolution() {
    assert_eq!(
        resolve_devin_chat_model_uid("swe-2:free", "", 0),
        "swe-2-high"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("devin/swe-2:free", "", 0),
        "swe-2-high"
    );
}
