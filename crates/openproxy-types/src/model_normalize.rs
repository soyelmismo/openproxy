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

/// Produces iterative candidate stripped forms of a model identifier by repeatedly
/// removing suffixes defined in `MODEL_SUFFIXES` without heap allocation.
pub fn candidate_normalized_forms(model: &str) -> impl Iterator<Item = &str> {
    let mut current = model;
    std::iter::from_fn(move || {
        let stripped = MODEL_SUFFIXES.iter().find_map(|s| {
            current
                .strip_suffix(s)
                .filter(|rem| !rem.is_empty() && *rem != current)
        })?;
        current = stripped;
        Some(stripped)
    })
}

fn strip_all_dynamic_suffixes(s: &str) -> &str {
    let mut cur = s;
    let strippers: [fn(&str) -> Option<&str>; 4] = [
        strip_date_suffix,
        strip_compact_yyyymmdd,
        strip_4digit_suffix,
        strip_version_suffix,
    ];
    for stripper in strippers {
        if let Some(stripped) = stripper(cur) {
            cur = stripped;
        }
    }
    cur
}

pub fn normalize_model_id(id: &str) -> String {
    let mut s: &str = id.rsplit_once('/').map_or(id, |(_, rest)| rest);
    for suffix in MODEL_SUFFIXES {
        s = s.trim_end_matches(suffix);
    }

    let replaced: std::borrow::Cow<'_, str> = if s.contains(':') {
        std::borrow::Cow::Owned(s.replace(':', "-"))
    } else {
        std::borrow::Cow::Borrowed(s)
    };

    let cur = strip_all_dynamic_suffixes(replaced.as_ref());
    normalize_family(cur).into_owned()
}

fn strip_fixed_suffix<'a, F>(s: &'a str, suffix_len: usize, predicate: F) -> Option<&'a str>
where
    F: FnOnce(&'a str) -> bool,
{
    if s.len() <= suffix_len {
        return None;
    }
    let idx = s.len() - suffix_len;
    if !s.is_char_boundary(idx) {
        return None;
    }
    if predicate(s.get(idx..).unwrap_or("")) {
        Some(s.get(..idx).unwrap_or(s))
    } else {
        None
    }
}

fn is_dash_date_suffix(suffix: &str) -> bool {
    let bytes = suffix.as_bytes();
    if bytes.len() != 11 {
        return false;
    }
    bytes[0] == b'-'
        && bytes[5] == b'-'
        && bytes[8] == b'-'
        && bytes[1..5]
            .iter()
            .chain(&bytes[6..8])
            .chain(&bytes[9..11])
            .all(u8::is_ascii_digit)
}

fn strip_date_suffix(s: &str) -> Option<&str> {
    strip_fixed_suffix(s, 11, is_dash_date_suffix)
}

fn strip_4digit_suffix(s: &str) -> Option<&str> {
    strip_fixed_suffix(s, 5, |suffix| {
        let bytes = suffix.as_bytes();
        bytes[0] == b'-' && bytes[1..5].iter().all(u8::is_ascii_digit)
    })
}

fn strip_version_suffix(s: &str) -> Option<&str> {
    let (prefix, rest) = s.rsplit_once("-v")?;
    if !rest.is_empty() && rest.as_bytes().iter().all(u8::is_ascii_digit) {
        Some(prefix)
    } else {
        None
    }
}

fn strip_compact_yyyymmdd(s: &str) -> Option<&str> {
    strip_fixed_suffix(s, 9, |suffix| {
        if !suffix.starts_with('-') {
            return false;
        }
        let digits = &suffix[1..];
        if digits.len() != 8 || !digits.as_bytes().iter().all(u8::is_ascii_digit) {
            return false;
        }
        let year = digits.get(..4).unwrap_or(digits);
        year.starts_with("19") || year.starts_with("20")
    })
}

fn normalize_family(s: &str) -> std::borrow::Cow<'_, str> {
    if s.starts_with("gemini-") && s.contains('_') {
        std::borrow::Cow::Owned(s.replace('_', "."))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
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

    #[test]
    fn test_strip_fixed_suffix() {
        assert_eq!(
            strip_fixed_suffix("hello-world", 6, |s| s == "-world"),
            Some("hello")
        );
        assert_eq!(
            strip_fixed_suffix("hello-world", 6, |s| s == "-other"),
            None
        );
        assert_eq!(strip_fixed_suffix("short", 10, |_| true), None);
    }

    #[test]
    fn test_candidate_normalized_forms() {
        let forms: Vec<&str> = candidate_normalized_forms("gpt-4o:free").collect();
        assert_eq!(forms, vec!["gpt-4o"]);

        let forms: Vec<&str> = candidate_normalized_forms("model-turbo:free").collect();
        assert_eq!(forms, vec!["model-turbo", "model"]);

        let forms: Vec<&str> =
            candidate_normalized_forms("claude-3-5-sonnet-preset-free-trial").collect();
        assert_eq!(forms, vec!["claude-3-5-sonnet-preset", "claude-3-5-sonnet"]);

        assert_eq!(candidate_normalized_forms("gpt-4o").count(), 0);
        assert_eq!(candidate_normalized_forms("").count(), 0);
        assert_eq!(candidate_normalized_forms(":free").count(), 0);
    }
}
