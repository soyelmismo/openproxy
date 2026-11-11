use super::util::{find_substring_match, to_lower_cow};

pub fn infer_context_length(model_id: &str) -> Option<i64> {
    const KNOWN: &[(&str, i64)] = &[
        ("claude-3", 200_000),
        ("claude-sonnet-4", 200_000),
        ("claude-opus-4", 200_000),
        ("claude-opus-3", 200_000),
        ("gemini-2.5-pro", 1_048_576),
        ("gemini-2.5-flash", 1_048_576),
        ("gemini-2", 1_048_576),
        ("gemini-1.5-pro", 2_097_152),
        ("gemini-1.5-flash", 1_048_576),
        ("gpt-4o", 128_000),
        ("gpt-4-turbo", 128_000),
        ("gpt-4-32k", 32_000),
        ("o1", 200_000),
        ("o3", 200_000),
        ("deepseek-chat", 64_000),
        ("deepseek-reasoner", 64_000),
        ("llama-3.1-405b", 131_072),
        ("llama-3.1-70b", 131_072),
        ("llama-3.1-8b", 131_072),
        ("llama-3.3-70b", 131_072),
        ("qwen2.5", 32_768),
        ("qwen-max", 32_768),
        ("mistral-large", 128_000),
    ];

    let lower = to_lower_cow(model_id);
    find_substring_match(&lower, KNOWN)
}

pub fn infer_max_output_tokens(model_id: &str) -> Option<i64> {
    const KNOWN: &[(&str, i64)] = &[
        ("claude-3", 8_192),
        ("claude-sonnet-4", 8_192),
        ("claude-opus-4", 32_000),
        ("gpt-4o", 16_384),
        ("o1", 100_000),
        ("o3", 100_000),
        ("gemini-2.5", 65_536),
        ("deepseek", 32_768),
    ];

    let lower = to_lower_cow(model_id);
    find_substring_match(&lower, KNOWN)
}
