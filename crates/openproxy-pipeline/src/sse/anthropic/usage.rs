//! Token usage merging for Anthropic SSE chunks.

use openproxy_types::UsagePresence;
use openproxy_types::message::{OpenAIUsage, PromptTokensDetails};

/// Merge one counter from two snapshots using the presence each snapshot
/// actually reported.
///
/// A value without presence is not an observation and is never carried
/// forward: that is what used to turn the 0 sentinel in `message_delta` into
/// a fabricated total. When both snapshots observed the field, the maximum
/// wins because SSE usage is incremental and a later sparse frame must not
/// lower an earlier count.
fn merge_observed(
    existing_value: u32,
    existing_present: bool,
    new_value: u32,
    new_present: bool,
) -> (u32, bool) {
    match (existing_present, new_present) {
        (true, true) => (existing_value.max(new_value), true),
        (true, false) => (existing_value, true),
        (false, true) => (new_value, true),
        (false, false) => (0, false),
    }
}

fn merge_cached(
    existing: Option<PromptTokensDetails>,
    existing_present: bool,
    new: Option<PromptTokensDetails>,
    new_present: bool,
) -> Option<PromptTokensDetails> {
    let pick = |a: Option<u32>, b: Option<u32>| match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    match (existing_present, new_present) {
        (true, true) => Some(PromptTokensDetails {
            cached_tokens: pick(
                existing.and_then(|d| d.cached_tokens),
                new.and_then(|d| d.cached_tokens),
            ),
        }),
        (true, false) => existing,
        (false, true) => new,
        (false, false) => existing.or(new),
    }
}

/// Merge two usage snapshots together with the per-field presence observed by
/// the SSE parser.
///
/// Every field keeps the maximum of the *observed* values; an explicit zero is
/// an observation and survives as `Some(0)`. A field that neither snapshot
/// reported stays at zero with `presence=false`, so callers can derive a wire
/// total later without ever marking it as measured.
pub(crate) fn merge_usage_with_presence(
    existing: OpenAIUsage,
    existing_presence: UsagePresence,
    new: OpenAIUsage,
    new_presence: UsagePresence,
) -> (OpenAIUsage, UsagePresence) {
    let (prompt_tokens, prompt) = merge_observed(
        existing.prompt_tokens,
        existing_presence.prompt,
        new.prompt_tokens,
        new_presence.prompt,
    );
    let (completion_tokens, completion) = merge_observed(
        existing.completion_tokens,
        existing_presence.completion,
        new.completion_tokens,
        new_presence.completion,
    );
    let (total_tokens, total) = merge_observed(
        existing.total_tokens,
        existing_presence.total,
        new.total_tokens,
        new_presence.total,
    );
    let prompt_tokens_details = merge_cached(
        existing.prompt_tokens_details,
        existing_presence.cached,
        new.prompt_tokens_details,
        new_presence.cached,
    );

    (
        OpenAIUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens,
            prompt_tokens_details,
        },
        UsagePresence {
            prompt,
            completion,
            total,
            cached: existing_presence.cached || new_presence.cached,
        },
    )
}

/// Merge two `OpenAIUsage` snapshots from successive SSE chunks.
///
/// Anthropic delivers token counts incrementally: `message_start` carries
/// `prompt_tokens` (with cache contributions) while `output_tokens` is ~0,
/// and `message_delta` carries the final `output_tokens`. The classic
/// `message_delta.usage` block omits `input_tokens` and sends a `0`
/// sentinel, so each field keeps the maximum seen so far rather than
/// overwriting. Losing the `message_start` prompt count would understate
/// usage for the whole request.
///
/// This variant has no presence information, so every field in both snapshots
/// is treated as an observation. Callers that parsed presence must use
/// [`merge_usage_with_presence`].
pub(crate) fn merge_usage(existing: OpenAIUsage, new: OpenAIUsage) -> OpenAIUsage {
    merge_usage_with_presence(existing, UsagePresence::all(), new, UsagePresence::all()).0
}
