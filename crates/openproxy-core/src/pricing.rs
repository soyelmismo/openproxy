//! Per-model pricing in USD per 1M tokens.
//! Source of truth varies by provider:
//! - OpenRouter: hardcoded map of known models. Default to NULL.
//! - MiniMax: hardcoded table (only minimax-m2.1 known in MVP: 0.2/0.2).
//! - OpenCode Zen: hardcoded small set; default NULL for unknown.

use once_cell::sync::Lazy;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Price {
    pub input_per_1m: f64,
    pub output_per_1m: f64,
}

static PRICING_TABLE: Lazy<HashMap<(&'static str, &'static str), Price>> = Lazy::new(|| {
    let mut table: HashMap<(&'static str, &'static str), Price> = HashMap::new();

    // OpenRouter
    table.insert(
        ("openrouter", "anthropic/claude-sonnet-4"),
        Price { input_per_1m: 3.0, output_per_1m: 15.0 },
    );
    table.insert(
        ("openrouter", "anthropic/claude-3-5-sonnet"),
        Price { input_per_1m: 3.0, output_per_1m: 15.0 },
    );
    table.insert(
        ("openrouter", "openai/gpt-4o"),
        Price { input_per_1m: 2.5, output_per_1m: 10.0 },
    );
    table.insert(
        ("openrouter", "openai/gpt-4o-mini"),
        Price { input_per_1m: 0.15, output_per_1m: 0.6 },
    );
    table.insert(
        ("openrouter", "openai/gpt-4.1"),
        Price { input_per_1m: 2.0, output_per_1m: 8.0 },
    );
    table.insert(
        ("openrouter", "google/gemini-2.5-pro"),
        Price { input_per_1m: 1.25, output_per_1m: 10.0 },
    );
    table.insert(
        ("openrouter", "google/gemini-2.5-flash"),
        Price { input_per_1m: 0.075, output_per_1m: 0.30 },
    );
    table.insert(
        ("openrouter", "meta-llama/llama-3.3-70b-instruct"),
        Price { input_per_1m: 0.59, output_per_1m: 0.79 },
    );
    table.insert(
        ("openrouter", "deepseek/deepseek-chat"),
        Price { input_per_1m: 0.14, output_per_1m: 0.28 },
    );

    // MiniMax
    table.insert(
        ("minimax", "minimax-m2.1"),
        Price { input_per_1m: 0.2, output_per_1m: 0.2 },
    );
    table.insert(
        ("minimax", "MiniMax-M2"),
        Price { input_per_1m: 0.2, output_per_1m: 0.2 },
    );

    // OpenCode Zen — empty in MVP, default NULL for unknown.

    table
});

/// Returns Some(Price) if the (provider, model) is in the static table.
/// Returns None if unknown — the caller decides whether to log WARN and treat as free.
pub fn lookup(provider: &str, model: &str) -> Option<Price> {
    PRICING_TABLE
        .iter()
        .find(|((p, m), _)| *p == provider && *m == model)
        .map(|(_, price)| *price)
}

/// Lookup pricing from the `model_capabilities_sync` table first, then
/// fall back to the static hardcoded table. This is called from the
/// usage recording path (`cost::record`) where a `&Connection` is
/// available.
///
/// Tiers (in order, first hit wins):
///   1. Exact `(provider, model)` match in the sync table.
///   2. Exact match after stripping common "free" suffixes
///      (`-free`, `:free`, `-free-trial`).
///   3. Exact match after appending `-free` / `:free` (user's model
///      might be the paid version but models.dev only has the -free
///      variant, or vice versa).
///   4. **Normalized match** — calls `normalize_model_id` on the
///      request's model id and matches against the sync table's
///      precomputed `model_id_normalized` column. This catches date
///      suffixes (`-20241022`), version suffixes (`-v1`, `-2407`),
///      provider prefixes (`openai/gpt-4o` → `gpt-4o`), free suffixes
///      (redundant with tier 2 but harmless), and family naming
///      variations (`gemini-2_5-pro` → `gemini-2.5-pro`).
///   5. Static hardcoded `PRICING_TABLE`.
pub fn lookup_with_db(conn: &Connection, provider: &str, model: &str) -> Option<Price> {
    // Try exact match first.
    if let Some(p) = lookup_exact_in_db(conn, provider, model) {
        return Some(p);
    }

    // Fallback 1: strip common "free" suffixes.
    for stripped in strip_free_suffixes(model) {
        if let Some(p) = lookup_exact_in_db(conn, provider, &stripped) {
            return Some(p);
        }
    }

    // Fallback 2: append -free (user's model might be the paid version
    // but models.dev only has the -free variant, or vice versa).
    let with_free = format!("{}-free", model);
    if let Some(p) = lookup_exact_in_db(conn, provider, &with_free) {
        return Some(p);
    }
    let with_colon = format!("{}:free", model);
    if let Some(p) = lookup_exact_in_db(conn, provider, &with_colon) {
        return Some(p);
    }

    // Fallback 3: normalized match against the sync table's
    // `model_id_normalized` column. `normalize_model_id` strips
    // provider prefixes, date/version suffixes, free suffixes, and
    // normalizes family naming, so e.g.
    //   "anthropic/claude-3-5-sonnet-20241022"
    // → "claude-3-5-sonnet" which matches the models.dev canonical id.
    let normalized = crate::model_normalize::normalize_model_id(model);
    if let Some(p) = lookup_by_normalized(conn, &normalized) {
        return Some(p);
    }

    // Fall back to static table.
    lookup(provider, model)
}

/// Try exact match in the sync table.
fn lookup_exact_in_db(conn: &Connection, provider: &str, model: &str) -> Option<Price> {
    use rusqlite::OptionalExtension;
    let result: Result<Option<(f64, f64)>, _> = conn.query_row(
        "SELECT pricing_input_per_1m, pricing_output_per_1m \
         FROM model_capabilities_sync \
         WHERE provider_id = ?1 AND model_id = ?2 \
           AND pricing_input_per_1m IS NOT NULL \
           AND pricing_output_per_1m IS NOT NULL",
        rusqlite::params![provider, model],
        |row| Ok((row.get::<_, f64>(0)?, row.get::<_, f64>(1)?)),
    ).optional();
    result.ok().flatten().map(|(inp, out)| Price {
        input_per_1m: inp,
        output_per_1m: out,
    })
}

/// Lookup pricing by `model_id_normalized` (ignoring provider). Used as
/// the cross-provider normalized fallback when the provider-specific
/// exact match fails.
///
/// This lets models from providers not in the PROVIDER_MAP, or whose
/// model IDs include date/version suffixes the sync table doesn't have
/// (e.g. `anthropic/claude-3-5-sonnet-20241022` → matches the sync
/// table's `claude-3-5-sonnet` row), still find their pricing from the
/// models.dev data.
fn lookup_by_normalized(conn: &Connection, normalized: &str) -> Option<Price> {
    use rusqlite::OptionalExtension;
    let result: Result<Option<(f64, f64)>, _> = conn.query_row(
        "SELECT pricing_input_per_1m, pricing_output_per_1m \
         FROM model_capabilities_sync \
         WHERE model_id_normalized = ?1 \
           AND pricing_input_per_1m IS NOT NULL \
           AND pricing_output_per_1m IS NOT NULL \
         LIMIT 1",
        rusqlite::params![normalized],
        |row| Ok((row.get::<_, f64>(0)?, row.get::<_, f64>(1)?)),
    ).optional();
    result.ok().flatten().map(|(inp, out)| Price {
        input_per_1m: inp,
        output_per_1m: out,
    })
}

/// Generate suffix-stripped variants of a model ID for fuzzy matching.
/// Returns up to 3 variants, longest suffix first.
fn strip_free_suffixes(model: &str) -> Vec<String> {
    let suffixes = ["-free-trial", "-free", ":free"];
    let mut out = Vec::new();
    for suffix in &suffixes {
        if let Some(stripped) = model.strip_suffix(suffix) {
            if !stripped.is_empty() {
                out.push(stripped.to_string());
            }
        }
    }
    out
}

/// Cost in USD for given token counts.
/// Returns 0.0 if price is None (unknown model — log WARN upstream).
pub fn compute_cost(price: Option<Price>, prompt_tokens: u32, completion_tokens: u32) -> f64 {
    let price = match price {
        Some(p) => p,
        None => return 0.0,
    };

    let input_cost = price.input_per_1m * (prompt_tokens as f64) / 1_000_000.0;
    let output_cost = price.output_per_1m * (completion_tokens as f64) / 1_000_000.0;
    input_cost + output_cost
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_openrouter_model() {
        let price = lookup("openrouter", "openai/gpt-4o").unwrap();
        assert_eq!(price.input_per_1m, 2.5);
        assert_eq!(price.output_per_1m, 10.0);
    }

    #[test]
    fn known_minimax_model() {
        let price = lookup("minimax", "minimax-m2.1").unwrap();
        assert_eq!(price.input_per_1m, 0.2);
        assert_eq!(price.output_per_1m, 0.2);
    }

    #[test]
    fn unknown_model_returns_none() {
        assert!(lookup("openrouter", "no/such-model").is_none());
        assert!(lookup("unknown-provider", "whatever").is_none());
    }

    #[test]
    fn compute_cost_basic() {
        let price = Some(Price { input_per_1m: 1.0, output_per_1m: 2.0 });
        // 1.0 * 1000 / 1e6 + 2.0 * 500 / 1e6 = 0.001 + 0.001 = 0.002
        let cost = compute_cost(price, 1000, 500);
        assert!((cost - 0.002).abs() < 1e-12);
    }

    #[test]
    fn compute_cost_with_zero_tokens() {
        let price = Some(Price { input_per_1m: 5.0, output_per_1m: 10.0 });
        assert_eq!(compute_cost(price, 0, 0), 0.0);
    }

    #[test]
    fn compute_cost_unknown_pricing() {
        // None means "unknown" — treat as free, no panic.
        assert_eq!(compute_cost(None, 1_000_000, 1_000_000), 0.0);
    }

    #[test]
    fn pricing_lookup_is_deterministic() {
        let a = lookup("openrouter", "anthropic/claude-sonnet-4").unwrap();
        let b = lookup("openrouter", "anthropic/claude-sonnet-4").unwrap();
        assert_eq!(a.input_per_1m, b.input_per_1m);
        assert_eq!(a.output_per_1m, b.output_per_1m);
        // Also: provider mismatch doesn't accidentally return the OpenRouter price.
        assert!(lookup("minimax", "anthropic/claude-sonnet-4").is_none());
    }
}
