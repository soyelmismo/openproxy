//! Translation and response extraction helpers for non-streaming upstream responses.

use openproxy_adapters::ProviderAdapter;
use openproxy_types::error::CoreError;
use openproxy_types::{OpenAIResponse, OpenAIUsage};

/// `OpenAIResponse` minimalista desde `choices[0].message.content` del body
/// crudo, para adapters que no producen OpenAI nativo.
pub(crate) fn translate_simple_text_response(
    response_body_raw: &serde_json::Value,
    model_name: String,
) -> OpenAIResponse {
    let text = response_body_raw
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|s| s.as_str())
        .or_else(|| response_body_raw.get("content").and_then(|s| s.as_str()))
        .unwrap_or("");

    let mut id = String::with_capacity(48);
    use std::fmt::Write;
    let _ = write!(&mut id, "chatcmpl_{}", uuid::Uuid::new_v4());

    OpenAIResponse {
        id,
        object: "chat.completion".to_string(),
        created: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        model: model_name,
        choices: vec![openproxy_types::OpenAIChoice {
            index: 0,
            message: openproxy_types::OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::Value::String(text.to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage: None,
    }
}

/// Overlay the usage the raw payload actually reported onto a translated
/// response.
///
/// `OpenAIUsage`' serde struct only knows the canonical field names, so a
/// dialect that reports `input_tokens`/`output_tokens` (or camelCase) would
/// deserialize them as `0` while the presence probe correctly reported them
/// present. This rebuilds the usage from the same extraction the presence
/// uses, so value and presence can never disagree.
fn apply_observed_openai_usage(resp: &mut OpenAIResponse, raw: &serde_json::Value) {
    let Some(usage) = raw.pointer("/usage") else {
        return;
    };
    let observed = openai_observed_usage_in(usage);
    if observed == ObservedUsage::default() {
        return;
    }
    let current = resp.usage.take();
    let prompt_tokens = observed
        .prompt
        .or_else(|| current.as_ref().map(|u| u.prompt_tokens))
        .unwrap_or(0);
    let completion_tokens = observed
        .completion
        .or_else(|| current.as_ref().map(|u| u.completion_tokens))
        .unwrap_or(0);
    let total_tokens = observed
        .total
        .or_else(|| current.as_ref().map(|u| u.total_tokens))
        .unwrap_or_else(|| prompt_tokens.saturating_add(completion_tokens));
    let prompt_tokens_details = observed
        .cached
        .map(|cached| openproxy_types::PromptTokensDetails {
            cached_tokens: Some(cached),
        })
        .or_else(|| current.and_then(|u| u.prompt_tokens_details));
    resp.usage = Some(openproxy_types::OpenAIUsage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
        prompt_tokens_details,
    });
}

/// Strict parse of an OpenAI-dialect body with `usage` excluded from the
/// serde contract.
///
/// The usage block is read once by [`openai_observed_usage_in`] *before* the
/// strict parse: a canonical counter that arrives as `null` means "absent"
/// (not "wrong type") and an alias is a value, so letting `OpenAIUsage`'
/// serde defaults see the block rejects the whole response before the alias
/// can be applied. Every other member keeps the exact `OpenAIResponse`
/// contract, so an invalid value outside `usage` still fails the parse.
fn deserialize_openai_response(
    response_body_raw: &serde_json::Value,
) -> Result<(OpenAIResponse, UsagePresence), CoreError> {
    use openproxy_types::ResultExt;

    #[derive(serde::Deserialize)]
    struct OpenAiResponseWithoutUsage {
        id: String,
        object: String,
        created: u64,
        model: String,
        choices: Vec<openproxy_types::OpenAIChoice>,
    }

    let usage_block = response_body_raw.get("usage");
    let observed = usage_block.map_or_else(ObservedUsage::default, openai_observed_usage_in);
    let presence = observed.presence();

    let body = <OpenAiResponseWithoutUsage as serde::Deserialize>::deserialize(response_body_raw)
        .ctx_parse("parse openai response")?;
    let usage = (observed != ObservedUsage::default()).then(|| OpenAIUsage {
        prompt_tokens: observed.prompt.unwrap_or(0),
        completion_tokens: observed.completion.unwrap_or(0),
        // A total the upstream never reported is not synthesized here: the
        // materialization step derives it from the fields that were observed.
        total_tokens: observed.total.unwrap_or(0),
        prompt_tokens_details: observed
            .cached
            .map(|cached| openproxy_types::PromptTokensDetails {
                cached_tokens: Some(cached),
            })
            .or_else(|| {
                usage_block
                    .filter(|usage| {
                        USAGE_DETAILS_KEYS
                            .iter()
                            .any(|key| usage.get(*key).is_some_and(serde_json::Value::is_object))
                    })
                    .map(|_| openproxy_types::PromptTokensDetails {
                        cached_tokens: None,
                    })
            }),
    });

    Ok((
        OpenAIResponse {
            id: body.id,
            object: body.object,
            created: body.created,
            model: body.model,
            choices: body.choices,
            usage,
        },
        presence,
    ))
}

/// Traduce el body según `target_format`. Gemini delega en su propia
/// traducción; Atomesus cae al fallback simple.
pub(crate) fn translate_non_streaming_body(
    target_format: openproxy_types::TargetFormat,
    response_body_raw: &serde_json::Value,
    req: &crate::PipelineRequest,
) -> Result<OpenAIResponse, CoreError> {
    use openproxy_types::ResultExt;
    match target_format {
        openproxy_types::TargetFormat::Responses => {
            crate::translation::responses_to_openai(response_body_raw, &req.openai_request.model)
        }
        openproxy_types::TargetFormat::Openai => {
            deserialize_openai_response(response_body_raw).map(|(response, _)| response)
        }
        openproxy_types::TargetFormat::Anthropic => {
            let anthropic_resp: crate::translation::AnthropicResponse =
                <crate::translation::AnthropicResponse as serde::Deserialize>::deserialize(
                    response_body_raw,
                )
                .ctx_parse("parse anthropic response")?;
            Ok(crate::translation::anthropic_to_openai(&anthropic_resp))
        }
        openproxy_types::TargetFormat::Gemini => {
            let adapter = openproxy_adapters::GeminiAdapter::new();
            adapter.translate_non_streaming_response(target_format, response_body_raw.clone())
        }
        openproxy_types::TargetFormat::Atomesus => Ok(translate_simple_text_response(
            response_body_raw,
            req.openai_request.model.clone(),
        )),
        openproxy_types::TargetFormat::CommandCodeGo => {
            if let Ok(mut resp) =
                <OpenAIResponse as serde::Deserialize>::deserialize(response_body_raw)
            {
                apply_observed_openai_usage(&mut resp, response_body_raw);
                Ok(resp)
            } else {
                let body_str = serde_json::to_string(response_body_raw).unwrap_or_default();
                crate::sse::parse_sse_stream_to_openai_response(
                    target_format,
                    &body_str,
                    &req.openai_request.model,
                )
            }
        }
        openproxy_types::TargetFormat::SystemOne => {
            let mut resp = <OpenAIResponse as serde::Deserialize>::deserialize(response_body_raw)
                .ctx_parse("parse systemone response")?;
            apply_observed_openai_usage(&mut resp, response_body_raw);
            Ok(resp)
        }
        // Unreachable in practice: Devin bodies are Connect-RPC protobuf and
        // `dispatch_upstream_non_streaming` folds them before JSON parse.
        openproxy_types::TargetFormat::Devin => {
            Err(CoreError::Parse("devin unary body is not JSON".to_string()))
        }
    }
}

/// Translate a unary body and return the per-field usage presence captured
/// from the raw upstream payload. Callers that know the target format must
/// use this variant instead of re-reading presence from a reserialized body.
pub(crate) fn translate_non_streaming_body_with_presence(
    target_format: openproxy_types::TargetFormat,
    response_body_raw: &serde_json::Value,
    req: &crate::PipelineRequest,
) -> Result<(OpenAIResponse, UsagePresence), CoreError> {
    if target_format == openproxy_types::TargetFormat::Openai {
        // Values and presence come from the same extraction, taken before the
        // strict parse of the fields that do not belong to `usage`.
        return deserialize_openai_response(response_body_raw);
    }
    let presence = upstream_usage_presence(target_format, response_body_raw);
    let response = translate_non_streaming_body(target_format, response_body_raw, req)?;
    Ok((response, presence))
}

/// Per-field presence of the upstream usage block. An explicit zero is a
/// valid observed value: it is only treated as "absent" when the field is
/// not present at all in the upstream payload.
///
/// This is parsed once next to the usage object and carried through
/// merge/record/fold. It is never reconstructed from a reserialized body.
pub(crate) use openproxy_types::UsagePresence;

/// Observed upstream usage before any client-only floor. `None` means the
/// field was absent upstream; `Some(0)` means the upstream explicitly
/// reported zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ObservedUsage {
    pub prompt: Option<u32>,
    pub completion: Option<u32>,
    pub total: Option<u32>,
    pub cached: Option<u32>,
}

impl ObservedUsage {
    /// Presence derived from the same extraction that produced the values, so
    /// the two can never disagree (an alias read as a value is also present).
    #[must_use]
    pub(crate) const fn presence(self) -> UsagePresence {
        UsagePresence {
            prompt: self.prompt.is_some(),
            completion: self.completion.is_some(),
            total: self.total.is_some(),
            cached: self.cached.is_some(),
        }
    }
}

/// Result of [`fill_usage`]: which fields were synthesized (never billed as
/// observed upstream consumption) and the observed snapshot the usage tracker
/// must persist.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct UsageFill {
    pub estimated: (bool, bool),
    pub observed: Option<ObservedUsage>,
}

/// First key in `keys` that carries a JSON integer. Keys are ordered by
/// canonical precedence (the dialect's own name first, aliases after), and a
/// value that is missing, `null` or not an integer is simply absent: it is
/// never coerced into a zero that would masquerade as an observation.
fn usage_u64(obj: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| obj.get(*key).and_then(serde_json::Value::as_u64))
}

fn usage_u32(obj: &serde_json::Value, keys: &[&str]) -> Option<u32> {
    usage_u64(obj, keys).map(|value| u32::try_from(value).unwrap_or(u32::MAX))
}

fn number_in(obj: &serde_json::Value, keys: &[&str]) -> bool {
    usage_u64(obj, keys).is_some()
}

const USAGE_DETAILS_KEYS: [&str; 4] = [
    "prompt_tokens_details",
    "prompt_token_details",
    "input_tokens_details",
    "input_token_details",
];
const USAGE_CACHED_KEYS: [&str; 5] = [
    "cached_tokens",
    "cached_prompt_tokens",
    "cache_read_input_tokens",
    "cache_read_tokens",
    "prompt_cache_hit_tokens",
];

fn cached_tokens_in(usage: &serde_json::Value) -> Option<u32> {
    USAGE_DETAILS_KEYS
        .iter()
        .find_map(|details_key| {
            usage
                .get(details_key)
                .and_then(|details| usage_u32(details, &USAGE_CACHED_KEYS))
        })
        .or_else(|| usage_u32(usage, &USAGE_CACHED_KEYS))
}

/// Single extraction point for OpenAI-shaped usage: the values and the
/// presence flags come from the same read, with the canonical names taking
/// precedence over aliases. `OpenAIUsage`' serde struct only knows the
/// canonical names, so callers that deserialize it must rebuild the usage
/// from this observation; otherwise `{input_tokens:17}` would be present but
/// zero.
pub(crate) fn openai_observed_usage_in(usage: &serde_json::Value) -> ObservedUsage {
    ObservedUsage {
        prompt: usage_u32(usage, &["prompt_tokens", "input_tokens"]),
        completion: usage_u32(usage, &["completion_tokens", "output_tokens"]),
        total: usage_u32(usage, &["total_tokens"]),
        cached: cached_tokens_in(usage),
    }
}

/// OpenAI-shaped usage presence from an already-extracted `usage` object.
pub(crate) fn openai_usage_presence_in(usage: &serde_json::Value) -> UsagePresence {
    openai_observed_usage_in(usage).presence()
}

/// OpenAI-shaped usage presence (`usage.prompt_tokens` /
/// `usage.completion_tokens` / `usage.total_tokens`), used by the folded SSE
/// accumulator and by native OpenAI responses.
pub(crate) fn openai_usage_presence(raw: &serde_json::Value) -> UsagePresence {
    raw.pointer("/usage")
        .map_or_else(UsagePresence::none, openai_usage_presence_in)
}

/// Command Code usage values: camelCase and snake_case aliases are both
/// accepted, camelCase first.
pub(crate) fn commandcode_observed_usage_in(usage: &serde_json::Value) -> ObservedUsage {
    ObservedUsage {
        prompt: usage_u32(
            usage,
            &[
                "promptTokens",
                "prompt_tokens",
                "inputTokens",
                "input_tokens",
            ],
        ),
        completion: usage_u32(
            usage,
            &[
                "completionTokens",
                "completion_tokens",
                "outputTokens",
                "output_tokens",
            ],
        ),
        total: usage_u32(usage, &["totalTokens", "total_tokens"]),
        cached: usage_u32(usage, &["cachedInputTokens", "cached_input_tokens"]).or_else(|| {
            usage
                .get("inputTokenDetails")
                .and_then(|d| usage_u32(d, &["cacheReadTokens", "cache_read_tokens"]))
        }),
    }
}

pub(crate) fn commandcode_usage_presence_in(usage: &serde_json::Value) -> UsagePresence {
    commandcode_observed_usage_in(usage).presence()
}

pub(crate) fn anthropic_observed_usage_in(usage: &serde_json::Value) -> ObservedUsage {
    ObservedUsage {
        prompt: usage_u32(
            usage,
            &[
                "input_tokens",
                "cache_creation_input_tokens",
                "cache_read_input_tokens",
            ],
        ),
        completion: usage_u32(usage, &["output_tokens"]),
        total: usage_u32(usage, &["total_tokens"]),
        cached: usage_u32(usage, &["cache_read_input_tokens"]),
    }
}

pub(crate) fn anthropic_usage_presence(raw: &serde_json::Value) -> UsagePresence {
    raw.pointer("/usage")
        .map_or_else(UsagePresence::none, |usage| {
            anthropic_observed_usage_in(usage).presence()
        })
}

pub(crate) fn anthropic_usage_presence_in(usage: &serde_json::Value) -> UsagePresence {
    anthropic_observed_usage_in(usage).presence()
}

fn responses_usage_value(raw: &serde_json::Value) -> Option<&serde_json::Value> {
    raw.get("usage")
        .or_else(|| raw.get("response").and_then(|r| r.get("usage")))
}

pub(crate) fn responses_observed_usage_in(usage: &serde_json::Value) -> ObservedUsage {
    ObservedUsage {
        prompt: usage_u32(usage, &["input_tokens", "prompt_tokens"]),
        completion: usage_u32(usage, &["output_tokens", "completion_tokens"]),
        total: usage_u32(usage, &["total_tokens"]),
        cached: cached_tokens_in(usage),
    }
}

pub(crate) fn responses_usage_presence(raw: &serde_json::Value) -> UsagePresence {
    responses_usage_value(raw).map_or_else(UsagePresence::none, |usage| {
        responses_observed_usage_in(usage).presence()
    })
}

fn gemini_usage_value(raw: &serde_json::Value) -> Option<&serde_json::Value> {
    raw.get("usageMetadata")
        .or_else(|| raw.get("usage_metadata"))
        .or_else(|| {
            raw.get("response").and_then(|response| {
                response
                    .get("usageMetadata")
                    .or_else(|| response.get("usage_metadata"))
            })
        })
}

pub(crate) fn gemini_usage_presence(raw: &serde_json::Value) -> UsagePresence {
    let Some(usage) = gemini_usage_value(raw) else {
        return UsagePresence::none();
    };
    UsagePresence {
        prompt: number_in(usage, &["promptTokenCount", "prompt_token_count"]),
        completion: number_in(usage, &["candidatesTokenCount", "candidates_token_count"]),
        total: number_in(usage, &["totalTokenCount", "total_token_count"]),
        cached: number_in(
            usage,
            &["cachedContentTokenCount", "cached_content_token_count"],
        ),
    }
}

/// Dialect-aware presence for unary upstream bodies. Unknown dialects fall
/// back to the OpenAI shape; when no usage block is found both fields are
/// absent and the caller estimates them.
pub(crate) fn upstream_usage_presence(
    target_format: openproxy_types::TargetFormat,
    raw: &serde_json::Value,
) -> UsagePresence {
    use openproxy_types::TargetFormat;
    match target_format {
        TargetFormat::Anthropic => anthropic_usage_presence(raw),
        TargetFormat::Responses => responses_usage_presence(raw),
        TargetFormat::Gemini => gemini_usage_presence(raw),
        _ => openai_usage_presence(raw),
    }
}

/// Client-wire-only discount for a dynamically injected system prefix.
///
/// It never raises the upstream value back up: if the heuristic prefix count
/// exceeds the observed prompt, the client sees zero rather than a clamped
/// floor. The recorded observation must be snapshotted before this runs.
pub(crate) fn discount_client_prefix(usage: &mut OpenAIUsage, prefix_tokens: u32) {
    usage.prompt_tokens = usage.prompt_tokens.saturating_sub(prefix_tokens);
    usage.total_tokens = usage.prompt_tokens.saturating_add(usage.completion_tokens);
}

/// Token count of a dynamically injected system prefix, measured with the
/// same estimator and model id the unary path uses for its discount.
pub(crate) fn injected_prefix_tokens(prefix: &str, model: Option<&str>) -> u32 {
    let prefix_msg = [openproxy_types::OpenAIMessage {
        role: "system".to_string(),
        content: Some(serde_json::Value::String(prefix.to_string())),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: Default::default(),
    }];
    openproxy_compression::token_estimate::estimate_prompt_tokens(&prefix_msg, model)
}

/// Materializes a coherent `total_tokens` on a client-facing copy without
/// claiming it was observed.
///
/// Folding frames never synthesizes a total: the merge keeps only real
/// observations. When the upstream never sent one but both components are
/// real, the wire total is derived here and `presence.total` stays `false`.
pub(crate) fn materialize_wire_total(usage: &mut OpenAIUsage, presence: UsagePresence) {
    if !presence.total && presence.prompt && presence.completion {
        usage.total_tokens = usage.prompt_tokens.saturating_add(usage.completion_tokens);
    }
}

fn estimate_completion_from_choices(resp: &OpenAIResponse, model: Option<&str>) -> u32 {
    let mut tokens = 0u32;
    for c in &resp.choices {
        if let Some(ref content) = c.message.content {
            if let Some(s) = content.as_str() {
                tokens +=
                    openproxy_compression::token_estimate::estimate_completion_tokens(s, model);
            } else if let Some(parts) = content.as_array() {
                for part in parts {
                    if let Some(s) = part.get("text").and_then(|t| t.as_str()) {
                        tokens += openproxy_compression::token_estimate::estimate_completion_tokens(
                            s, model,
                        );
                    }
                }
            }
        }
        if let Some(ref tcs) = c.message.tool_calls {
            for tc in tcs {
                if let Some(args) = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                {
                    tokens += openproxy_compression::token_estimate::estimate_completion_tokens(
                        args, model,
                    );
                }
            }
        }
        if let Some(serde_json::Value::String(reasoning)) = c.message.extra.get("reasoning_content")
        {
            tokens +=
                openproxy_compression::token_estimate::estimate_completion_tokens(reasoning, model);
        }
    }
    tokens
}

/// Fill usage fields the upstream did not report, preserving observed values
/// per field. An explicit upstream zero is a real observation and is kept
/// verbatim on the wire: only genuinely missing fields are estimated.
///
/// `model` must be the resolved target model id when available, never a combo
/// alias or `resp.model`.
///
/// `rail` gates the pinned template: only an identity OpenAI chat rail forwards
/// the prompt we render, so translated rails keep the clearly-estimated
/// heuristic instead of claiming another format's prompt.
pub(crate) fn ensure_usage_populated(
    resp: &mut OpenAIResponse,
    request: &openproxy_types::OpenAIRequest,
    model: Option<&str>,
    presence: UsagePresence,
    rail: openproxy_compression::token_estimate::PromptRail,
) -> UsageFill {
    let has_usage = resp.usage.is_some();
    let observed = resp.usage.as_ref().map(|u| ObservedUsage {
        prompt: presence.prompt.then_some(u.prompt_tokens),
        completion: presence.completion.then_some(u.completion_tokens),
        total: presence.total.then_some(u.total_tokens),
        cached: presence
            .cached
            .then(|| {
                u.prompt_tokens_details
                    .as_ref()
                    .and_then(|d| d.cached_tokens)
            })
            .flatten(),
    });
    let prompt_present = presence.prompt && has_usage;
    let completion_present = presence.completion && has_usage;

    let mut estimated = (false, false);
    let mut adjusted = false;
    let (mut prompt_tokens, mut completion_tokens) = resp
        .usage
        .as_ref()
        .map_or((0, 0), |u| (u.prompt_tokens, u.completion_tokens));

    if !prompt_present {
        prompt_tokens =
            openproxy_compression::token_estimate::estimate_request_prompt_tokens_for_rail(
                request, model, rail,
            )
            .max(1);
        estimated.0 = true;
        adjusted = true;
    }

    if !completion_present {
        completion_tokens = estimate_completion_from_choices(resp, model).max(1);
        estimated.1 = true;
        adjusted = true;
    }

    let total_tokens = if adjusted {
        prompt_tokens.saturating_add(completion_tokens)
    } else {
        observed
            .and_then(|o| o.total)
            .unwrap_or_else(|| prompt_tokens.saturating_add(completion_tokens))
    };
    // Any cached detail the upstream already declared survives untouched; an
    // observed cached counter overrides it.
    let prompt_tokens_details = observed
        .and_then(|o| o.cached)
        .map(|c| openproxy_types::PromptTokensDetails {
            cached_tokens: Some(c),
        })
        .or_else(|| {
            resp.usage
                .as_ref()
                .and_then(|u| u.prompt_tokens_details.clone())
        });
    resp.usage = Some(openproxy_types::OpenAIUsage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
        prompt_tokens_details,
    });

    UsageFill {
        estimated,
        observed,
    }
}
