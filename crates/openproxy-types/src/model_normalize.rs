//! Model ID normalization for matching against models.dev.
//!
//! Strips provider prefixes, free suffixes, date/version suffixes, and
//! normalizes known family naming variations (e.g. `gemini-2_5-pro` →
//! `gemini-2.5-pro`) so that `anthropic/claude-3-5-sonnet-20241022`
//! matches models.dev's `claude-3-5-sonnet`.

pub const MODEL_SUFFIXES: &[&str] = &[
    "-free-trial",
    "-free",
    ":free",
    "-low",
    "-high",
    "-medium",
    "-tiered",
    "-thinking",
    "-agent",
    "-preset",
    "-fast",
    "-turbo",
    ":thinking",
    ":online",
    ":extended",
    ":nitro",
];

pub fn normalize_model_id(id: &str) -> String {
    let mut s: &str = id.rsplit_once('/').map_or(id, |(_, rest)| rest);
    for suffix in MODEL_SUFFIXES {
        s = s.trim_end_matches(suffix);
    }

    let replaced = s.replace(':', "-");
    let mut cur = replaced.as_str();

    if let Some(stripped) = strip_date_suffix(cur) {
        cur = stripped;
    }
    if let Some(stripped) = strip_compact_yyyymmdd(cur) {
        cur = stripped;
    }
    if let Some(stripped) = strip_4digit_suffix(cur) {
        cur = stripped;
    }
    if let Some(stripped) = strip_version_suffix(cur) {
        cur = stripped;
    }

    normalize_family(cur)
}

fn strip_date_suffix(s: &str) -> Option<&str> {
    if s.len() <= 11 {
        return None;
    }
    let idx = s.len() - 11;
    if !s.is_char_boundary(idx) {
        return None;
    }
    let suffix = &s[idx..];
    let bytes = suffix.as_bytes();
    if bytes[0] == b'-'
        && bytes[1..5].iter().all(u8::is_ascii_digit)
        && bytes[5] == b'-'
        && bytes[6..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'-'
        && bytes[9..11].iter().all(u8::is_ascii_digit)
    {
        Some(&s[..idx])
    } else {
        None
    }
}

fn strip_4digit_suffix(s: &str) -> Option<&str> {
    if s.len() <= 5 {
        return None;
    }
    let idx = s.len() - 5;
    if !s.is_char_boundary(idx) {
        return None;
    }
    let suffix = &s[idx..];
    let bytes = suffix.as_bytes();
    if bytes[0] == b'-' && bytes[1..5].iter().all(u8::is_ascii_digit) {
        Some(&s[..idx])
    } else {
        None
    }
}

fn strip_version_suffix(s: &str) -> Option<&str> {
    let (prefix, rest) = s.rsplit_once("-v")?;
    if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
        Some(prefix)
    } else {
        None
    }
}

fn strip_compact_yyyymmdd(s: &str) -> Option<&str> {
    if s.len() <= 9 {
        return None;
    }
    let idx = s.len() - 9;
    if !s.is_char_boundary(idx) {
        return None;
    }
    let suffix = &s[idx..];
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
    Some(&s[..idx])
}

fn normalize_family(s: &str) -> String {
    if s.starts_with("gemini-") {
        return s.replace('_', ".");
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
        assert_eq!(
            normalize_model_id("claude-sonnet-4-20250514"),
            "claude-sonnet-4"
        );
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
        assert_eq!(
            normalize_model_id("meta-llama/llama-3.3-70b-instruct"),
            "llama-3.3-70b-instruct"
        );
        assert_eq!(
            normalize_model_id("qwen/qwen2.5-72b-instruct"),
            "qwen2.5-72b-instruct"
        );
    }

    #[test]
    fn normalizes_colons_to_dashes() {
        assert_eq!(normalize_model_id("gpt-oss:120b"), "gpt-oss-120b");
        assert_eq!(normalize_model_id("gpt-oss:120b:free"), "gpt-oss-120b");
        assert_eq!(normalize_model_id("llama3:8b"), "llama3-8b");
    }

    #[test]
    fn strip_compact_yyyymmdd_edge_cases() {
        // Valid compact form with 19xx or 20xx
        assert_eq!(
            strip_compact_yyyymmdd("claude-3-5-sonnet-20241022"),
            Some("claude-3-5-sonnet")
        );
        assert_eq!(strip_compact_yyyymmdd("model-19991231"), Some("model"));

        // Too short (len <= 9)
        assert_eq!(strip_compact_yyyymmdd("m-202410"), None);

        // Does not start with dash in the suffix
        assert_eq!(strip_compact_yyyymmdd("model_20241022"), None);
        assert_eq!(strip_compact_yyyymmdd("model20241022"), None);

        // Digits len != 8
        assert_eq!(strip_compact_yyyymmdd("model-2024102"), None);
        assert_eq!(strip_compact_yyyymmdd("model-202410223"), None);

        // Non-digit characters
        assert_eq!(strip_compact_yyyymmdd("model-2024102a"), None);

        // Invalid year (doesn't start with 19 or 20)
        assert_eq!(strip_compact_yyyymmdd("model-21241022"), None);
        assert_eq!(strip_compact_yyyymmdd("model-18991231"), None);
    }

    #[test]
    fn handles_multibyte_utf8_safely() {
        assert_eq!(normalize_model_id("🤖-20241022"), "🤖");
        assert_eq!(normalize_model_id("modèle-2024-01-01"), "modèle");
        assert_eq!(normalize_model_id("🦀-v1"), "🦀");
    }
}
