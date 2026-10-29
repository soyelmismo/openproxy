use openproxy_types::{candidate_normalized_forms, normalize_model_id};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PriceKind {
    #[default]
    Chat,
    Audio,
    Image,
    Video,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct Price {
    pub input_per_1m: f64,
    pub output_per_1m: f64,
    #[serde(default)]
    pub kind: PriceKind,
    /// Audio duration pricing, in USD per hour of audio. Only meaningful for
    /// [`PriceKind::Audio`]. Whisper-class STT bills by the hour, not by token,
    /// and that unit cannot be expressed as a per-1M-token rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_per_hour: Option<f64>,
}

macro_rules! pricing_catalog {
    ($( ($provider:literal, $model:literal) => ($inp:expr, $out:expr $(, $kind:expr)? ) ),* $(,)?) => {{
        let mut table: HashMap<(&'static str, &'static str), Price> = HashMap::new();
        $(
            #[allow(clippy::redundant_field_names)]
            table.insert(
                ($provider, $model),
                Price {
                    input_per_1m: $inp,
                    output_per_1m: $out,
                    $(kind: $kind,)?
                    ..Default::default()
                },
            );
        )*
        table
    }};
}

/// Model-keyed audio pricing catalog: USD per **hour** of audio.
///
/// Whisper-class STT providers bill by the hour of audio submitted, a unit that
/// has no per-token equivalent. Entries are keyed by normalized model id alone,
/// deliberately ignoring the provider: an operator routing `whisper-large-v3`
/// through a reseller must still see the vendor's official list price rather
/// than a blank cell, and provider-scoped keys were the reason audio always
/// priced at zero.
macro_rules! audio_pricing_catalog {
    ($( $model:literal => $per_hour:expr ),* $(,)?) => {{
        let mut table: HashMap<&'static str, f64> = HashMap::new();
        $( table.insert($model, $per_hour); )*
        table
    }};
}

static AUDIO_PRICING_TABLE: LazyLock<HashMap<String, f64>> = LazyLock::new(|| {
    // Keyed by *normalized model id*, never by provider: the rate belongs to the
    // model family, and reseller aliases (`@cf/openai/whisper-large-v3-turbo`,
    // `whisper-large-v3:free`) are the same model at a subsidised price. Only
    // the canonical published rate is stored, so a lookup can never return a
    // provider-specific discount as if it were the official price.
    let raw = audio_pricing_catalog! {
        // Groq — https://groq.com/pricing (10s minimum billed per request)
        "whisper-large-v3" => 0.111,
        "whisper-large-v3-turbo" => 0.04,
        // OpenAI — https://openai.com/api/pricing
        "whisper-1" => 0.36,
        "gpt-4o-transcribe" => 0.36,
        "gpt-4o-mini-transcribe" => 0.18,
    };
    let mut table: HashMap<String, f64> = HashMap::with_capacity(raw.len());
    for (model, rate) in raw {
        table.insert(normalize_audio_model_id(model), rate);
    }
    table
});

/// Normalize a model id for audio-price lookup.
///
/// Deliberately more aggressive than [`normalize_model_id`]: STT ids arrive
/// with provider prefixes (`openai/whisper-1`), reseller suffixes
/// (`whisper-large-v3:free`) and arbitrary casing. The price is a property of
/// the model family, so all of that has to collapse away.
fn normalize_audio_model_id(model: &str) -> String {
    // Provider prefix: keep only the last path segment.
    let mut s = model.rsplit('/').next().unwrap_or(model);
    s = s.trim();
    // Reseller / variant suffixes that never change the underlying model.
    loop {
        let stripped = [
            ":free", "-free", ":beta", "-beta", ":latest", "-latest", ":online", "-online",
        ]
        .iter()
        .find_map(|suffix| s.strip_suffix(suffix))
        .or_else(|| {
            // A trailing `@cf/...` style scope is already handled by the prefix
            // split above; date/version tails are not meaningful for STT ids.
            None
        });
        match stripped {
            Some(rest) if !rest.is_empty() => s = rest,
            _ => break,
        }
    }
    s.to_ascii_lowercase()
}

/// Look up hourly audio pricing for a model, ignoring the provider.
///
/// Falls back through the same normalization ladder as the token catalog so
/// `openai/whisper-large-v3`, `whisper-large-v3:free` and `Whisper-Large-V3`
/// all resolve to the same rate.
pub fn lookup_audio_price_per_hour(model: &str) -> Option<f64> {
    AUDIO_PRICING_TABLE
        .get(&normalize_audio_model_id(model))
        .copied()
}

/// Build a [`Price`] for an audio model from the hourly table.
///
/// `input_per_1m` mirrors the hourly rate so callers that only understand the
/// token-shaped [`Price`] still see a non-zero number; cost math for
/// [`PriceKind::Audio`] reads [`Price::audio_per_hour`] exclusively.
pub fn lookup_audio_price(model: &str) -> Option<Price> {
    let per_hour = lookup_audio_price_per_hour(model)?;
    Some(Price {
        input_per_1m: per_hour,
        output_per_1m: 0.0,
        kind: PriceKind::Audio,
        audio_per_hour: Some(per_hour),
    })
}

static PRICING_TABLE: LazyLock<HashMap<(&'static str, &'static str), Price>> =
    LazyLock::new(|| {
        pricing_catalog! {
            // OpenRouter
            ("openrouter", "anthropic/claude-sonnet-4") => (3.0, 15.0),
            ("openrouter", "anthropic/claude-3-5-sonnet") => (3.0, 15.0),
            ("openrouter", "anthropic/claude-3-haiku") => (0.25, 1.25),
            ("openrouter", "openai/gpt-4o") => (2.5, 10.0),
            ("openrouter", "openai/gpt-4o-mini") => (0.15, 0.6),
            ("openrouter", "openai/gpt-4.1") => (2.0, 8.0),
            ("openrouter", "openai/gpt-4.1-mini") => (0.4, 1.6),
            ("openrouter", "google/gemini-2.5-pro") => (1.25, 10.0),
            ("openrouter", "google/gemini-2.5-flash") => (0.075, 0.30),
            ("openrouter", "meta-llama/llama-3.3-70b-instruct") => (0.59, 0.79),
            ("openrouter", "deepseek/deepseek-chat") => (0.14, 0.28),
            ("openrouter", "deepseek/deepseek-r1") => (0.55, 2.19),
            ("openrouter", "qwen/qwen-2.5-72b-instruct") => (0.23, 0.40),
            ("openrouter", "mistralai/mistral-large") => (2.0, 6.0),
            ("openrouter", "x-ai/grok-2") => (2.0, 10.0),

            // MiniMax
            ("minimax", "minimax-m2.1") => (0.2, 0.2),
            ("minimax", "MiniMax-M2") => (0.2, 0.2),
            ("minimax", "MiniMax-M3") => (1.0, 1.0),
            ("nvidia-nim", "minimaxai/minimax-m3") => (1.0, 1.0),
            ("tokenrouter", "MiniMax-M3") => (1.0, 1.0),

            // NVIDIA NIM
            ("nvidia-nim", "meta/llama-3.3-70b-instruct") => (0.77, 0.77),
            ("nvidia-nim", "meta/llama-3.1-8b-instruct") => (0.18, 0.18),
            ("nvidia-nim", "nvidia/nemotron-3-ultra-550b-a55b") => (0.77, 0.77),
            ("nvidia-nim", "moonshotai/kimi-k2.6") => (0.60, 2.50),
            ("nvidia-nim", "z-ai/glm-5.1") => (0.14, 0.28),
            ("nvidia-nim", "z-ai/glm-4.6") => (0.14, 0.28),

            // z.ai GLM
            ("zenmux", "z-ai/glm-5.2") => (0.14, 0.28),

            // Vercel AI Gateway
            ("vercel-gateway", "zai/glm-5.2") => (0.0, 0.0),
            ("vercel-gateway", "zai/glm-5.2-fast") => (0.0, 0.0),
        }
    });

pub fn lookup(provider: &str, model: &str) -> Option<Price> {
    if let Some(price) = PRICING_TABLE.get(&(provider, model)) {
        return Some(*price);
    }
    if let Some((_, price)) = PRICING_TABLE.iter().find(|((_, m), _)| *m == model) {
        return Some(*price);
    }
    let normalized = normalize_model_id(model);
    if let Some((_, price)) = PRICING_TABLE
        .iter()
        .find(|((_, m), _)| normalize_model_id(m) == normalized)
    {
        return Some(*price);
    }
    None
}

fn lookup_db_variants(conn: &Connection, provider: &str, model: &str) -> Option<Price> {
    if let Some(p) = lookup_exact_in_db(conn, provider, model) {
        return Some(p);
    }
    for stripped in candidate_normalized_forms(model) {
        if let Some(p) = lookup_exact_in_db(conn, provider, stripped) {
            return Some(p);
        }
    }
    let with_free = format!("{model}-free");
    let with_colon = format!("{model}:free");
    lookup_exact_in_db(conn, provider, &with_free)
        .or_else(|| lookup_exact_in_db(conn, provider, &with_colon))
}

fn lookup_db_normalized_variants(conn: &Connection, model: &str) -> Option<Price> {
    let normalized = normalize_model_id(model);
    if let Some(p) = lookup_by_normalized(conn, &normalized) {
        return Some(p);
    }
    for stripped in candidate_normalized_forms(model) {
        let norm = normalize_model_id(stripped);
        if let Some(p) = lookup_by_normalized(conn, &norm) {
            return Some(p);
        }
    }
    None
}

fn lookup_static_variants(provider: &str, model: &str) -> Option<Price> {
    if let Some(p) = lookup(provider, model) {
        return Some(p);
    }
    for stripped in candidate_normalized_forms(model) {
        if let Some(p) = lookup(provider, stripped) {
            return Some(p);
        }
    }
    None
}

pub fn lookup_with_db(conn: &Connection, provider: &str, model: &str) -> Option<Price> {
    // Audio first and provider-agnostic: whisper-class models are priced per
    // hour of audio and are routinely routed through resellers that models.dev
    // lists with `cost: null`. A provider-scoped DB row would shadow the real
    // list price, so the audio catalog wins whenever the model is known.
    if let Some(price) = lookup_audio_price(model) {
        return Some(price);
    }
    lookup_db_variants(conn, provider, model)
        .or_else(|| lookup_db_normalized_variants(conn, model))
        .or_else(|| lookup_static_variants(provider, model))
}

crate::def_table_select!(
    pricing_select,
    "model_capabilities_sync",
    "pricing_input_per_1m, pricing_output_per_1m"
);

fn lookup_exact_in_db(conn: &Connection, provider: &str, model: &str) -> Option<Price> {
    use rusqlite::OptionalExtension;
    let result: Result<Option<(f64, f64)>, _> = conn
        .query_row(
            pricing_select!(
                "WHERE provider_id = ?1 AND model_id = ?2 \
                 AND pricing_input_per_1m IS NOT NULL \
                 AND pricing_output_per_1m IS NOT NULL"
            ),
            rusqlite::params![provider, model],
            |row| crate::map_row_tuple!(row => ((0, f64), (1, f64))),
        )
        .optional();
    result.ok().flatten().map(|(inp, out)| Price {
        input_per_1m: inp,
        output_per_1m: out,
        ..Default::default()
    })
}

pub fn lookup_by_normalized(conn: &Connection, normalized: &str) -> Option<Price> {
    use rusqlite::OptionalExtension;
    let result: Result<Option<(f64, f64)>, _> = conn
        .query_row(
            pricing_select!(
                "WHERE model_id_normalized = ?1 \
                 AND pricing_input_per_1m IS NOT NULL \
                 AND pricing_output_per_1m IS NOT NULL \
                 LIMIT 1"
            ),
            rusqlite::params![normalized],
            |row| crate::map_row_tuple!(row => ((0, f64), (1, f64))),
        )
        .optional();
    result.ok().flatten().map(|(inp, out)| Price {
        input_per_1m: inp,
        output_per_1m: out,
        ..Default::default()
    })
}

pub fn compute_cost_opt_with_cache(
    price: Option<Price>,
    prompt_tokens: u32,
    completion_tokens: u32,
    cached_tokens: Option<u32>,
) -> Option<f64> {
    let price = price?;
    match price.kind {
        // Audio bills by duration, never by token. `prompt_tokens` carries
        // *milliseconds* of audio on this path (see `UsageInput::audio_ms`), so
        // the per-hour rate converts as ms / 3_600_000 * usd_per_hour.
        PriceKind::Audio => {
            let per_hour = price
                .audio_per_hour
                .unwrap_or_else(|| price.input_per_1m);
            let hours = f64::from(prompt_tokens) / 3_600_000.0;
            Some(per_hour * hours)
        }
        PriceKind::Image => Some(price.input_per_1m * f64::from(prompt_tokens) / 1_000_000.0),
        _ => {
            let cached = cached_tokens.unwrap_or(0).min(prompt_tokens);
            let non_cached = prompt_tokens.saturating_sub(cached);
            // Default cache read discount is 50% of input rate
            let cached_price_per_1m = price.input_per_1m * 0.5;
            let input_cost = (price.input_per_1m * f64::from(non_cached)
                + cached_price_per_1m * f64::from(cached))
                / 1_000_000.0;
            let output_cost = price.output_per_1m * f64::from(completion_tokens) / 1_000_000.0;
            Some(input_cost + output_cost)
        }
    }
}

pub fn compute_cost_opt(
    price: Option<Price>,
    prompt_tokens: u32,
    completion_tokens: u32,
) -> Option<f64> {
    compute_cost_opt_with_cache(price, prompt_tokens, completion_tokens, None)
}

pub fn compute_cost_with_cache(
    price: Option<Price>,
    prompt_tokens: u32,
    completion_tokens: u32,
    cached_tokens: Option<u32>,
) -> f64 {
    compute_cost_opt_with_cache(price, prompt_tokens, completion_tokens, cached_tokens)
        .unwrap_or(0.0)
}

pub fn compute_cost(price: Option<Price>, prompt_tokens: u32, completion_tokens: u32) -> f64 {
    compute_cost_opt(price, prompt_tokens, completion_tokens).unwrap_or(0.0)
}

pub fn lookup_price(provider: &str, model: &str) -> Option<Price> {
    lookup(provider, model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_openrouter_model() {
        let price = lookup("openrouter", "openai/gpt-4o").unwrap();
        assert!((price.input_per_1m - 2.5).abs() < f64::EPSILON);
        assert!((price.output_per_1m - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn known_minimax_model() {
        let price = lookup("minimax", "minimax-m2.1").unwrap();
        assert!((price.input_per_1m - 0.2).abs() < f64::EPSILON);
        assert!((price.output_per_1m - 0.2).abs() < f64::EPSILON);
    }

    #[test]
    fn compute_cost_basic() {
        let price = Some(Price {
            input_per_1m: 1.0,
            output_per_1m: 2.0,
            ..Default::default()
        });
        // 1.0 * 1000 / 1e6 + 2.0 * 500 / 1e6 = 0.001 + 0.001 = 0.002
        let cost = compute_cost(price, 1000, 500);
        assert!((cost - 0.002).abs() < 1e-12);
    }

    #[test]
    fn compute_cost_with_zero_tokens() {
        let price = Some(Price {
            input_per_1m: 5.0,
            output_per_1m: 10.0,
            ..Default::default()
        });
        assert!(compute_cost(price, 0, 0).abs() < f64::EPSILON);
    }

    #[test]
    fn compute_cost_unknown_pricing() {
        // None means "unknown" — treat as free, no panic.
        assert!(compute_cost(None, 1_000_000, 1_000_000).abs() < f64::EPSILON);
    }

    #[test]
    fn pricing_lookup_is_deterministic() {
        let a = lookup("openrouter", "anthropic/claude-sonnet-4").unwrap();
        let b = lookup("openrouter", "anthropic/claude-sonnet-4").unwrap();
        assert!((a.input_per_1m - b.input_per_1m).abs() < f64::EPSILON);
        assert!((a.output_per_1m - b.output_per_1m).abs() < f64::EPSILON);
        // Cross-provider fallback: a model registered under "openrouter"
        // can be found via a different provider id.
        let cross = lookup("minimax", "anthropic/claude-sonnet-4").unwrap();
        assert!((cross.input_per_1m - a.input_per_1m).abs() < f64::EPSILON);
    }

    #[test]
    fn pricing_lookup_cross_provider_matches_minimax_m3() {
        // MiniMax-M3 is registered under ("minimax", "MiniMax-M3").
        // A request from "tokenrouter" with model "MiniMax-M3" should
        // still find the price via the cross-provider fallback.
        let price = lookup("tokenrouter", "MiniMax-M3").unwrap();
        assert!((price.input_per_1m - 1.0).abs() < f64::EPSILON);
        assert!((price.output_per_1m - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn pricing_lookup_truly_unknown_returns_none() {
        // A model that doesn't exist in ANY provider's entry.
        assert!(lookup("openrouter", "no/such-model-xyz").is_none());
        assert!(lookup("unknown-provider", "whatever").is_none());
    }

    #[test]
    fn default_pricing_kind_is_chat() {
        let price = Price {
            input_per_1m: 1.0,
            output_per_1m: 2.0,
            ..Default::default()
        };
        assert_eq!(price.kind, PriceKind::Chat);
    }

    #[test]
    fn serde_default_pricing_kind_is_chat() {
        let json = r#"{"input_per_1m": 1.0, "output_per_1m": 2.0}"#;
        let price: Price = serde_json::from_str(json).unwrap();
        assert_eq!(price.kind, PriceKind::Chat);
    }

    #[test]
    fn compute_cost_audio_dispatch() {
        // Audio bills by duration: `prompt_tokens` carries milliseconds and the
        // rate is USD per hour.
        let price = Some(Price {
            input_per_1m: 0.0,
            output_per_1m: 0.0,
            kind: PriceKind::Audio,
            audio_per_hour: Some(3.6),
        });
        // 60_000 ms = 1/60 h => 3.6 / 60 = 0.06
        let cost = compute_cost(price, 60_000, 0);
        assert!((cost - 0.06).abs() < 1e-12, "got {cost}");
    }

    #[test]
    fn compute_cost_image_dispatch() {
        let price = Some(Price {
            input_per_1m: 10.0,
            output_per_1m: 0.0,
            kind: PriceKind::Image,
            audio_per_hour: None,
        });
        let cost = compute_cost(price, 4, 0);
        assert!((cost - 40.0 / 1_000_000.0).abs() < 1e-15);
    }

    #[test]
    fn compute_cost_with_cache_discount() {
        let price = Some(Price {
            input_per_1m: 3.0,
            output_per_1m: 15.0,
            kind: PriceKind::Chat,
            audio_per_hour: None,
        });
        // 1000 prompt tokens total, 800 cached, 200 uncached, 100 completion tokens
        // non_cached cost = 200 * 3.0 / 1M = 0.0006
        // cached cost = 800 * 1.5 / 1M = 0.0012
        // completion cost = 100 * 15.0 / 1M = 0.0015
        // total = 0.0033
        let cost = compute_cost_with_cache(price, 1000, 100, Some(800));
        let expected = (200.0 * 3.0 + 800.0 * 1.5 + 100.0 * 15.0) / 1_000_000.0;
        assert!((cost - expected).abs() < 1e-9);
    }

    #[test]
    fn compute_cost_chat_dispatch_ignores_completion_for_audio() {
        let price = Some(Price {
            input_per_1m: 0.0,
            output_per_1m: 999_999.0,
            kind: PriceKind::Audio,
            audio_per_hour: Some(0.36),
        });
        // 10_000 ms = 1/360 h => 0.36 / 360 = 0.001. Completion tokens are
        // irrelevant: audio is never billed per output token.
        let cost = compute_cost(price, 10_000, 1_000_000);
        assert!((cost - 0.001).abs() < 1e-12, "got {cost}");
    }

    #[test]
    fn compute_cost_unknown_kind_falls_back_to_chat() {
        let price = Some(Price {
            input_per_1m: 1.0,
            output_per_1m: 2.0,
            kind: PriceKind::Other,
            audio_per_hour: None,
        });
        let cost = compute_cost(price, 1000, 500);
        assert!((cost - 0.002).abs() < 1e-12);
    }

    #[test]
    fn audio_price_matches_by_model_ignoring_provider() {
        // Whisper-class models are billed per hour of audio. The rate must come
        // from the model alone: a reseller id or a `:free` suffix must not hide
        // the vendor's official list price.
        let direct = lookup_audio_price_per_hour("whisper-large-v3").unwrap();
        assert!((direct - 0.111).abs() < f64::EPSILON);
        assert_eq!(
            lookup_audio_price_per_hour("openai/whisper-large-v3"),
            Some(direct)
        );
        assert_eq!(
            lookup_audio_price_per_hour("whisper-large-v3:free"),
            Some(direct)
        );
        assert_eq!(
            lookup_audio_price_per_hour("Whisper-Large-V3"),
            Some(direct)
        );
    }

    #[test]
    fn audio_price_builds_audio_kind() {
        let p = lookup_audio_price("whisper-large-v3-turbo").unwrap();
        assert_eq!(p.kind, PriceKind::Audio);
        assert_eq!(p.audio_per_hour, Some(0.04));
    }

    #[test]
    fn audio_catalog_holds_only_official_model_prices() {
        // No provider-scoped entries: a Cloudflare/Workers-AI alias of the same
        // model must resolve to the model's official rate, not to that
        // provider's subsidised one. Otherwise the same audio costs a different
        // amount depending on which target happened to serve it.
        assert_eq!(
            lookup_audio_price_per_hour("@cf/openai/whisper-large-v3-turbo"),
            lookup_audio_price_per_hour("whisper-large-v3-turbo"),
        );
        assert_eq!(
            lookup_audio_price_per_hour("@cf/openai/whisper"),
            lookup_audio_price_per_hour("whisper"),
        );
        // Every key in the table is a bare model id.
        for key in AUDIO_PRICING_TABLE.keys() {
            assert!(
                !key.contains('/') && !key.contains('@'),
                "audio catalog key `{key}` is provider-scoped"
            );
        }
    }

    #[test]
    fn audio_cost_uses_hourly_rate_not_tokens() {
        // 4 seconds of Groq turbo => 4/3600 h * $0.04
        let p = lookup_audio_price("whisper-large-v3-turbo");
        let cost = compute_cost(p, 4_000, 0);
        let expected = 4.0 / 3600.0 * 0.04;
        assert!((cost - expected).abs() < 1e-15, "got {cost}");
    }

    #[test]
    fn audio_cost_scales_with_duration() {
        let p = lookup_audio_price("whisper-large-v3");
        let one_hour = compute_cost(p, 3_600_000, 0);
        assert!((one_hour - 0.111).abs() < 1e-12, "got {one_hour}");
        let half = compute_cost(p, 1_800_000, 0);
        assert!((half - 0.0555).abs() < 1e-12, "got {half}");
    }

    #[test]
    fn audio_cost_ignores_completion_tokens() {
        let p = lookup_audio_price("whisper-large-v3");
        let a = compute_cost(p, 60_000, 0);
        let b = compute_cost(p, 60_000, 999_999);
        assert!((a - b).abs() < 1e-15);
    }

    #[test]
    fn unknown_audio_model_has_no_price() {
        assert!(lookup_audio_price("no-such-stt-model").is_none());
    }
}
