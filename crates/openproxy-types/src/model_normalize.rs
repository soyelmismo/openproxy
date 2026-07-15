use once_cell::sync::Lazy;
use regex::Regex;

static DATE_SUFFIX_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"-\d{4}-\d{2}-\d{2}$").unwrap());
static YYYYMM_SUFFIX_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"-\d{4}$").unwrap());
static VERSION_SUFFIX_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"-v\d+$").unwrap());

pub fn normalize_model_id(id: &str) -> String {
    let s: &str = id.rsplit('/').next().unwrap_or(id);
    let s: &str = s
        .trim_end_matches("-free-trial")
        .trim_end_matches("-free")
        .trim_end_matches(":free");

    let s = s.replace(":", "-");

    let s: String = DATE_SUFFIX_RE.replace_all(&s, "").into_owned();
    let s: String = strip_compact_yyyymmdd(&s).unwrap_or_else(|| s.clone());

    let s: String = YYYYMM_SUFFIX_RE.replace_all(&s, "").into_owned();

    let s: String = VERSION_SUFFIX_RE.replace_all(&s, "").into_owned();

    normalize_family(&s)
}

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

fn normalize_family(s: &str) -> String {
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
}
