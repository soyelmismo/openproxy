use openproxy_types::{candidate_normalized_forms, normalize_model_id};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Price {
    pub input_per_1m: f64,
    pub output_per_1m: f64,
    #[serde(default = "default_pricing_kind")]
    pub kind: String,
}

fn default_pricing_kind() -> String {
    "chat".to_string()
}

impl Default for Price {
    fn default() -> Self {
        Self {
            input_per_1m: 0.0,
            output_per_1m: 0.0,
            kind: "chat".to_string(),
        }
    }
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
                    $(kind: $kind.to_string(),)?
                    ..Default::default()
                },
            );
        )*
        table
    }};
}

static PRICING_TABLE: LazyLock<HashMap<(&'static str, &'static str), Price>> = LazyLock::new(|| {
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
    }
});

pub fn lookup(provider: &str, model: &str) -> Option<Price> {
    if let Some(price) = PRICING_TABLE.get(&(provider, model)) {
        return Some(price.clone());
    }
    if let Some((_, price)) = PRICING_TABLE.iter().find(|((_, m), _)| *m == model) {
        return Some(price.clone());
    }
    let normalized = normalize_model_id(model);
    if let Some((_, price)) = PRICING_TABLE
        .iter()
        .find(|((_, m), _)| normalize_model_id(m) == normalized)
    {
        return Some(price.clone());
    }
    None
}

pub fn lookup_with_db(conn: &Connection, provider: &str, model: &str) -> Option<Price> {
    if let Some(p) = lookup_exact_in_db(conn, provider, model) {
        return Some(p);
    }
    for stripped in candidate_normalized_forms(model) {
        if let Some(p) = lookup_exact_in_db(conn, provider, stripped) {
            return Some(p);
        }
    }
    let with_free = format!("{model}-free");
    if let Some(p) = lookup_exact_in_db(conn, provider, &with_free) {
        return Some(p);
    }
    let with_colon = format!("{model}:free");
    if let Some(p) = lookup_exact_in_db(conn, provider, &with_colon) {
        return Some(p);
    }
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
    match price.kind.as_str() {
        "audio" => {
            let seconds = f64::from(prompt_tokens) / 1000.0;
            Some(price.input_per_1m * seconds / 1_000_000.0)
        }
        "image" => Some(price.input_per_1m * f64::from(prompt_tokens) / 1_000_000.0),
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
        assert_eq!(price.kind, "chat");
    }

    #[test]
    fn serde_default_pricing_kind_is_chat() {
        let json = r#"{"input_per_1m": 1.0, "output_per_1m": 2.0}"#;
        let price: Price = serde_json::from_str(json).unwrap();
        assert_eq!(price.kind, "chat");
    }

    #[test]
    fn compute_cost_audio_dispatch() {
        let price = Some(Price {
            input_per_1m: 1.0,
            output_per_1m: 0.0,
            kind: "audio".to_string(),
        });
        let cost = compute_cost(price, 60_000, 0);
        assert!((cost - 60.0 / 1_000_000.0).abs() < 1e-15);
    }

    #[test]
    fn compute_cost_image_dispatch() {
        let price = Some(Price {
            input_per_1m: 10.0,
            output_per_1m: 0.0,
            kind: "image".to_string(),
        });
        let cost = compute_cost(price, 4, 0);
        assert!((cost - 40.0 / 1_000_000.0).abs() < 1e-15);
    }

    #[test]
    fn compute_cost_with_cache_discount() {
        let price = Some(Price {
            input_per_1m: 3.0,
            output_per_1m: 15.0,
            kind: "chat".to_string(),
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
            input_per_1m: 1.0,
            output_per_1m: 999_999.0,
            kind: "audio".to_string(),
        });
        let cost = compute_cost(price, 10_000, 1_000_000);
        assert!((cost - 10.0 / 1_000_000.0).abs() < 1e-12);
    }

    #[test]
    fn compute_cost_unknown_kind_falls_back_to_chat() {
        let price = Some(Price {
            input_per_1m: 1.0,
            output_per_1m: 2.0,
            kind: "video".to_string(),
        });
        let cost = compute_cost(price, 1000, 500);
        assert!((cost - 0.002).abs() < 1e-12);
    }
}
