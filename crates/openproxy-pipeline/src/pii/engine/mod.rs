//! Native in-process PII detection and pseudonymization engine.

pub mod candidates;
pub mod protected;
pub mod regexes;
pub mod stopwords;
pub mod validators;

use openproxy_types::config::PiiEntity;
use openproxy_types::message::OpenAIMessage;
use std::ops::Range;

use candidates::{Candidate, collect_candidates};
pub use protected::{
    find_code_protected_ranges, find_protected_ranges, find_syntax_protected_ranges,
    is_in_protected_range, merge_ranges,
};
pub use regexes::*;
pub use stopwords::is_stopword;
pub use validators::*;

use super::session::PiiSession;

/// Native, deterministic PII detection and redaction engine.
#[derive(Debug, Clone)]
pub struct PiiEngine {
    entities: Vec<PiiEntity>,
}

impl PiiEngine {
    pub fn new(entities: &[PiiEntity]) -> Self {
        Self {
            entities: entities.to_vec(),
        }
    }

    pub fn from_config(config: &openproxy_types::config::PiiConfig) -> Self {
        Self::new(&config.pii_entities)
    }

    #[inline]
    pub fn has_entity(&self, entity: PiiEntity) -> bool {
        self.entities.contains(&entity)
    }

    /// Identify syntax-protected spans (URLs with carveouts, JSON keys)
    /// that must not have their structural boundaries corrupted.
    pub fn find_syntax_protected_ranges(&self, text: &str) -> Vec<Range<usize>> {
        protected::find_syntax_protected_ranges(text)
    }

    /// Identify code blocks (fenced ``` and inline `) to avoid heuristic false-positives
    /// on code variables, types, or syntax.
    pub fn find_code_protected_ranges(&self, text: &str) -> Vec<Range<usize>> {
        protected::find_code_protected_ranges(text)
    }

    /// Identify all protected spans (combining syntax and code protection).
    pub fn find_protected_ranges(&self, text: &str) -> Vec<Range<usize>> {
        protected::find_protected_ranges(text)
    }

    pub(crate) fn collect_candidates<'a>(
        &self,
        text: &'a str,
        syntax_protected: &[Range<usize>],
        code_protected: &[Range<usize>],
    ) -> Vec<Candidate<'a>> {
        collect_candidates(&self.entities, text, syntax_protected, code_protected)
    }

    /// Redact detected PII in `text`, replacing them with stable per-request placeholders.
    pub fn redact_text(&self, text: &str, session: &mut PiiSession) -> String {
        if text.is_empty() || self.entities.is_empty() {
            return text.to_string();
        }

        session.seed_existing_placeholders(text);

        let syntax_protected = self.find_syntax_protected_ranges(text);
        let code_protected = self.find_code_protected_ranges(text);
        let mut candidates = self.collect_candidates(text, &syntax_protected, &code_protected);

        if candidates.is_empty() {
            return text.to_string();
        }

        // Sort by start asc, longer match first if same start
        candidates.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| b.end.cmp(&a.end)));

        // Remove overlapping candidates
        let mut non_overlapping = Vec::with_capacity(candidates.len());
        let mut last_end = 0;
        for cand in candidates {
            if cand.start >= last_end {
                last_end = cand.end;
                non_overlapping.push(cand);
            }
        }

        let mut out = String::with_capacity(text.len() + 32);
        let mut curr = 0;

        for cand in non_overlapping {
            out.push_str(&text[curr..cand.start]);
            let placeholder = session.get_or_create_placeholder(cand.entity, cand.matched_text);
            out.push_str(&placeholder);
            curr = cand.end;
        }

        out.push_str(&text[curr..]);
        out
    }

    #[inline]
    fn is_probable_base64_payload(s: &str) -> bool {
        let trimmed = s.trim();
        if trimmed.len() < 64 {
            return false;
        }
        let mut base64_len = 0;
        for b in trimmed.bytes() {
            if b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=' {
                base64_len += 1;
            } else if b == b'\r' || b == b'\n' {
                continue;
            } else {
                return false;
            }
        }
        base64_len >= 64
    }

    fn is_multimodal_or_metadata_field(key: &str, val: &serde_json::Value) -> bool {
        // 1. Structural / protocol metadata
        if matches!(
            key,
            "id" | "type"
                | "name"
                | "role"
                | "model"
                | "mime_type"
                | "mimeType"
                | "media_type"
                | "mediaType"
                | "format"
                | "detail"
        ) {
            return true;
        }

        // 2. Multimodal container objects
        if matches!(key, "inline_data" | "inlineData" | "input_audio" | "audio") {
            return true;
        }

        // 3. Anthropic base64 source object
        if key == "source" && val.get("type").and_then(|t| t.as_str()) == Some("base64") {
            return true;
        }

        // 4. Raw base64 or media binary arrays
        if matches!(key, "b64_json" | "images") {
            return true;
        }

        // 5. Image/audio URLs with data URI
        if key == "url"
            && let Some(s) = val.as_str()
            && s.starts_with("data:")
        {
            return true;
        }

        // 6. Generic "data" key containing base64 or data URI
        if key == "data"
            && let Some(s) = val.as_str()
            && (s.starts_with("data:") || Self::is_probable_base64_payload(s))
        {
            return true;
        }

        false
    }

    /// Recursively redact strings inside a serde_json::Value.
    /// Structural JSON keys and function identifier names (id, type, name) are preserved.
    /// Multimodal payload content (base64 images, audio, data URIs) is preserved intact.
    /// If a string is itself serialized JSON (e.g. tool_calls.arguments), it is parsed,
    /// recursively redacted at leaf strings, and re-serialized, guaranteeing valid JSON escaping.
    pub fn redact_json_value(&self, val: &mut serde_json::Value, session: &mut PiiSession) {
        match val {
            serde_json::Value::String(s) => {
                let trimmed = s.trim();
                // Preserve RFC 2397 Data URIs (data:image/...;base64,...)
                if trimmed.starts_with("data:") && trimmed.contains(";base64,") {
                    return;
                }
                // Preserve large continuous base64 media payloads
                if trimmed.len() >= 256 && Self::is_probable_base64_payload(trimmed) {
                    return;
                }
                if ((trimmed.starts_with('{') && trimmed.ends_with('}'))
                    || (trimmed.starts_with('[') && trimmed.ends_with(']')))
                    && let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(s)
                {
                    self.redact_json_value(&mut parsed, session);
                    if let Ok(serialized) = serde_json::to_string(&parsed) {
                        *s = serialized;
                        return;
                    }
                }
                let redacted = self.redact_text(s, session);
                *s = redacted;
            }
            serde_json::Value::Array(arr) => {
                for item in arr {
                    self.redact_json_value(item, session);
                }
            }
            serde_json::Value::Object(map) => {
                for (k, v) in map.iter_mut() {
                    if Self::is_multimodal_or_metadata_field(k, v) {
                        continue;
                    }
                    self.redact_json_value(v, session);
                }
            }
            _ => {}
        }
    }

    /// Redact messages (system, developer, user, assistant, tool, function) and tool_calls
    /// in a slice of OpenAIMessage.
    pub fn redact_messages(
        &self,
        messages: &[OpenAIMessage],
        session: &mut PiiSession,
    ) -> Vec<OpenAIMessage> {
        messages
            .iter()
            .map(|msg| {
                let mut cloned = msg.clone();

                // Intercept content of all messages
                if let Some(ref mut content) = cloned.content {
                    self.redact_json_value(content, session);
                }

                // Intercept extra fields (e.g. reasoning_content, thought)
                for (k, v) in &mut cloned.extra {
                    if k == "reasoning_content" || k == "thought" {
                        self.redact_json_value(v, session);
                    }
                }

                // Intercept tool_calls arguments in any message
                if let Some(ref mut tool_calls) = cloned.tool_calls {
                    for tc in tool_calls.iter_mut() {
                        self.redact_json_value(tc, session);
                    }
                }

                cloned
            })
            .collect()
    }
}
