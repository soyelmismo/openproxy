//! Model ID normalization for matching against models.dev.
//!
//! Strips provider prefixes, free suffixes, date/version suffixes, and
//! normalizes known family naming variations (e.g. `gemini-2_5-pro` →
//! `gemini-2.5-pro`) so that `anthropic/claude-3-5-sonnet-20241022`
//! matches models.dev's `claude-3-5-sonnet`.

use once_cell::sync::Lazy;
use regex::Regex;

static DATE_SUFFIX_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"-\d{4}-\d{2}-\d{2}$").unwrap());
static YYYYMM_SUFFIX_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"-\d{4}$").unwrap());
static VERSION_SUFFIX_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"-v\d+$").unwrap());

/// Normalize a model ID for matching against models.dev canonical IDs.
///
/// Steps (applied in order):
/// 1. Strip provider prefix: `openai/gpt-4o` → `gpt-4o`,
///    `meta-llama/llama-3.3` → `llama-3.3`. Also handles multi-segment
///    prefixes like `cloudflare-workers-ai/@cf/meta/llama-3.1` →
///    `llama-3.1`.
/// 2. Strip free suffixes: `:free`, `-free-trial`, `-free`.
/// 3. Strip date suffixes: `-20241022`, `-2024-04-09`, `-20250514`.
/// 4. Strip YYYYMM version suffixes: `-2407` (Mistral convention).
/// 5. Strip `-vN` version suffixes: `-v1`, `-v2`.
/// 6. Normalize known family naming: `gemini-2_5-pro` → `gemini-2.5-pro`.
///
/// Returns the normalized ID. The original ID is preserved in the DB;
/// this is only used for matching.
pub fn normalize_model_id(id: &str) -> String {
    // 1. Strip provider prefix — take the last segment after any '/'.
    let s: &str = id.rsplit('/').next().unwrap_or(id);

    // 2. Strip free suffixes (order matters: longest first).
    let s: &str = s
        .trim_end_matches("-free-trial")
        .trim_end_matches("-free")
        .trim_end_matches(":free");

    // 3. Strip date suffixes: -YYYY-MM-DD (dashed form) or -YYYYMMDD
    //    (compact form). The compact form is only stripped when the
    //    4-digit year prefix looks like a real year (19xx or 20xx) so
    //    that legitimate 8-digit version numbers are left alone.
    let s: String = DATE_SUFFIX_RE.replace_all(s, "").into_owned();
    let s: String = strip_compact_yyyymmdd(&s).unwrap_or_else(|| s.clone());

    // 4. Strip YYYYMM version suffixes: -2407
    let s: String = YYYYMM_SUFFIX_RE.replace_all(&s, "").into_owned();

    // 5. Strip -vN version suffixes: -v1, -v2
    let s: String = VERSION_SUFFIX_RE.replace_all(&s, "").into_owned();

    // 6. Normalize known family naming variations.
    normalize_family(&s)
}

/// If `s` ends with `-YYYYMMDD` where YYYY starts with `19` or `20`,
/// return the prefix without the suffix. Otherwise return `None`.
fn strip_compact_yyyymmdd(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.len() <= 9 {
        return None;
    }
    let suffix = &s[s.len() - 9..];
    if !suffix.starts_with('-') {
        return None;
    }
    let digits = &suffix[1..];
    if digits.len() != 8 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year = &digits[..4];
    if !(year.starts_with("19") || year.starts_with("20")) {
        return None;
    }
    Some(s[..s.len() - 9].to_string())
}

/// Normalize known model family naming variations.
/// E.g. `gemini-2_5-pro` → `gemini-2.5-pro` (underscore → dot in version).
fn normalize_family(s: &str) -> String {
    // Gemini: gemini-2_5-pro → gemini-2.5-pro, gemini-1_5-flash → gemini-1.5-flash
    if s.starts_with("gemini-") {
        return s.replace("_", ".");
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_provider_prefix() {
        assert_eq!(normalize_model_id("openai/gpt-4o"), "gpt-4o");
        assert_eq!(
            normalize_model_id("anthropic/claude-3-5-sonnet"),
            "claude-3-5-sonnet"
        );
        assert_eq!(
            normalize_model_id("meta-llama/llama-3.3-70b-instruct"),
            "llama-3.3-70b-instruct"
        );
    }

    #[test]
    fn strips_multi_segment_prefix() {
        assert_eq!(
            normalize_model_id("cloudflare-workers-ai/@cf/meta/llama-3.1-8b-instruct"),
            "llama-3.1-8b-instruct"
        );
    }

    #[test]
    fn strips_free_suffixes() {
        assert_eq!(normalize_model_id("openai/gpt-4o:free"), "gpt-4o");
        assert_eq!(normalize_model_id("openai/gpt-4o-free"), "gpt-4o");
        assert_eq!(normalize_model_id("openai/gpt-4o-free-trial"), "gpt-4o");
    }

    #[test]
    fn strips_date_suffixes() {
        assert_eq!(
            normalize_model_id("anthropic/claude-3-5-sonnet-20241022"),
            "claude-3-5-sonnet"
        );
        assert_eq!(
            normalize_model_id("openai/gpt-4-turbo-2024-04-09"),
            "gpt-4-turbo"
        );
        assert_eq!(normalize_model_id("claude-sonnet-4-20250514"), "claude-sonnet-4");
        assert_eq!(
            normalize_model_id("claude-3-7-sonnet-20250219"),
            "claude-3-7-sonnet"
        );
    }

    #[test]
    fn strips_yyyymm_version_suffixes() {
        assert_eq!(
            normalize_model_id("mistral/mistral-large-2407"),
            "mistral-large"
        );
    }

    #[test]
    fn strips_v_n_version_suffixes() {
        assert_eq!(
            normalize_model_id("deepseek/deepseek-chat-v1"),
            "deepseek-chat"
        );
        assert_eq!(
            normalize_model_id("deepseek/deepseek-chat-v2"),
            "deepseek-chat"
        );
    }

    #[test]
    fn normalizes_gemini_underscores() {
        assert_eq!(
            normalize_model_id("google/gemini-2_5-pro"),
            "gemini-2.5-pro"
        );
        assert_eq!(normalize_model_id("gemini-1_5-flash"), "gemini-1.5-flash");
    }

    #[test]
    fn combined_variations() {
        assert_eq!(
            normalize_model_id("anthropic/claude-3-5-sonnet-20241022:free"),
            "claude-3-5-sonnet"
        );
        assert_eq!(
            normalize_model_id("openai/gpt-4o-2024-08-06-free"),
            "gpt-4o"
        );
    }

    #[test]
    fn bare_id_unchanged() {
        assert_eq!(normalize_model_id("gpt-4o"), "gpt-4o");
        assert_eq!(normalize_model_id("claude-3-5-sonnet"), "claude-3-5-sonnet");
    }

    #[test]
    fn does_not_strip_legitimate_numbers() {
        // `llama-3.3-70b-instruct` — 70b is not a date, should stay
        assert_eq!(
            normalize_model_id("meta-llama/llama-3.3-70b-instruct"),
            "llama-3.3-70b-instruct"
        );
        // `qwen2.5-72b-instruct` — no suffix to strip
        assert_eq!(
            normalize_model_id("qwen/qwen2.5-72b-instruct"),
            "qwen2.5-72b-instruct"
        );
    }
}
