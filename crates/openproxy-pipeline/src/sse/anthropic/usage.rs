//! Token usage merging for Anthropic SSE chunks.

use openproxy_types::message::{OpenAIUsage, PromptTokensDetails};

/// Merge two `OpenAIUsage` snapshots from successive SSE chunks.
///
/// Token counts in streamed responses are delivered incrementally:
/// `message_start` carries `prompt_tokens` (with cache contributions)
/// but `output_tokens` is still ~0; later `message_delta` carries the
/// final `output_tokens`. Newer Anthropic streams include
/// `input_tokens` in the `message_delta.usage` block too, but the
/// classic format omits it. This helper preserves the maximum seen
/// value per field so that a zero-sentinel chunk (e.g. message_delta
/// with `output_tokens: 89, prompt_tokens: 0`) does not clobber the
/// prompt count extracted from `message_start`.
pub(crate) fn merge_usage(existing: OpenAIUsage, new: OpenAIUsage) -> OpenAIUsage {
    use std::cmp::max;

    let pick_prompt = |new_val: u32| {
        if new_val > 0 {
            max(existing.prompt_tokens, new_val)
        } else {
            existing.prompt_tokens
        }
    };
    let pick_total = |new_val: u32| {
        if new_val > 0 {
            max(existing.total_tokens, new_val)
        } else if existing.total_tokens > 0 {
            existing.total_tokens
        } else {
            existing
                .prompt_tokens
                .saturating_add(existing.completion_tokens)
        }
    };

    OpenAIUsage {
        prompt_tokens: pick_prompt(new.prompt_tokens),
        completion_tokens: max(existing.completion_tokens, new.completion_tokens),
        total_tokens: pick_total(new.total_tokens),
        prompt_tokens_details: match (existing.prompt_tokens_details, new.prompt_tokens_details) {
            (Some(e), Some(n)) => Some(PromptTokensDetails {
                cached_tokens: match (e.cached_tokens, n.cached_tokens) {
                    (Some(a), Some(b)) => Some(max(a, b)),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                },
            }),
            (Some(e), None) => Some(e),
            (None, Some(n)) => Some(n),
            (None, None) => None,
        },
    }
}
