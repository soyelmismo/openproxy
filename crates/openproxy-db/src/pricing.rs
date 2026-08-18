use openproxy_types::normalize_model_id;
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

static PRICING_TABLE: LazyLock<HashMap<(&'static str, &'static str), Price>> =
    LazyLock::new(|| {
        let mut table: HashMap<(&'static str, &'static str), Price> = HashMap::new();

        // OpenRouter
        table.insert(
            ("openrouter", "anthropic/claude-sonnet-4"),
            Price {
                input_per_1m: 3.0,
                output_per_1m: 15.0,
                ..Default::default()
            },
        );
        table.insert(
            ("openrouter", "anthropic/claude-3-5-sonnet"),
            Price {
                input_per_1m: 3.0,
                output_per_1m: 15.0,
                ..Default::default()
            },
        );
        table.insert(
            ("openrouter", "anthropic/claude-3-haiku"),
            Price {
                input_per_1m: 0.25,
                output_per_1m: 1.25,
                ..Default::default()
            },
        );
        table.insert(
            ("openrouter", "openai/gpt-4o"),
            Price {
                input_per_1m: 2.5,
                output_per_1m: 10.0,
                ..Default::default()
            },
        );
        table.insert(
            ("openrouter", "openai/gpt-4o-mini"),
            Price {
                input_per_1m: 0.15,
                output_per_1m: 0.6,
                ..Default::default()
            },
        );
        table.insert(
            ("openrouter", "openai/gpt-4.1"),
            Price {
                input_per_1m: 2.0,
                output_per_1m: 8.0,
                ..Default::default()
            },
        );
        table.insert(
            ("openrouter", "openai/gpt-4.1-mini"),
            Price {
                input_per_1m: 0.4,
                output_per_1m: 1.6,
                ..Default::default()
            },
        );
        table.insert(
            ("openrouter", "google/gemini-2.5-pro"),
            Price {
                input_per_1m: 1.25,
                output_per_1m: 10.0,
                ..Default::default()
            },
        );
        table.insert(
            ("openrouter", "google/gemini-2.5-flash"),
            Price {
                input_per_1m: 0.075,
                output_per_1m: 0.30,
                ..Default::default()
            },
        );
        table.insert(
            ("openrouter", "meta-llama/llama-3.3-70b-instruct"),
            Price {
                input_per_1m: 0.59,
                output_per_1m: 0.79,
                ..Default::default()
            },
        );
        table.insert(
            ("openrouter", "deepseek/deepseek-chat"),
            Price {
                input_per_1m: 0.14,
                output_per_1m: 0.28,
                ..Default::default()
            },
        );
        table.insert(
            ("openrouter", "deepseek/deepseek-r1"),
            Price {
                input_per_1m: 0.55,
                output_per_1m: 2.19,
                ..Default::default()
            },
        );
        table.insert(
            ("openrouter", "qwen/qwen-2.5-72b-instruct"),
            Price {
                input_per_1m: 0.23,
                output_per_1m: 0.40,
                ..Default::default()
            },
        );
        table.insert(
            ("openrouter", "mistralai/mistral-large"),
            Price {
                input_per_1m: 2.0,
                output_per_1m: 6.0,
                ..Default::default()
            },
        );
        table.insert(
            ("openrouter", "x-ai/grok-2"),
            Price {
                input_per_1m: 2.0,
                output_per_1m: 10.0,
                ..Default::default()
            },
        );

        // MiniMax
        table.insert(
            ("minimax", "minimax-m2.1"),
            Price {
                input_per_1m: 0.2,
                output_per_1m: 0.2,
                ..Default::default()
            },
        );
        table.insert(
            ("minimax", "MiniMax-M2"),
            Price {
                input_per_1m: 0.2,
                output_per_1m: 0.2,
                ..Default::default()
            },
        );
        table.insert(
            ("minimax", "MiniMax-M3"),
            Price {
                input_per_1m: 1.0,
                output_per_1m: 1.0,
                ..Default::default()
            },
        );
        table.insert(
            ("nvidia-nim", "minimaxai/minimax-m3"),
            Price {
                input_per_1m: 1.0,
                output_per_1m: 1.0,
                ..Default::default()
            },
        );
        table.insert(
            ("tokenrouter", "MiniMax-M3"),
            Price {
                input_per_1m: 1.0,
                output_per_1m: 1.0,
                ..Default::default()
            },
        );

        // NVIDIA NIM
        table.insert(
            ("nvidia-nim", "meta/llama-3.3-70b-instruct"),
            Price {
                input_per_1m: 0.77,
                output_per_1m: 0.77,
                ..Default::default()
            },
        );
        table.insert(
            ("nvidia-nim", "meta/llama-3.1-8b-instruct"),
            Price {
                input_per_1m: 0.18,
                output_per_1m: 0.18,
                ..Default::default()
            },
        );
        table.insert(
            ("nvidia-nim", "nvidia/nemotron-3-ultra-550b-a55b"),
            Price {
                input_per_1m: 0.77,
                output_per_1m: 0.77,
                ..Default::default()
            },
        );
        table.insert(
            ("nvidia-nim", "moonshotai/kimi-k2.6"),
            Price {
                input_per_1m: 0.60,
                output_per_1m: 2.50,
                ..Default::default()
            },
        );
        table.insert(
            ("nvidia-nim", "z-ai/glm-5.1"),
            Price {
                input_per_1m: 0.14,
                output_per_1m: 0.28,
                ..Default::default()
            },
        );
        table.insert(
            ("nvidia-nim", "z-ai/glm-4.6"),
            Price {
                input_per_1m: 0.14,
                output_per_1m: 0.28,
                ..Default::default()
            },
        );

        // z.ai GLM
        table.insert(
            ("zenmux", "z-ai/glm-5.2"),
            Price {
                input_per_1m: 0.14,
                output_per_1m: 0.28,
                ..Default::default()
            },
        );

        table
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
    let candidates = strip_model_suffixes(model);
    for stripped in &candidates {
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
    for stripped in &candidates {
        let norm = normalize_model_id(stripped);
        if let Some(p) = lookup_by_normalized(conn, &norm) {
            return Some(p);
        }
    }
    if let Some(p) = lookup(provider, model) {
        return Some(p);
    }
    for stripped in &candidates {
        if let Some(p) = lookup(provider, stripped) {
            return Some(p);
        }
    }
    None
}

fn lookup_exact_in_db(conn: &Connection, provider: &str, model: &str) -> Option<Price> {
    use rusqlite::OptionalExtension;
    let result: Result<Option<(f64, f64)>, _> = conn
        .query_row(
            "SELECT pricing_input_per_1m, pricing_output_per_1m \
         FROM model_capabilities_sync \
         WHERE provider_id = ?1 AND model_id = ?2 \
           AND pricing_input_per_1m IS NOT NULL \
           AND pricing_output_per_1m IS NOT NULL",
            rusqlite::params![provider, model],
            |row| Ok((row.get::<_, f64>(0)?, row.get::<_, f64>(1)?)),
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
            "SELECT pricing_input_per_1m, pricing_output_per_1m \
         FROM model_capabilities_sync \
         WHERE model_id_normalized = ?1 \
           AND pricing_input_per_1m IS NOT NULL \
           AND pricing_output_per_1m IS NOT NULL \
         LIMIT 1",
            rusqlite::params![normalized],
            |row| Ok((row.get::<_, f64>(0)?, row.get::<_, f64>(1)?)),
        )
        .optional();
    result.ok().flatten().map(|(inp, out)| Price {
        input_per_1m: inp,
        output_per_1m: out,
        ..Default::default()
    })
}

fn strip_model_suffixes(model: &str) -> Vec<&str> {
    let mut results = Vec::new();
    let mut current = model;

    while let Some(stripped) = openproxy_types::model_normalize::MODEL_SUFFIXES
        .iter()
        .find_map(|s| {
            current
                .strip_suffix(s)
                .filter(|rem| !rem.is_empty() && *rem != current)
        })
    {
        results.push(stripped);
        current = stripped;
    }

    results
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
