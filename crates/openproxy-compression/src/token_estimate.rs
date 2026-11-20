//! BPE fallback accounting when the upstream does not report usage.
//!
//! Text is tokenized with a model-aware tokenizer registry: cl100k_base for
//! unknown families, o200k_base for OpenAI models, and the real public
//! tokenizer of each family (Anthropic npm claude.json, Kimi K2 tiktoken
//! vocab, Qwen3 / DeepSeek HF tokenizer.json) loaded from
//! `OPENPROXY_TOKENIZER_DIR`. This remains an estimate for chat/tool framing.
//! Upstream-reported usage is the authoritative count.
//! Media bytes are not text tokens; vision parts retain an explicit allowance.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;

use openproxy_types::message::OpenAIMessage;

/// Estimate tokens for a single `OpenAIMessage` without cloning `content` when
/// `tool_calls` is absent.
pub(crate) fn estimate_single_message_tokens(msg: &OpenAIMessage, model: Option<&str>) -> u32 {
    let mut total: u32 = 0;
    let has_tool_args = msg.tool_calls.as_ref().is_some_and(|calls| {
        calls.iter().any(|tc| {
            tc.get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(serde_json::Value::as_str)
                .is_some()
        })
    });
    if has_tool_args {
        let text = message_content_to_text(msg);
        if !text.is_empty() {
            total = total.saturating_add(count_tokens_for_model(model, &text));
        }
    } else {
        let cow = msg.extract_text_cow();
        if !cow.is_empty() {
            total = total.saturating_add(count_tokens_for_model(model, &cow));
        }
    }
    if let Some(name) = &msg.name {
        total = total.saturating_add(count_tokens_for_model(model, name));
    }
    // Reasoning in historical assistant turns is part of the submitted
    // context, not just visible content. Aliases must not count twice.
    if let Some(reasoning) = msg
        .extra
        .get("reasoning_content")
        .or_else(|| msg.extra.get("reasoning"))
        .and_then(serde_json::Value::as_str)
    {
        total = total.saturating_add(count_tokens_for_model(model, reasoning));
    }
    if let Some(calls) = &msg.tool_calls {
        for call in calls {
            total = total.saturating_add(4);
            if let Some(name) = call
                .pointer("/function/name")
                .and_then(serde_json::Value::as_str)
            {
                total = total.saturating_add(count_tokens_for_model(model, name));
            }
        }
    }

    // Add standard image vision token allowance (~800 tokens per image part)
    if let Some(serde_json::Value::Array(parts)) = &msg.content {
        for part in parts {
            let is_image = part
                .get("type")
                .and_then(|v| v.as_str())
                .is_some_and(|t| t == "image_url" || t == "input_image" || t == "image")
                || part.get("image_url").is_some();
            if is_image {
                total += 800;
            }
        }
    }

    // 4 tokens of role framing: <|im_start|>role\ncontent<|im_end|>\n.
    total + 4
}

/// Estimate prompt tokens from string and array-of-parts (Anthropic-style
/// `[{type:"text",text:"..."}]`) content across every message role, plus 4
/// tokens of framing overhead per message (`<|im_start|>role\n...<|im_end|>`).
/// Image parts (base64 or URLs) are billed standard vision tiles (~800 tokens)
/// instead of counting raw base64 characters.
pub fn estimate_prompt_tokens(messages: &[OpenAIMessage], model: Option<&str>) -> u32 {
    let mut total: u32 = 0;
    for msg in messages {
        total = total.saturating_add(estimate_single_message_tokens(msg, model));
    }
    total
}

/// Request-level fallback includes tool definitions and structured-output
/// schemas, which are prompt context even though they are outside messages.
/// Generation controls (temperature, max_tokens, model) are not prompt text.
/// `model_override` carries the resolved upstream model id when it differs
/// from `request.model` (e.g. combo names map to their real target model).
///
/// This entry point is the offline/reference estimate: it may render the
/// pinned Qwen template. Dispatcher call sites that know the resolved rail
/// must use [`estimate_request_prompt_tokens_for_rail`] so a template is only
/// used on a rail that sends the request body unchanged.
pub fn estimate_request_prompt_tokens(
    request: &openproxy_types::OpenAIRequest,
    model_override: Option<&str>,
) -> u32 {
    estimate_request_prompt_tokens_inner(request, model_override, PromptRail::OpenAiChat)
}

/// Whether the resolved upstream rail forwards the OpenAI chat body unchanged
/// (`OpenAiChat`) or rewrites it (`Translated`: Kiro, Anthropic, Gemini,
/// Responses, Devin, CommandCode, …).
///
/// The pinned Qwen3-Coder-Next template is only a faithful description of the
/// prompt when the upstream receives the roles/tools/tool_calls we render. On
/// translated rails it stays a reference number, never a claim of the real
/// upstream prompt, so the caller keeps the clearly-estimated heuristic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PromptRail {
    OpenAiChat,
    Translated,
}

pub fn estimate_request_prompt_tokens_for_rail(
    request: &openproxy_types::OpenAIRequest,
    model_override: Option<&str>,
    rail: PromptRail,
) -> u32 {
    estimate_request_prompt_tokens_inner(request, model_override, rail)
}

fn estimate_request_prompt_tokens_inner(
    request: &openproxy_types::OpenAIRequest,
    model_override: Option<&str>,
    rail: PromptRail,
) -> u32 {
    let model = model_override.or(Some(request.model.as_str()));

    // Qwen3-Coder-Next has a pinned, reproducible chat template. Counting the
    // rendered prompt once (roles, tool definitions, tool calls and the
    // generation prompt) is materially closer to the upstream prompt than
    // summing per-message heuristics. It is only used when:
    //   * the exact Qwen tokenizer is loaded,
    //   * the model id is the pinned canonical one (`not-qwen3-coder-next-*`
    //     must not match),
    //   * the rail forwards the OpenAI body unchanged, and
    //   * the template represents everything in the request
    //     (`functions`/`response_format` have no template slot; skipping them
    //     silently would undercount, so the heuristic below counts them).
    let template_cannot_represent_request = ["functions", "response_format"].iter().any(|key| {
        request
            .extra
            .get(*key)
            .is_some_and(|value| !value.is_null())
    });
    if rail == PromptRail::OpenAiChat
        && !template_cannot_represent_request
        && let Some(model) = model
        && crate::prompt_template::is_qwen3_coder_next(model)
        && has_family_tokenizer(model)
        && let Ok(rendered) = crate::prompt_template::render_qwen3_coder_next_prompt(
            &request.messages,
            request.tools.as_deref(),
        )
    {
        let tokens = count_tokens_for_model(Some(model), &rendered);
        if tokens > 0 {
            return tokens;
        }
    }

    let mut total = estimate_prompt_tokens(&request.messages, model);
    if let Some(tools) = request.tools.as_ref().filter(|tools| !tools.is_empty()) {
        total = total.saturating_add(count_tokens_for_model(
            model,
            &serde_json::to_string(tools).unwrap_or_default(),
        ));
    }
    for key in ["functions", "response_format"] {
        if let Some(schema) = request.extra.get(key).filter(|value| !value.is_null()) {
            total = total.saturating_add(count_tokens_for_model(model, &schema.to_string()));
        }
    }
    total
}

/// Cheap prompt-size hint for TTFT bucketing. It deliberately avoids the
/// tokenizer registry and any allocation: the latency predictor only needs
/// the logarithmic bucket (<1k, 1-4k, 4-16k, ...), never a billing-grade
/// count. Text is approximated as `ascii/4 + non_ascii` code points; media
/// parts keep the same explicit ~800-token allowance as the exact estimate.
///
/// Degenerate inputs (e.g. 20k repetitions of one character) can overestimate
/// by one bucket because BPE merges long runs; the error is conservative
/// (wider/slower bucket) and never affects reported usage.
pub fn estimate_prompt_tokens_bucket_hint(messages: &[OpenAIMessage]) -> u32 {
    let mut total: u32 = 0;
    for msg in messages {
        total = total.saturating_add(4);
        match &msg.content {
            Some(serde_json::Value::String(text)) => {
                total = total.saturating_add(approx_text_tokens(text));
            }
            Some(serde_json::Value::Array(parts)) => {
                for part in parts {
                    let is_image = part
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|t| {
                            matches!(
                                t,
                                "image_url" | "input_image" | "image" | "input_audio" | "audio"
                            )
                        })
                        || part.get("image_url").is_some();
                    if is_image {
                        total = total.saturating_add(800);
                    } else if let Some(text) = part.get("text").and_then(serde_json::Value::as_str)
                    {
                        total = total.saturating_add(approx_text_tokens(text));
                    } else if let Some(text) =
                        part.get("content").and_then(serde_json::Value::as_str)
                    {
                        total = total.saturating_add(approx_text_tokens(text));
                    }
                }
            }
            _ => {}
        }
        if let Some(name) = &msg.name {
            total = total.saturating_add(approx_text_tokens(name));
        }
        if let Some(reasoning) = msg
            .extra
            .get("reasoning_content")
            .or_else(|| msg.extra.get("reasoning"))
            .and_then(serde_json::Value::as_str)
        {
            total = total.saturating_add(approx_text_tokens(reasoning));
        }
        if let Some(calls) = &msg.tool_calls {
            for call in calls {
                total = total.saturating_add(4);
                if let Some(name) = call
                    .pointer("/function/name")
                    .and_then(serde_json::Value::as_str)
                {
                    total = total.saturating_add(approx_text_tokens(name));
                }
                if let Some(args) = call
                    .pointer("/function/arguments")
                    .and_then(serde_json::Value::as_str)
                {
                    total = total.saturating_add(approx_text_tokens(args));
                }
            }
        }
    }
    total
}

/// Allocation-free token approximation used only for latency buckets.
fn approx_text_tokens(text: &str) -> u32 {
    let mut ascii: usize = 0;
    let mut non_ascii: usize = 0;
    for byte in text.bytes() {
        if byte < 0x80 {
            ascii += 1;
        } else if byte & 0xC0 != 0x80 {
            non_ascii += 1;
        }
    }
    u32::try_from(ascii.div_ceil(4) + non_ascii).unwrap_or(u32::MAX)
}

/// Estimate completion tokens from a text string.
pub fn estimate_completion_tokens(text: &str, model: Option<&str>) -> u32 {
    if text.is_empty() {
        return 0;
    }
    count_tokens_for_model(model, text)
}

/// Estimate completion tokens from a response body JSON value: OpenAI
/// `choices[].message` / `choices[].delta` (`content`, `reasoning_content` /
/// `reasoning`, `tool_calls`), Anthropic `content` blocks (`text`, `thinking`,
/// `tool_use`), Gemini `candidates[].content.parts` (`text`, `functionCall`),
/// or a bare string.
pub fn estimate_completion_tokens_from_body(body: &serde_json::Value, model: Option<&str>) -> u32 {
    let mut total_tokens = 0u32;
    let mut found_structured = false;

    // 1. OpenAI shape: choices[].message or choices[].delta
    if let Some(choices) = body.get("choices").and_then(|c| c.as_array()) {
        for choice in choices {
            let msg = choice.get("message").or_else(|| choice.get("delta"));
            if let Some(msg) = msg {
                found_structured = true;
                if let Some(content) = msg.get("content") {
                    if let Some(s) = content.as_str()
                        && !s.is_empty()
                    {
                        total_tokens += count_tokens_for_model(model, s);
                    } else if let Some(parts) = content.as_array() {
                        for part in parts {
                            if let Some(s) = part.get("text").and_then(|t| t.as_str())
                                && !s.is_empty()
                            {
                                total_tokens += count_tokens_for_model(model, s);
                            }
                        }
                    }
                }

                let reasoning = msg
                    .get("reasoning_content")
                    .or_else(|| msg.get("reasoning"))
                    .and_then(|r| r.as_str());
                if let Some(r) = reasoning
                    && !r.is_empty()
                {
                    total_tokens += count_tokens_for_model(model, r);
                }

                if let Some(tool_calls) = msg.get("tool_calls").and_then(|tc| tc.as_array()) {
                    for tc in tool_calls {
                        // 4 tokens of framing overhead per tool call
                        total_tokens += 4;
                        if let Some(func) = tc.get("function") {
                            if let Some(name) = func.get("name").and_then(|n| n.as_str()) {
                                total_tokens += count_tokens_for_model(model, name);
                            }
                            if let Some(args) = func.get("arguments") {
                                if let Some(s) = args.as_str() {
                                    total_tokens += count_tokens_for_model(model, s);
                                } else if !args.is_null() {
                                    total_tokens +=
                                        count_tokens_for_model(model, &args.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Anthropic shape: content: [ { type: "text", text: ... }, { type: "tool_use", ... } ]
    if let Some(content) = body.get("content").and_then(|c| c.as_array()) {
        for block in content {
            found_structured = true;
            let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match block_type {
                "text" => {
                    if let Some(s) = block.get("text").and_then(|t| t.as_str())
                        && !s.is_empty()
                    {
                        total_tokens += count_tokens_for_model(model, s);
                    }
                }
                "thinking" => {
                    if let Some(s) = block.get("thinking").and_then(|t| t.as_str())
                        && !s.is_empty()
                    {
                        total_tokens += count_tokens_for_model(model, s);
                    }
                }
                "tool_use" => {
                    total_tokens += 4;
                    if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                        total_tokens += count_tokens_for_model(model, name);
                    }
                    if let Some(input) = block.get("input") {
                        if let Some(s) = input.as_str() {
                            total_tokens += count_tokens_for_model(model, s);
                        } else if !input.is_null() {
                            total_tokens += count_tokens_for_model(model, &input.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Gemini shape: candidates[].content.parts[]
    if let Some(candidates) = body.get("candidates").and_then(|c| c.as_array()) {
        for cand in candidates {
            if let Some(parts) = cand.pointer("/content/parts").and_then(|p| p.as_array()) {
                found_structured = true;
                for part in parts {
                    if let Some(text) = part.get("text").and_then(|t| t.as_str())
                        && !text.is_empty()
                    {
                        total_tokens += count_tokens_for_model(model, text);
                    }
                    if let Some(call) = part.get("functionCall") {
                        total_tokens += 4;
                        if let Some(name) = call.get("name").and_then(|n| n.as_str()) {
                            total_tokens += count_tokens_for_model(model, name);
                        }
                        if let Some(args) = call.get("args") {
                            total_tokens += count_tokens_for_model(model, &args.to_string());
                        }
                    }
                }
            }
        }
    }

    if !found_structured && let Some(text) = body.as_str() {
        total_tokens = count_tokens_for_model(model, text);
    }

    total_tokens
}

/// Count ordinary text with cl100k_base — the fallback for families without
/// a pinned, verified public tokenizer (Gemini, unknown models, and versions
/// of a known family that were never verified against the pinned artifact).
fn count_tokens_cl100k(text: &str) -> u32 {
    u32::try_from(
        tiktoken_rs::cl100k_base_singleton()
            .encode_ordinary(text)
            .len(),
    )
    .unwrap_or(u32::MAX)
}

/// Concatenated text of an `OpenAIMessage`: `extract_text()` plus the
/// `tool_calls[].function.arguments` JSON string when present.
pub fn message_content_to_text(msg: &OpenAIMessage) -> String {
    let mut text = msg.extract_text();

    if let Some(ref tool_calls) = msg.tool_calls {
        for tc in tool_calls {
            if let Some(args) = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
            {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(args);
            }
        }
    }

    text
}

// ===== Model-aware tokenizer registry =====

/// Public tokenizer pinned for one model family/version.
///
/// The registry only assigns an artifact to the exact version that was
/// verified against the published Hugging Face revision (see
/// `assets/tokenizers/manifest.json`). Older or unverified versions of the
/// same family deliberately fall back to cl100k instead of borrowing a
/// versioned vocabulary they were never checked against.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModelFamily {
    /// Anthropic's legacy npm tokenizer. Clearly a heuristic: it does not
    /// prove fidelity to modern Claude models.
    Claude,
    Kimi,
    Qwen3,
    DeepSeekV32,
    Glm53,
    MiniMaxM3,
    /// OpenAI models whose published vocabulary is o200k_base.
    OpenAiO200k,
    /// Legacy OpenAI models (gpt-4, gpt-3.5) whose vocabulary is cl100k_base.
    OpenAiCl100k,
    Fallback,
}

/// Pick the segment that names the model when an id carries a routing prefix
/// (`openai:gpt-5`) or a provider suffix (`qwen3-coder-next:free`). The last
/// segment carrying a known family marker wins; otherwise keep the last
/// segment, which preserves the legacy behaviour for unknown ids.
pub(crate) fn model_segment(segment: &str) -> &str {
    segment
        .split(':')
        .rev()
        .find(|part| {
            part.contains("claude")
                || part.contains("kimi")
                || part.contains("moonshot")
                || part.contains("qwen")
                || part.contains("deepseek")
                || part.contains("glm")
                || part.contains("minimax")
                || part.contains("gpt")
                || part.contains("openai")
                || part.contains("chatgpt")
                || part.starts_with("o1")
                || part.starts_with("o3")
                || part.starts_with("o4")
                || part.starts_with("o5")
        })
        .unwrap_or_else(|| segment.rsplit(':').next().unwrap_or(segment))
}

/// Family gate: the segment must start with the family token and either end
/// there or continue with a version/attribute separator (`qwen3-32b`,
/// `glm-5.3`, `minimax-m3`). A look-alike such as `notqwen…` never matches
/// because the remainder starts with a letter.
pub(crate) fn starts_with_family(segment: &str, family: &str) -> bool {
    segment.strip_prefix(family).is_some_and(|rest| {
        rest.is_empty()
            || rest
                .as_bytes()
                .first()
                .is_none_or(|byte| !byte.is_ascii_alphabetic())
    })
}

/// Attribute suffixes that keep an otherwise exact id inside its verified
/// family (quantization/deployment markers a hub repo adds to the same
/// vocabulary). Anything else falls back.
const VARIANT_SUFFIXES: [&str; 14] = [
    "instruct", "base", "chat", "it", "preview", "latest", "free", "beta", "fp8", "fp16", "bf16",
    "int4", "int8", "awq",
];

/// True when `segment` is exactly one of the canonical ids, or carries a
/// recognized variant suffix (`qwen3-coder-next-fp8`). Rejects substring
/// look-alikes such as `not-qwen3-coder-next-unknown`, which must never
/// borrow a versioned vocabulary.
pub(crate) fn starts_with_canonical(segment: &str, canon: &[&str]) -> bool {
    canon.iter().any(|token| {
        segment.strip_prefix(token).is_some_and(|rest| {
            if rest.is_empty() {
                return true;
            }
            let Some(suffix) = rest
                .strip_prefix('-')
                .or_else(|| rest.strip_prefix('_'))
                .or_else(|| rest.strip_prefix('.'))
            else {
                return false;
            };
            let head = suffix
                .split(['-', '_', '.'])
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            VARIANT_SUFFIXES.contains(&head.as_str())
        })
    })
}

/// Canonical family/version dispatch. `model` may carry a provider prefix
/// (`kiro/deepseek-3.2`) or a routing prefix (`openai:gpt-5`); only the last
/// path/colon segment is inspected. Matching is by exact canonical id (plus a
/// non-alphanumeric attribute suffix), never by bare substring: unknown ids
/// must not inherit a versioned vocabulary by family name alone.
pub fn family_for_model(model: &str) -> ModelFamily {
    let lower = model.to_ascii_lowercase();
    let path_segment = lower.rsplit('/').next().unwrap_or(&lower);
    let last = model_segment(path_segment);

    if starts_with_family(last, "claude") {
        ModelFamily::Claude
    } else if starts_with_family(last, "kimi") || starts_with_family(last, "moonshot") {
        // Only the revision whose vocabulary/pattern was verified against the
        // pinned Kimi-K2.7-Code artifact. Other Kimi versions fall back.
        if starts_with_canonical(
            last,
            &["kimi-k2.7-code", "kimi-k2-7-code", "kimi-k2_7-code"],
        ) {
            ModelFamily::Kimi
        } else {
            ModelFamily::Fallback
        }
    } else if starts_with_family(last, "qwen") {
        // Qwen3-32B and Qwen3-Coder-Next share the pinned vocabulary and
        // merges; other Qwen3 variants were not verified against it.
        if starts_with_canonical(last, &["qwen3-coder-next", "qwen3-coder_next"])
            || last == "qwen3-32b"
            || last == "qwen3_32b"
        {
            ModelFamily::Qwen3
        } else {
            ModelFamily::Fallback
        }
    } else if starts_with_family(last, "deepseek") {
        // `deepseek-3.2` is the documented Kiro integration alias for
        // DeepSeek-V3.2 (wiki: kiro/deepseek-3.2 was verified E2E).
        if starts_with_canonical(
            last,
            &[
                "deepseek-v3.2",
                "deepseek-v3-2",
                "deepseek-v3_2",
                "deepseek-v32",
                "deepseek-3.2",
                "deepseek3.2",
            ],
        ) {
            ModelFamily::DeepSeekV32
        } else {
            ModelFamily::Fallback
        }
    } else if starts_with_family(last, "glm") {
        if starts_with_canonical(last, &["glm-5.3", "glm5.3", "glm_5_3", "glm-5-3"]) {
            ModelFamily::Glm53
        } else {
            ModelFamily::Fallback
        }
    } else if starts_with_family(last, "minimax") {
        if starts_with_canonical(last, &["minimax-m3", "minimax_m3"]) {
            ModelFamily::MiniMaxM3
        } else {
            ModelFamily::Fallback
        }
    } else if is_openai_model(last) {
        openai_family(last)
    } else {
        ModelFamily::Fallback
    }
}

fn is_openai_model(last: &str) -> bool {
    last.contains("gpt")
        || last.contains("openai")
        || last.contains("chatgpt")
        || tiktoken_rs::tokenizer::get_tokenizer(last).is_some()
        || last.starts_with("o1")
        || last.starts_with("o3")
        || last.starts_with("o4")
        // An `o5-*` id is still an OpenAI model (so it is not treated as a
        // foreign family), but its vocabulary is not verified: `openai_family`
        // decides that from the tokenizer crate's map, not from this prefix.
        || last.starts_with("o5")
        || last.starts_with("text-embedding-")
        || last.starts_with("davinci")
        || last.starts_with("babbage")
}

/// OpenAI's published vocabularies, taken from `tiktoken_rs`' verified model
/// map rather than from local prefix guessing: a family only gets o200k when
/// the tokenizer crate documents that vocabulary for the id. Unknown ids
/// (`o5-unknown`, a future `gpt-6`) keep the conservative cl100k estimate
/// instead of claiming a vocabulary no source lists.
fn openai_family(last: &str) -> ModelFamily {
    match tiktoken_rs::tokenizer::get_tokenizer(last) {
        Some(tiktoken_rs::tokenizer::Tokenizer::O200kBase) => ModelFamily::OpenAiO200k,
        // `gpt-oss-*` uses the harmony encoding, which this crate does not
        // expose as a base BPE; estimating with cl100k is the honest fallback.
        Some(_) => ModelFamily::OpenAiCl100k,
        None => ModelFamily::OpenAiCl100k,
    }
}

/// A tiktoken-style BPE vocab (bytes -> rank) with its pre-tokenizer pattern.
struct TiktokenCustom {
    ranks: HashMap<Vec<u8>, u32>,
    split: fancy_regex::Regex,
}

fn tokenizer_dir() -> std::path::PathBuf {
    std::env::var("OPENPROXY_TOKENIZER_DIR").map_or_else(
        |_| std::path::PathBuf::from("/root/.openproxy/tokenizers"),
        std::path::PathBuf::from,
    )
}

/// Pinned artifacts are optional in normal operation (missing files fall back
/// to cl100k) but must never be silently accepted when the operator asks for
/// strict accounting: `OPENPROXY_TOKENIZER_STRICT=1` turns every missing or
/// hash-mismatched artifact into an error log and `validate_tokenizer_assets`
/// into a hard failure.
fn strict_assets() -> bool {
    std::env::var("OPENPROXY_TOKENIZER_STRICT")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
}

#[derive(Debug, serde::Deserialize)]
struct Manifest {
    artifacts: Vec<ManifestArtifact>,
    #[serde(default)]
    templates: Vec<ManifestTemplate>,
}

#[derive(Debug, serde::Deserialize)]
struct ManifestTemplate {
    id: String,
    embedded: bool,
    sha256: String,
    #[serde(default)]
    bytes: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
struct ManifestArtifact {
    id: String,
    kind: String,
    filename: String,
    #[serde(default)]
    legacy_filenames: Vec<String>,
    url: String,
    revision: String,
    sha256: String,
    #[serde(default)]
    bytes: Option<u64>,
}

static MANIFEST: OnceLock<Option<Manifest>> = OnceLock::new();

fn manifest() -> Option<&'static Manifest> {
    MANIFEST
        .get_or_init(|| {
            match serde_json::from_str::<Manifest>(include_str!(
                "../assets/tokenizers/manifest.json"
            )) {
                Ok(manifest) => Some(manifest),
                Err(error) => {
                    tracing::error!(%error, "tokenizer manifest is invalid");
                    None
                }
            }
        })
        .as_ref()
}

fn artifact(id: &str) -> Option<&'static ManifestArtifact> {
    manifest()?.artifacts.iter().find(|a| a.id == id)
}

/// Resolve the artifact path, preferring the pinned versioned filename and
/// only then a legacy compatibility name. Returns whether the file is the
/// pinned versioned artifact (and therefore must match the manifest hash).
fn resolve_artifact_path(
    dir: &Path,
    artifact: &ManifestArtifact,
) -> Option<(std::path::PathBuf, bool)> {
    let versioned = dir.join(&artifact.filename);
    if versioned.is_file() {
        return Some((versioned, true));
    }
    for legacy in &artifact.legacy_filenames {
        let path = dir.join(legacy);
        if path.is_file() {
            return Some((path, false));
        }
    }
    None
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn note_missing_artifact(artifact: &ManifestArtifact, dir: &Path) {
    if strict_assets() {
        tracing::error!(
            artifact = %artifact.id,
            dir = %dir.display(),
            filename = %artifact.filename,
            "required tokenizer artifact is missing; strict accounting is enabled"
        );
    } else {
        tracing::warn!(
            artifact = %artifact.id,
            dir = %dir.display(),
            filename = %artifact.filename,
            "tokenizer artifact unavailable; using cl100k fallback"
        );
    }
}

/// Load a manifest artifact and verify its sha256 when it is the pinned
/// versioned file. Legacy compatibility names are accepted without a hash
/// (the manifest does not pin them) but are logged as such.
fn load_artifact_bytes(artifact: &ManifestArtifact) -> Option<Vec<u8>> {
    let dir = tokenizer_dir();
    let Some((path, versioned)) = resolve_artifact_path(&dir, artifact) else {
        note_missing_artifact(artifact, &dir);
        return None;
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "failed to read tokenizer artifact");
            return None;
        }
    };
    if versioned {
        let actual = sha256_hex(&bytes);
        if actual != artifact.sha256 {
            if strict_assets() {
                tracing::error!(
                    artifact = %artifact.id,
                    path = %path.display(),
                    expected = %artifact.sha256,
                    actual = %actual,
                    "tokenizer artifact sha256 mismatch; strict accounting is enabled"
                );
            } else {
                tracing::warn!(
                    artifact = %artifact.id,
                    path = %path.display(),
                    expected = %artifact.sha256,
                    actual = %actual,
                    "tokenizer artifact sha256 mismatch; using cl100k fallback"
                );
            }
            return None;
        }
        if let Some(expected) = artifact.bytes
            && bytes.len() as u64 != expected
        {
            tracing::warn!(
                artifact = %artifact.id,
                path = %path.display(),
                expected,
                actual = bytes.len(),
                "tokenizer artifact size differs from the manifest"
            );
        }
    } else {
        tracing::warn!(
            artifact = %artifact.id,
            path = %path.display(),
            "legacy tokenizer filename used; sha256 is not pinned for this name"
        );
    }
    Some(bytes)
}

/// Validate every pinned artifact under `dir` (used by tests and operators).
/// Legacy filenames satisfy presence but not hash verification; a missing or
/// corrupt versioned artifact is always reported, regardless of
/// `OPENPROXY_TOKENIZER_STRICT`.
pub fn validate_tokenizer_assets(dir: &Path) -> Result<(), Vec<String>> {
    let Some(manifest) = manifest() else {
        return Err(vec!["tokenizer manifest is invalid".to_string()]);
    };
    let mut problems = Vec::new();
    for artifact in &manifest.artifacts {
        if artifact.revision.len() != 40 || !artifact.url.contains(&artifact.revision) {
            problems.push(format!(
                "artifact '{}' does not pin a 40-char revision in its URL",
                artifact.id
            ));
        }
        let Some((path, versioned)) = resolve_artifact_path(dir, artifact) else {
            problems.push(format!(
                "missing tokenizer artifact '{}' ({})",
                artifact.id, artifact.filename
            ));
            continue;
        };
        if !versioned {
            continue;
        }
        match std::fs::read(&path) {
            Ok(bytes) => {
                let actual = sha256_hex(&bytes);
                if actual != artifact.sha256 {
                    problems.push(format!(
                        "sha256 mismatch for '{}' at {}: expected {}, got {}",
                        artifact.id,
                        path.display(),
                        artifact.sha256,
                        actual
                    ));
                }
                if let Some(expected) = artifact.bytes
                    && bytes.len() as u64 != expected
                {
                    problems.push(format!(
                        "size mismatch for '{}' at {}: expected {}, got {}",
                        artifact.id,
                        path.display(),
                        expected,
                        bytes.len()
                    ));
                }
            }
            Err(error) => problems.push(format!(
                "cannot read tokenizer artifact '{}' at {}: {error}",
                artifact.id,
                path.display()
            )),
        }
    }
    for template in &manifest.templates {
        if !template.embedded {
            continue;
        }
        let actual = sha256_hex(crate::prompt_template::QWEN3_CODER_NEXT_TEMPLATE.as_bytes());
        if actual != crate::prompt_template::QWEN3_CODER_NEXT_TEMPLATE_SHA256 {
            problems.push(format!(
                "embedded template '{}' does not match its compiled sha256 constant",
                template.id
            ));
        }
        if actual != template.sha256 {
            problems.push(format!(
                "embedded template '{}' sha256 mismatch: expected {}, got {}",
                template.id, template.sha256, actual
            ));
        }
        if let Some(expected) = template.bytes
            && crate::prompt_template::QWEN3_CODER_NEXT_TEMPLATE.len() as u64 != expected
        {
            problems.push(format!(
                "embedded template '{}' size mismatch: expected {}, got {}",
                template.id,
                expected,
                crate::prompt_template::QWEN3_CODER_NEXT_TEMPLATE.len()
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

static CLAUDE_TOK: OnceLock<Option<Arc<TiktokenCustom>>> = OnceLock::new();
static KIMI_TOK: OnceLock<Option<Arc<TiktokenCustom>>> = OnceLock::new();
static QWEN_TOK: OnceLock<Option<Arc<tokenizers::Tokenizer>>> = OnceLock::new();
static DEEPSEEK_V32_TOK: OnceLock<Option<Arc<tokenizers::Tokenizer>>> = OnceLock::new();
static GLM_TOK: OnceLock<Option<Arc<tokenizers::Tokenizer>>> = OnceLock::new();
static MINIMAX_TOK: OnceLock<Option<Arc<tokenizers::Tokenizer>>> = OnceLock::new();
static O200K: OnceLock<tiktoken_rs::CoreBPE> = OnceLock::new();

/// Anthropic's official npm tokenizer file (`@anthropic-ai/tokenizer`
/// claude.json). Format: `"<literal_token> <reserved_rank> <b64> <b64> ..."`
/// where the first entry is a literal byte token occupying `reserved_rank`,
/// and the remaining base64 tokens take sequential ranks that skip it.
/// The wrapper normalizes input with NFKC before encoding.
fn load_claude_ranks(path: &Path) -> Option<TiktokenCustom> {
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let pat_str = v.get("pat_str")?.as_str()?.to_string();
    let items: Vec<&str> = v.get("bpe_ranks")?.as_str()?.split(' ').collect();
    if items.len() < 3 {
        return None;
    }
    let literal = items[0].as_bytes().to_vec();
    let reserved: usize = items[1].parse().ok()?;
    let mut ranks: HashMap<Vec<u8>, u32> = HashMap::with_capacity(items.len() * 2);
    for (i, item) in items[2..].iter().enumerate() {
        let bytes = B64.decode(item).ok()?;
        let rank = if i < reserved { i as u32 } else { i as u32 + 1 };
        ranks.insert(bytes, rank);
    }
    ranks.insert(literal, reserved as u32);
    let split = fancy_regex::Regex::new(&pat_str).ok()?;
    Some(TiktokenCustom { ranks, split })
}

/// Standard tiktoken vocab file: one `"<b64> <rank>"` pair per line
/// (Moonshot Kimi-K2 `tiktoken.model`), paired with Kimi's own pre-tokenizer
/// pattern from `tokenization_kimi.py` (Han-aware, with `&&` class
/// intersections that fancy_regex supports).
const KIMI_PAT_STR: &str = r"[\p{Han}]+|[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]*[\p{Ll}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]+(?i:'s|'t|'re|'ve|'m|'ll|'d)?|[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]+[\p{Ll}\p{Lm}\p{Lo}\p{M}&&[^\p{Han}]]*(?i:'s|'t|'re|'ve|'m|'ll|'d)?|\p{N}{1,3}| ?[^\s\p{L}\p{N}]+[\r\n]*|\s*[\r\n]+|\s+(?!\S)|\s+";

fn load_kimi_ranks_bytes(raw: &[u8]) -> Option<TiktokenCustom> {
    let raw = std::str::from_utf8(raw).ok()?;
    let mut ranks: HashMap<Vec<u8>, u32> = HashMap::with_capacity(raw.len() / 12);
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split(' ');
        let tok = it.next()?;
        let rank: u32 = it.next()?.parse().ok()?;
        ranks.insert(B64.decode(tok).ok()?, rank);
    }
    let split = fancy_regex::Regex::new(KIMI_PAT_STR).ok()?;
    Some(TiktokenCustom { ranks, split })
}

fn custom(
    cell: &'static OnceLock<Option<Arc<TiktokenCustom>>>,
    id: &'static str,
    load: fn(&[u8]) -> Option<TiktokenCustom>,
) -> Option<Arc<TiktokenCustom>> {
    cell.get_or_init(|| {
        let artifact = artifact(id)?;
        if artifact.kind != "tiktoken_ranks" {
            tracing::warn!(artifact = %artifact.id, kind = %artifact.kind, "unexpected artifact kind for custom tokenizer");
            return None;
        }
        let bytes = load_artifact_bytes(artifact)?;
        match load(&bytes) {
            Some(t) => Some(Arc::new(t)),
            None => {
                tracing::warn!(artifact = %artifact.id, "failed to parse custom tokenizer");
                None
            }
        }
    })
    .clone()
}

fn hf(
    cell: &'static OnceLock<Option<Arc<tokenizers::Tokenizer>>>,
    id: &'static str,
) -> Option<Arc<tokenizers::Tokenizer>> {
    cell.get_or_init(|| {
        let artifact = artifact(id)?;
        if artifact.kind != "hf_tokenizer_json" {
            tracing::warn!(artifact = %artifact.id, kind = %artifact.kind, "unexpected artifact kind for HF tokenizer");
            return None;
        }
        let bytes = load_artifact_bytes(artifact)?;
        match tokenizers::Tokenizer::from_bytes(&bytes) {
            Ok(t) => Some(Arc::new(t)),
            Err(error) => {
                tracing::warn!(artifact = %artifact.id, %error, "failed to parse HF tokenizer");
                None
            }
        }
    })
    .clone()
}

fn claude_custom() -> Option<Arc<TiktokenCustom>> {
    CLAUDE_TOK
        .get_or_init(|| {
            let path = tokenizer_dir().join("claude.json");
            match load_claude_ranks(&path) {
                Some(t) => {
                    tracing::debug!(
                        path = %path.display(),
                        "loaded Anthropic npm tokenizer as a labelled legacy heuristic"
                    );
                    Some(Arc::new(t))
                }
                None => {
                    tracing::warn!(
                        path = %path.display(),
                        "Anthropic npm tokenizer unavailable; using cl100k fallback"
                    );
                    None
                }
            }
        })
        .clone()
}

fn o200k() -> &'static tiktoken_rs::CoreBPE {
    O200K.get_or_init(|| match tiktoken_rs::o200k_base() {
        Ok(vocab) => vocab,
        Err(err) => panic!("built-in o200k_base vocab must load: {err}"),
    })
}

/// True when the family's pinned tokenizer is actually loaded. Used by the
/// prompt-template path: a rendered prompt is only counted with the exact
/// family tokenizer, never silently with cl100k.
pub fn has_family_tokenizer(model: &str) -> bool {
    match family_for_model(model) {
        ModelFamily::Claude => claude_custom().is_some(),
        ModelFamily::Kimi => custom(&KIMI_TOK, "kimi-k2.7-code", load_kimi_ranks_bytes).is_some(),
        ModelFamily::Qwen3 => hf(&QWEN_TOK, "qwen3-coder-next").is_some(),
        ModelFamily::DeepSeekV32 => hf(&DEEPSEEK_V32_TOK, "deepseek-v3.2").is_some(),
        ModelFamily::Glm53 => hf(&GLM_TOK, "glm-5.3").is_some(),
        ModelFamily::MiniMaxM3 => hf(&MINIMAX_TOK, "minimax-m3").is_some(),
        ModelFamily::OpenAiO200k | ModelFamily::OpenAiCl100k => true,
        ModelFamily::Fallback => false,
    }
}

/// Count tokens of `text` with the tokenizer of `model`'s family.
/// `model` is the best-known model id (upstream id preferred over the client
/// name); `None` or unknown families use cl100k_base.
pub fn count_tokens_for_model(model: Option<&str>, text: &str) -> u32 {
    let Some(model) = model else {
        return count_tokens_cl100k(text);
    };
    match family_for_model(model) {
        ModelFamily::Claude => match claude_custom() {
            Some(t) => count_custom(&t, text, true),
            None => count_tokens_cl100k(text),
        },
        ModelFamily::Kimi => match custom(&KIMI_TOK, "kimi-k2.7-code", load_kimi_ranks_bytes) {
            Some(t) => count_custom(&t, text, false),
            None => count_tokens_cl100k(text),
        },
        ModelFamily::Qwen3 => match hf(&QWEN_TOK, "qwen3-coder-next") {
            Some(t) => count_hf(&t, text),
            None => count_tokens_cl100k(text),
        },
        ModelFamily::DeepSeekV32 => match hf(&DEEPSEEK_V32_TOK, "deepseek-v3.2") {
            Some(t) => count_hf(&t, text),
            None => count_tokens_cl100k(text),
        },
        ModelFamily::Glm53 => match hf(&GLM_TOK, "glm-5.3") {
            Some(t) => count_hf(&t, text),
            None => count_tokens_cl100k(text),
        },
        ModelFamily::MiniMaxM3 => match hf(&MINIMAX_TOK, "minimax-m3") {
            Some(t) => count_hf(&t, text),
            None => count_tokens_cl100k(text),
        },
        ModelFamily::OpenAiO200k => {
            u32::try_from(o200k().encode_ordinary(text).len()).unwrap_or(u32::MAX)
        }
        ModelFamily::OpenAiCl100k | ModelFamily::Fallback => count_tokens_cl100k(text),
    }
}

fn count_hf(t: &tokenizers::Tokenizer, text: &str) -> u32 {
    t.encode(text, false).map_or_else(
        |_| count_tokens_cl100k(text),
        |e| u32::try_from(e.get_ids().len()).unwrap_or(u32::MAX),
    )
}

/// tiktoken-style count: NFKC-normalize (Anthropic wrapper behavior),
/// pre-split with the vocab's pattern, then BPE-merge each piece.
fn count_custom(t: &TiktokenCustom, text: &str, nfkc: bool) -> u32 {
    let normalized: std::borrow::Cow<str> = if nfkc {
        use unicode_normalization::UnicodeNormalization;
        text.nfkc().collect::<String>().into()
    } else {
        std::borrow::Cow::Borrowed(text)
    };
    let mut total: u32 = 0;
    for piece in split_pieces(&normalized, &t.split) {
        let bytes = piece.as_bytes();
        if bytes.is_empty() {
            continue;
        }
        if bytes.len() == 1 || t.ranks.contains_key(bytes) {
            total += 1; // every single byte is in these vocabs
        } else {
            total += bpe_token_count(bytes, &t.ranks);
        }
    }
    total
}

fn split_pieces<'a>(text: &'a str, re: &fancy_regex::Regex) -> Vec<&'a str> {
    let mut out = Vec::new();
    for m in re.find_iter(text) {
        match m {
            Ok(m) => out.push(&text[m.start()..m.end()]),
            Err(_) => break,
        }
    }
    out
}

#[derive(PartialEq, Eq)]
struct Merge {
    start: usize,
    rank: u32,
}

impl Ord for Merge {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Inverted so BinaryHeap pops the smallest rank first (leftmost on ties).
        other
            .rank
            .cmp(&self.rank)
            .then(other.start.cmp(&self.start))
    }
}

impl PartialOrd for Merge {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

struct BpeState {
    prev: usize,
    end: usize,
    next_end: usize,
    next_rank: u32,
    cur_rank: u32,
}

/// Faithful port of tiktoken's `_byte_pair_merge_large`; returns the number of
/// tokens the piece merges into.
fn bpe_token_count(piece: &[u8], ranks: &HashMap<Vec<u8>, u32>) -> u32 {
    const MAX: u32 = u32::MAX;
    let mut state: Vec<BpeState> = Vec::with_capacity(piece.len());
    state.push(BpeState {
        prev: usize::MAX,
        end: 1,
        next_end: 2,
        next_rank: MAX,
        cur_rank: MAX,
    });
    let mut heap = std::collections::BinaryHeap::with_capacity(piece.len());
    for i in 0..piece.len() - 1 {
        if let Some(&rank) = ranks.get(&piece[i..i + 2]) {
            heap.push(Merge { start: i, rank });
            state[i].next_rank = rank;
        }
        state.push(BpeState {
            prev: i,
            end: i + 2,
            next_end: i + 3,
            next_rank: MAX,
            cur_rank: MAX,
        });
    }

    let potential_merge = |state: &mut Vec<BpeState>,
                           heap: &mut std::collections::BinaryHeap<Merge>,
                           start: usize,
                           next_end_item: usize| {
        state[start].next_end = next_end_item;
        state[start].next_rank = MAX; // always invalidate the old merge
        if next_end_item <= piece.len()
            && let Some(&rank) = ranks.get(&piece[start..next_end_item])
        {
            heap.push(Merge { start, rank });
            state[start].next_rank = rank;
        }
    };

    while let Some(left) = heap.pop() {
        if left.rank == MAX {
            break;
        }
        if left.rank != state[left.start].next_rank {
            continue; // this merge was invalidated; ignore it
        }

        let left_start = left.start;
        let right_start = state[left_start].end;
        let right_end = state[left_start].next_end;
        let right_next_end = state[right_start].next_end;

        // Merge left and right into a single token
        state[left_start].cur_rank = state[left_start].next_rank;
        state[left_start].end = right_end;
        potential_merge(&mut state, &mut heap, left_start, right_next_end);
        if right_end < state.len() {
            state[right_end].prev = left_start;
        }
        if left_start > 0 {
            let prev_start = state[left_start].prev;
            potential_merge(&mut state, &mut heap, prev_start, right_end);
        }
        state[right_start].next_rank = MAX;
    }

    let mut count: u32 = 0;
    let mut i = 0;
    while i < state.len() {
        count += 1;
        i = state[i].end;
    }
    count
}

#[cfg(test)]
fn flush_whitespace_run(ws_run: usize) -> usize {
    if ws_run > 4 { ws_run / 10 } else { ws_run }
}

/// Historical heuristic retained only to compare regression fixtures.
#[cfg(test)]
fn estimate_tokens_heuristic(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }

    let mut cjk_count: usize = 0;
    let mut other_count: usize = 0;
    let mut ws_run: usize = 0;

    for ch in text.chars() {
        if ch.is_whitespace() {
            ws_run += 1;
        } else {
            other_count += flush_whitespace_run(ws_run);
            ws_run = 0;

            if is_cjk(ch) {
                cjk_count += 1;
            } else {
                other_count += 1;
            }
        }
    }
    other_count += flush_whitespace_run(ws_run);

    // CJK: ~2 chars/token; Latin/other: ~4 chars/token
    let cjk_tokens = cjk_count.div_ceil(2); // ceiling div
    let other_tokens = other_count.div_ceil(4); // ceiling div

    (cjk_tokens + other_tokens).max(1) as u32
}

/// Check if a character is in a CJK Unicode range.
#[cfg(test)]
fn is_cjk(ch: char) -> bool {
    let code = ch as u32;
    // CJK Unified Ideographs
    (0x4E00..=0x9FFF).contains(&code)
    // Hiragana + Katakana
    || (0x3040..=0x30FF).contains(&code)
    // Hangul Syllables
    || (0xAC00..=0xD7AF).contains(&code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn msg(role: &str, content: &str) -> OpenAIMessage {
        OpenAIMessage {
            role: role.to_string(),
            content: Some(Value::String(content.to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        }
    }

    #[test]
    fn test_estimate_english_text() {
        // "Hello world" is 2 tokens in cl100k_base.
        let tokens = estimate_completion_tokens("Hello world", None);
        assert!(
            (1..=5).contains(&tokens),
            "expected 1-5 tokens, got {tokens}"
        );
    }

    #[test]
    fn test_estimate_empty_string() {
        assert_eq!(estimate_completion_tokens("", None), 0);
    }

    #[test]
    fn test_estimate_cjk_text() {
        // CJK chars are ~1 token each in cl100k_base (not 0.5 like the
        // heuristic assumed). "你好世界" should be ~4 tokens.
        let tokens = estimate_completion_tokens("你好世界", None);
        assert!(
            (2..=6).contains(&tokens),
            "expected 2-6 tokens, got {tokens}"
        );
    }

    #[test]
    fn test_estimate_code() {
        let code = "fn main() { println!(\"hello\"); }";
        let tokens = estimate_completion_tokens(code, None);
        // Code tokenizes unlike prose, so the range is wide on purpose.
        assert!(
            (5..=30).contains(&tokens),
            "expected 5-30 tokens, got {tokens}"
        );
    }

    #[test]
    fn test_estimate_prompt_tokens_with_string_content() {
        let messages = vec![msg("user", "Hello"), msg("assistant", "Hi there!")];
        let tokens = estimate_prompt_tokens(&messages, None);
        // 2 messages x 4 overhead = 8, plus ~1 and ~2 content tokens.
        assert!(
            (10..=20).contains(&tokens),
            "expected 10-20 tokens, got {tokens}"
        );
    }

    #[test]
    fn test_estimate_prompt_tokens_with_array_content() {
        let messages = vec![OpenAIMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!([
                {"type": "text", "text": "Hello from array"},
                {"type": "text", "text": "Second part"}
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        }];
        let tokens = estimate_prompt_tokens(&messages, None);
        assert!(tokens >= 5, "expected at least 5 tokens, got {tokens}");
    }

    #[test]
    fn test_estimate_prompt_tokens_with_tool_calls() {
        let messages = vec![OpenAIMessage {
            role: "assistant".to_string(),
            content: Some(Value::String("Let me search".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![serde_json::json!({
                "id": "call_1",
                "type": "function",
                "function": {
                    "name": "search",
                    "arguments": "{\"query\": \"hello world\"}"
                }
            })]),
            extra: serde_json::Map::default(),
        }];
        let tokens = estimate_prompt_tokens(&messages, None);
        assert!(
            tokens >= 8,
            "expected at least 8 tokens (4 overhead + content + args), got {tokens}"
        );
    }

    #[test]
    fn test_estimate_prompt_tokens_message_overhead() {
        let messages = vec![msg("system", ""), msg("user", ""), msg("assistant", "")];
        let tokens = estimate_prompt_tokens(&messages, None);
        // 3 messages x 4 overhead = 12; empty content adds nothing.
        assert!(
            tokens >= 12,
            "expected at least 12 tokens (3×4 overhead), got {tokens}"
        );
    }

    #[test]
    fn test_estimate_whitespace_heavy() {
        let text = "a".to_string() + &" ".repeat(100) + "b";
        let tokens = estimate_completion_tokens(&text, None);
        // 100 spaces merge into a handful of tokens, not 25+.
        assert!(
            tokens <= 30,
            "expected ≤30 tokens for whitespace-heavy text, got {tokens}"
        );
    }

    #[test]
    fn test_bpe_vs_heuristic_differ_on_whitespace() {
        // Whitespace runs merge into adjacent word tokens, so BPE lands near
        // 3-5 tokens where chars/4 would overcount.
        let text = "hello     world     test"; // 5-space runs
        let bpe_tokens = count_tokens_cl100k(text);
        let heuristic_tokens = estimate_tokens_heuristic(text);
        assert!(bpe_tokens > 0, "BPE should produce > 0 tokens");
        assert!(
            bpe_tokens <= 10,
            "BPE should be efficient with whitespace, got {bpe_tokens}"
        );
        let _ = heuristic_tokens; // used for documentation
    }

    #[test]
    fn test_json_tokenization() {
        // Braces and quotes tokenize unlike prose, so the count is not
        // len/4.
        let json = r#"{"id":1,"name":"test","status":"active"}"#;
        let tokens = estimate_completion_tokens(json, None);
        assert!(
            (5..=30).contains(&tokens),
            "expected 5-30 tokens for JSON, got {tokens}"
        );
    }

    #[test]
    fn test_large_text_performance() {
        let large_text = "The quick brown fox jumps over the lazy dog. ".repeat(2500); // ~110KB
        let start = std::time::Instant::now();
        let tokens = estimate_completion_tokens(&large_text, None);
        let elapsed = start.elapsed();
        assert!(
            tokens > 1000,
            "expected >1000 tokens for 110KB text, got {tokens}"
        );
        assert!(
            elapsed.as_millis() < 2000,
            "tokenization took {elapsed:?} — should be < 2s for 110KB"
        );
    }

    #[test]
    fn test_message_content_to_text_extracts_tool_args() {
        let msg = OpenAIMessage {
            role: "assistant".to_string(),
            content: Some(Value::String("text content".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![serde_json::json!({
                "id": "call_1",
                "type": "function",
                "function": {
                    "name": "search",
                    "arguments": "{\"query\": \"test\"}"
                }
            })]),
            extra: serde_json::Map::default(),
        };
        let text = message_content_to_text(&msg);
        assert!(text.contains("text content"));
        assert!(text.contains("query"));
    }

    #[test]
    fn test_estimate_completion_tokens_from_body_tool_calls() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "delegate",
                            "arguments": "{\"tasks\":[{\"context\":\"Research host logs and system status\"}]}"
                        }
                    }]
                }
            }]
        });
        let tokens = estimate_completion_tokens_from_body(&body, None);
        assert!(
            tokens >= 15,
            "expected at least 15 tokens for tool call, got {tokens}"
        );
    }

    #[test]
    fn test_estimate_completion_tokens_from_body_anthropic_tool_use() {
        let body = serde_json::json!({
            "content": [
                { "type": "thinking", "thinking": "Let me check the tools." },
                { "type": "tool_use", "name": "get_weather", "input": { "city": "Madrid" } }
            ]
        });
        let tokens = estimate_completion_tokens_from_body(&body, None);
        assert!(
            tokens >= 10,
            "expected at least 10 tokens for anthropic tool_use, got {tokens}"
        );
    }

    #[test]
    fn test_estimate_completion_tokens_from_body_empty() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null
                }
            }]
        });
        let tokens = estimate_completion_tokens_from_body(&body, None);
        assert_eq!(tokens, 0);
    }

    // ===== Model-aware tokenizer registry =====

    #[test]
    fn test_unknown_family_falls_back_to_cl100k() {
        for model in [
            "gemini-3.5-flash-lite",
            "glm-4.5",
            "minimax-m2",
            "qwen2.5-coder",
            "deepseek-v3",
            "combo:sonnet55",
        ] {
            assert_eq!(
                family_for_model(model),
                ModelFamily::Fallback,
                "family dispatch for {model}"
            );
        }
        // Fallback (and None) must equal raw cl100k.
        let text = "The quick brown fox jumps over the lazy dog.";
        assert_eq!(
            count_tokens_for_model(Some("gemini-3.5-flash-lite"), text),
            count_tokens_cl100k(text)
        );
        assert_eq!(
            count_tokens_for_model(None, text),
            count_tokens_cl100k(text)
        );
    }

    #[test]
    fn test_family_dispatch() {
        assert_eq!(family_for_model("claude-sonnet-5.5"), ModelFamily::Claude);
        assert_eq!(
            family_for_model("Anthropic/claude-opus-4.8"),
            ModelFamily::Claude
        );
        assert_eq!(
            family_for_model("moonshotai/Kimi-K2.7-code"),
            ModelFamily::Kimi
        );
        // Unverified Kimi revisions must not inherit the K2.7 vocabulary.
        assert_eq!(family_for_model("kimi-k3"), ModelFamily::Fallback);
        assert_eq!(family_for_model("qwen3-coder-next"), ModelFamily::Qwen3);
        assert_eq!(family_for_model("Qwen/Qwen3-32B"), ModelFamily::Qwen3);
        assert_eq!(family_for_model("qwen3-max"), ModelFamily::Fallback);
        assert_eq!(family_for_model("deepseek-v3.2"), ModelFamily::DeepSeekV32);
        assert_eq!(family_for_model("zai-org/GLM-5.3"), ModelFamily::Glm53);
        assert_eq!(
            family_for_model("MiniMaxAI/MiniMax-M3"),
            ModelFamily::MiniMaxM3
        );
        assert_eq!(family_for_model("gpt-5.4"), ModelFamily::OpenAiO200k);
        // Only the families tiktoken-rs 0.12 documents get o200k; future ids
        // keep the conservative cl100k mapping instead of guessing.
        assert_eq!(family_for_model("gpt-6.1-sol"), ModelFamily::OpenAiCl100k);
        assert_eq!(family_for_model("gpt-7-unknown"), ModelFamily::OpenAiCl100k);
        assert_eq!(family_for_model("o4-mini"), ModelFamily::OpenAiO200k);
        // `o5-*` has no published vocabulary in tiktoken-rs; a prefix guess
        // used to map it to o200k, which the probe proved wrong.
        assert_eq!(family_for_model("o5-unknown"), ModelFamily::OpenAiCl100k);
        assert_eq!(family_for_model("o5"), ModelFamily::OpenAiCl100k);
        assert_eq!(
            family_for_model("openai/gpt-5.6-luna"),
            ModelFamily::OpenAiO200k
        );
        // Routing prefixes and provider suffixes must not hide the model name.
        assert_eq!(family_for_model("openai:gpt-5"), ModelFamily::OpenAiO200k);
        assert_eq!(family_for_model("gpt-5:free"), ModelFamily::OpenAiO200k);
        assert_eq!(
            family_for_model("qwen3-coder-next:free"),
            ModelFamily::Qwen3
        );
        assert_eq!(
            family_for_model("moonshotai/Kimi-K2.7-Code:beta"),
            ModelFamily::Kimi
        );
        // Legacy OpenAI vocabularies must not be upgraded to o200k by family
        // name alone.
        assert_eq!(family_for_model("gpt-4"), ModelFamily::OpenAiCl100k);
        assert_eq!(family_for_model("gpt-4-turbo"), ModelFamily::OpenAiCl100k);
        assert_eq!(family_for_model("gpt-3.5-turbo"), ModelFamily::OpenAiCl100k);
    }

    #[test]
    fn test_look_alike_ids_do_not_inherit_versioned_vocabularies() {
        // Substring matching used to activate the Qwen artifact for any id
        // containing the family name; only the canonical id may match.
        assert_eq!(
            family_for_model("not-qwen3-coder-next-unknown"),
            ModelFamily::Fallback
        );
        assert_eq!(
            family_for_model("notqwen3-coder-next"),
            ModelFamily::Fallback
        );
        assert_eq!(
            family_for_model("qwen3-coder-next-unknown"),
            ModelFamily::Fallback
        );
        assert_eq!(
            family_for_model("not-kimi-k2.7-code"),
            ModelFamily::Fallback
        );
        assert_eq!(family_for_model("not-deepseek-v3.2"), ModelFamily::Fallback);
        // Recognized deployment suffixes stay inside the verified family.
        assert_eq!(family_for_model("qwen3-coder-next-fp8"), ModelFamily::Qwen3);
    }

    #[test]
    fn test_documented_deepseek_32_alias_maps_to_the_verified_revision() {
        // `kiro/deepseek-3.2` is the documented integration alias for
        // DeepSeek-V3.2; it must resolve to the pinned vocabulary instead of
        // falling back.
        assert_eq!(
            family_for_model("kiro/deepseek-3.2"),
            ModelFamily::DeepSeekV32
        );
        assert_eq!(
            family_for_model("kiro/deepseek-3.2:free"),
            ModelFamily::DeepSeekV32
        );
        // Unrelated DeepSeek revisions keep the conservative fallback.
        assert_eq!(family_for_model("deepseek-v3.1"), ModelFamily::Fallback);
    }

    #[test]
    fn test_openai_vocabularies_are_split() {
        let text = "hello world, 你好世界";
        assert_eq!(
            count_tokens_for_model(Some("gpt-4"), text),
            count_tokens_cl100k(text)
        );
        assert_eq!(
            count_tokens_for_model(Some("gpt-4o"), text),
            u32::try_from(o200k().encode_ordinary(text).len()).unwrap()
        );
        assert_eq!(
            count_tokens_for_model(Some("gpt-5.4"), text),
            u32::try_from(o200k().encode_ordinary(text).len()).unwrap()
        );
    }

    #[test]
    fn test_special_token_strings_are_counted_as_text() {
        // Literal special-token strings from users are ordinary text, never
        // silently dropped or treated as control tokens.
        let text = "<|im_start|>你好 🌍 <|endoftext|>";
        for model in ["gpt-4", "gpt-5", "gemini-3.5-flash-lite"] {
            let count = count_tokens_for_model(Some(model), text);
            assert!(count >= 8, "{model}: expected >= 8 tokens, got {count}");
            assert_ne!(
                count,
                u32::try_from(text.chars().count() / 4).unwrap(),
                "{model}: must not use chars/4"
            );
        }
    }

    #[test]
    fn test_bucket_hint_stays_within_one_bucket_for_mixed_content() {
        use crate::token_estimate::estimate_prompt_tokens_bucket_hint;
        let texts = [
            "The quick brown fox jumps over the lazy dog. ".repeat(120),
            "fn item_42(ids: &[u32]) -> usize { ids.iter().filter(|id| **id > 0).count() }\n"
                .repeat(80),
            r#"{"id":12345,"name":"widget","tags":["alpha","beta"],"ok":true}"#.repeat(80),
            "你好世界，テストです。안녕하세요. ".repeat(200),
            "https://example.com/api/v1/items/12345?query=hello&page=2 ".repeat(100),
            "1234567890 9876543210 5555555555 ".repeat(120),
            "a b c d e f g h i j k l m n o p ".repeat(120),
        ];
        for text in &texts {
            let messages = vec![msg("user", text)];
            let exact = estimate_prompt_tokens(&messages, None);
            let hint = estimate_prompt_tokens_bucket_hint(&messages).max(1);
            let delta = bucket_for_test(exact).abs_diff(bucket_for_test(hint));
            assert!(
                delta <= 1,
                "bucket drift > 1 for {} bytes: exact={exact} (b{}), hint={hint} (b{})",
                text.len(),
                bucket_for_test(exact),
                bucket_for_test(hint)
            );
        }
    }

    #[test]
    fn test_missing_assets_are_reported_not_silenced() {
        let problems = validate_tokenizer_assets(Path::new(
            "/nonexistent/openproxy-tokenizer-assets-for-test",
        ))
        .expect_err("missing assets must be reported");
        for id in [
            "qwen3-coder-next",
            "deepseek-v3.2",
            "kimi-k2.7-code",
            "glm-5.3",
            "minimax-m3",
        ] {
            assert!(
                problems.iter().any(|p| p.contains(id)),
                "missing report for {id}: {problems:?}"
            );
        }
    }

    #[test]
    fn test_manifest_pins_sha256_and_revisions() {
        let manifest = manifest().expect("embedded manifest must parse");
        assert_eq!(manifest.artifacts.len(), 5);
        for artifact in &manifest.artifacts {
            assert_eq!(artifact.sha256.len(), 64, "{} sha256", artifact.id);
            assert!(
                artifact.url.contains(&artifact.revision),
                "{} URL must pin its revision",
                artifact.id
            );
        }
    }

    #[test]
    fn test_bucket_hint_matches_exact_bucket_for_typical_prompts() {
        use crate::token_estimate::estimate_prompt_tokens_bucket_hint;
        let texts = [
            "short".to_string(),
            "The quick brown fox jumps over the lazy dog. ".repeat(100),
            "你好世界，测试。".repeat(500),
            "The quick brown fox jumps over the lazy dog. ".repeat(450),
        ];
        for text in &texts {
            let messages = vec![msg("user", text)];
            let exact = estimate_prompt_tokens(&messages, None);
            let hint = estimate_prompt_tokens_bucket_hint(&messages).max(1);
            assert_eq!(
                bucket_for_test(exact),
                bucket_for_test(hint),
                "bucket drift for {} bytes: exact={exact}, hint={hint}",
                text.len()
            );
        }
    }

    /// Local mirror of `latency_budget::stats::tokens_bucket` (private to the
    /// pipeline crate). Kept in sync by the boundary values.
    fn bucket_for_test(tokens: u32) -> usize {
        match tokens.max(1) {
            0..=999 => 0,
            1_000..=3_999 => 1,
            4_000..=15_999 => 2,
            16_000..=63_999 => 3,
            64_000..=255_999 => 4,
            _ => 5,
        }
    }
}
