//! Claude Code metadata sanitizer for Antigravity provider.
//!
//! Precision-scoped:
//! - Sanitizes ONLY `systemInstruction` / `system_instruction` and tool `description`
//! - NEVER touches `contents` (user/model turns), tool arguments/results, or signatures
//! - Strips ONLY explicit billing metadata lines:
//!   - `(x-)?anthropic-billing-header:`
//!   - `x-client-metadata:` (or `x-client-*:`) carrying explicit `cc_*` attributes (`cc_workload`,
//!     `cc_is_subagent`, `cc_prev_req`, `cc_prompt_id`, `cc_turn_origin`, `cc_version`, `cc_entrypoint`, `cch=`)
//! - Normalizes external identity ONLY on known Claude Code / Agent SDK preambles at the
//!   beginning of system prompts, leaving user instructions intact
//! - Fully exempts fenced code blocks (```...```) and inline code (`...`)
//! - Preserves legitimate whitespace and formatting in descriptions without broad trimming

use openproxy_types::static_regex;
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

/// Fenced code blocks ```...``` and inline code `...`
static RE_CODE_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| static_regex!(r"(?ms)(```[\s\S]*?```|`[^`\r\n]+`)"));

/// Matches an entire single line that is an unambiguous Anthropic billing header
/// or client metadata header carrying explicit Claude Code telemetry (cc_*).
/// Anchored to line start and consumes trailing line break.
static RE_METADATA_HEADER_LINE: LazyLock<Regex> = LazyLock::new(|| {
    static_regex!(concat!(
        r"(?im)^\s*(?:x-anthropic-billing-header|anthropic-billing-header):\s*[^\r\n]*(\r?\n)?",
        r"|^\s*(?:x-client-metadata|x-client-[a-z0-9_-]+):\s*[^\r\n]*(?:cc_version|cc_entrypoint|cc_workload|cc_is_subagent|cc_prev_req|cc_prompt_id|cc_turn_origin|cch=)[^\r\n]*(\r?\n)?"
    ))
});

/// Official Antigravity identity statement
pub const OFFICIAL_ANTIGRAVITY_IDENTITY: &str = "You are Antigravity, a powerful agentic AI coding assistant designed by the Google Deepmind team working on Advanced Agentic Coding.";

/// Known Claude Code / Agent SDK preamble forms at the start of system instructions
static RE_KNOWN_PREAMBLE_START: LazyLock<Regex> = LazyLock::new(|| {
    static_regex!(concat!(
        r"(?i)^\s*(?:",
        r"You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK\.",
        r"|You are Claude Code, Anthropic's official CLI for Claude\.",
        r"|You are a Claude agent, built on Anthropic's Claude Agent SDK\.",
        r")(\r?\n)?"
    ))
});

pub struct PromptSanitizer;

impl PromptSanitizer {
    /// Strip metadata header lines from text outside code blocks.
    /// Preserves all other text, indentation, and structure without aggressive trimming.
    pub fn strip_metadata_lines(text: &str) -> String {
        let lower = text.to_lowercase();
        let has_suspect = lower.contains("anthropic-billing-header")
            || lower.contains("cc_version")
            || lower.contains("cc_entrypoint")
            || lower.contains("cc_workload")
            || lower.contains("cc_is_subagent")
            || lower.contains("cc_prev_req")
            || lower.contains("cc_prompt_id")
            || lower.contains("cc_turn_origin")
            || lower.contains("cch=");

        if !has_suspect {
            return text.to_string();
        }

        let mut result = String::with_capacity(text.len());
        let mut last_idx = 0;

        for m in RE_CODE_BLOCK.find_iter(text) {
            let non_code = &text[last_idx..m.start()];
            if !non_code.is_empty() {
                let cleaned = RE_METADATA_HEADER_LINE.replace_all(non_code, "");
                result.push_str(&cleaned);
            }
            result.push_str(m.as_str());
            last_idx = m.end();
        }

        if last_idx < text.len() {
            let non_code = &text[last_idx..];
            let cleaned = RE_METADATA_HEADER_LINE.replace_all(non_code, "");
            result.push_str(&cleaned);
        }

        result
    }

    /// Normalize known Claude Code / Agent SDK preambles at the start of system text.
    /// Replaces the known preamble with the official Antigravity identity while preserving
    /// everything that follows.
    pub fn normalize_known_preamble(text: &str) -> String {
        if let Some(m) = RE_KNOWN_PREAMBLE_START.find(text) {
            let mut result =
                String::with_capacity(text.len() + OFFICIAL_ANTIGRAVITY_IDENTITY.len());
            result.push_str(OFFICIAL_ANTIGRAVITY_IDENTITY);
            let matched_str = m.as_str();
            if matched_str.ends_with('\n') {
                if matched_str.ends_with("\r\n") {
                    result.push_str("\r\n");
                } else {
                    result.push('\n');
                }
            } else if m.end() < text.len() && !text[m.end()..].starts_with(' ') {
                result.push(' ');
            }
            result.push_str(&text[m.end()..]);
            result
        } else {
            text.to_string()
        }
    }

    /// Sanitize a system text segment: strip metadata lines and normalize known preamble.
    pub fn sanitize_system_text(text: &str) -> String {
        let stripped = Self::strip_metadata_lines(text);
        Self::normalize_known_preamble(&stripped)
    }

    /// Sanitize tool description: strip metadata lines only, preserving legitimate text and formatting.
    pub fn sanitize_tool_description(text: &str) -> String {
        Self::strip_metadata_lines(text)
    }

    /// Precision sanitization of Gemini request payload:
    /// - Cleans ONLY `systemInstruction` / `system_instruction`
    /// - Cleans ONLY `description` in tool declarations
    /// - NEVER inspects or alters `contents` (user/model turns, thought blocks, signatures, tool calls/responses)
    pub fn sanitize_gemini_request(body: &mut Value) -> usize {
        let mut cleaned_count = 0;

        // 1. Sanitize systemInstruction / system_instruction
        let sys_key = if body.get_mut("systemInstruction").is_some() {
            Some("systemInstruction")
        } else if body.get_mut("system_instruction").is_some() {
            Some("system_instruction")
        } else {
            None
        };

        if let Some(key) = sys_key
            && let Some(sys) = body.get_mut(key).and_then(Value::as_object_mut)
        {
            if let Some(parts) = sys.get_mut("parts").and_then(Value::as_array_mut) {
                for part in parts.iter_mut() {
                    if let Some(obj) = part.as_object_mut() {
                        // Keep thought parts and thought signatures completely untouched
                        if obj.get("thought").and_then(Value::as_bool).unwrap_or(false)
                            || obj.contains_key("thoughtSignature")
                            || obj.contains_key("thought_signature")
                        {
                            continue;
                        }

                        if let Some(text_val) = obj.get("text").and_then(Value::as_str) {
                            let cleaned = Self::sanitize_system_text(text_val);
                            if cleaned != text_val {
                                obj.insert("text".to_string(), Value::String(cleaned));
                                cleaned_count += 1;
                            }
                        }
                    }
                }

                // Retain non-empty parts, but never prune thought parts
                parts.retain(|part| {
                    let is_thought = part
                        .get("thought")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                        || part.get("thoughtSignature").is_some()
                        || part.get("thought_signature").is_some();
                    if is_thought {
                        return true;
                    }
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        !t.trim().is_empty()
                    } else {
                        true
                    }
                });
            }

            let is_parts_empty = sys
                .get("parts")
                .and_then(Value::as_array)
                .is_none_or(|p| p.is_empty());

            if is_parts_empty {
                body.as_object_mut().map(|b| b.remove(key));
            }
        }

        // 2. Sanitize tools: ONLY description fields in functionDeclarations
        if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
            for tool in tools.iter_mut() {
                let decls_key = if tool.get("functionDeclarations").is_some() {
                    Some("functionDeclarations")
                } else if tool.get("function_declarations").is_some() {
                    Some("function_declarations")
                } else {
                    None
                };

                if let Some(key) = decls_key
                    && let Some(decls) = tool.get_mut(key).and_then(Value::as_array_mut)
                {
                    for decl in decls.iter_mut() {
                        let desc_opt = decl
                            .get("description")
                            .and_then(Value::as_str)
                            .map(|s| s.to_string());
                        if let Some(desc) = desc_opt {
                            let cleaned = Self::sanitize_tool_description(&desc);
                            if cleaned != desc
                                && let Some(obj) = decl.as_object_mut()
                            {
                                obj.insert("description".to_string(), Value::String(cleaned));
                                cleaned_count += 1;
                            }
                        }
                    }
                }
            }
        }

        cleaned_count
    }
}
