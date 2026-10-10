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

    pub fn find_syntax_protected_ranges(&self, text: &str) -> Vec<Range<usize>> {
        protected::find_syntax_protected_ranges(text)
    }

    pub fn find_code_protected_ranges(&self, text: &str) -> Vec<Range<usize>> {
        protected::find_code_protected_ranges(text)
    }

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

    fn is_protocol_metadata_field(key: &str) -> bool {
        matches!(
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
                | "tool_call_id"
                | "finish_reason"
        )
    }

    fn is_secret_field_name(key: &str) -> bool {
        let lower = key.to_ascii_lowercase();
        matches!(
            lower.as_str(),
            "secret"
                | "password"
                | "passwd"
                | "passphrase"
                | "pass"
                | "pwd"
                | "token"
                | "api_key"
                | "apikey"
                | "private_key"
                | "access_key"
                | "master_key"
                | "signing_key"
                | "encryption_key"
                | "auth_key"
                | "client_secret"
                | "app_secret"
                | "webhook_secret"
                | "signing_secret"
                | "api_hash"
                | "app_hash"
                | "contraseña"
                | "contrasena"
                | "clave"
        ) || lower.ends_with("_secret")
            || lower.ends_with("_token")
            || lower.ends_with("_key")
            || lower.ends_with("_password")
            || lower.ends_with("_passwd")
            || lower.ends_with("_hash")
            || lower.starts_with("secret_")
            || lower.starts_with("token_")
            || lower.starts_with("api_key_")
    }

    fn should_redact_secret_leaf(key: &str, s: &str) -> bool {
        let trimmed = s.trim();
        if trimmed.len() < 4 {
            return false;
        }
        is_valid_secret_value(key, trimmed)
    }

    fn is_multimodal_payload_field(key: &str, val: &serde_json::Value) -> bool {
        if matches!(key, "inline_data" | "inlineData" | "input_audio" | "audio") {
            return true;
        }

        if key == "source" && val.get("type").and_then(|t| t.as_str()) == Some("base64") {
            return true;
        }

        if matches!(key, "b64_json" | "images") {
            return true;
        }

        if key == "url"
            && let Some(s) = val.as_str()
            && s.starts_with("data:")
        {
            return true;
        }

        if key == "data"
            && let Some(s) = val.as_str()
            && (s.starts_with("data:") || Self::is_probable_base64_payload(s))
        {
            return true;
        }

        false
    }

    fn is_multimodal_or_metadata_field(key: &str, val: &serde_json::Value) -> bool {
        Self::is_protocol_metadata_field(key) || Self::is_multimodal_payload_field(key, val)
    }

    /// Redacts leaf strings recursively, leaving structural JSON keys,
    /// function identifier names and multimodal payloads intact. A string that
    /// is itself serialized JSON (`tool_calls.arguments`) is parsed, redacted
    /// and re-serialized, so the output escaping stays valid.
    pub fn redact_json_value(&self, val: &mut serde_json::Value, session: &mut PiiSession) {
        self.redact_json_value_internal(val, session, false);
    }

    fn redact_json_value_internal(
        &self,
        val: &mut serde_json::Value,
        session: &mut PiiSession,
        in_payload: bool,
    ) {
        match val {
            serde_json::Value::String(s) => {
                let trimmed = s.trim();
                if trimmed.starts_with("data:") && trimmed.contains(";base64,") {
                    return;
                }
                if trimmed.len() >= 256 && Self::is_probable_base64_payload(trimmed) {
                    return;
                }
                if ((trimmed.starts_with('{') && trimmed.ends_with('}'))
                    || (trimmed.starts_with('[') && trimmed.ends_with(']')))
                    && let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(s)
                {
                    self.redact_json_value_internal(&mut parsed, session, true);
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
                    self.redact_json_value_internal(item, session, in_payload);
                }
            }
            serde_json::Value::Object(map) => {
                for (k, v) in map.iter_mut() {
                    if in_payload {
                        if Self::is_multimodal_payload_field(k, v) {
                            continue;
                        }
                        if self.has_entity(PiiEntity::Secret)
                            && Self::is_secret_field_name(k)
                            && let serde_json::Value::String(s) = v
                            && Self::should_redact_secret_leaf(k, s)
                        {
                            let placeholder =
                                session.get_or_create_placeholder(PiiEntity::Secret, s);
                            *s = placeholder;
                            continue;
                        }
                        if self.has_entity(PiiEntity::Secret)
                            && let serde_json::Value::Number(number) = v
                            && number.is_u64()
                        {
                            let token = number.to_string();
                            let labeled = format!("\"{k}\": {token}");
                            let is_sensitive_id = REGEX_SECRET_PLATFORM_ID.is_match(&labeled);
                            if is_sensitive_id
                                || (Self::is_secret_field_name(k)
                                    && Self::should_redact_secret_leaf(k, &token))
                            {
                                let placeholder =
                                    session.get_or_create_placeholder(PiiEntity::Secret, &token);
                                if let Ok(replacement) = placeholder.parse::<serde_json::Number>() {
                                    *number = replacement;
                                }
                                continue;
                            }
                        }
                        self.redact_json_value_internal(v, session, true);
                    } else {
                        if Self::is_multimodal_or_metadata_field(k, v) {
                            continue;
                        }
                        let child_in_payload = k == "arguments";
                        self.redact_json_value_internal(v, session, child_in_payload);
                    }
                }
            }
            _ => {}
        }
    }

    fn seed_json_value(val: &serde_json::Value, session: &mut PiiSession) {
        match val {
            serde_json::Value::String(s) => {
                session.seed_existing_placeholders(s);
                let trimmed = s.trim();
                if ((trimmed.starts_with('{') && trimmed.ends_with('}'))
                    || (trimmed.starts_with('[') && trimmed.ends_with(']')))
                    && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s)
                {
                    Self::seed_json_value(&parsed, session);
                }
            }
            serde_json::Value::Array(arr) => {
                for item in arr {
                    Self::seed_json_value(item, session);
                }
            }
            serde_json::Value::Object(map) => {
                for v in map.values() {
                    Self::seed_json_value(v, session);
                }
            }
            _ => {}
        }
    }

    pub fn redact_messages(
        &self,
        messages: &[OpenAIMessage],
        session: &mut PiiSession,
    ) -> Vec<OpenAIMessage> {
        // Preseed all messages to avoid placeholder collisions across turns
        for msg in messages {
            if let Some(ref content) = msg.content {
                Self::seed_json_value(content, session);
            }
            for (k, v) in &msg.extra {
                if k == "reasoning_content" || k == "thought" {
                    Self::seed_json_value(v, session);
                }
            }
            if let Some(ref tool_calls) = msg.tool_calls {
                for tc in tool_calls {
                    Self::seed_json_value(tc, session);
                }
            }
        }

        messages
            .iter()
            .map(|msg| {
                let mut cloned = msg.clone();
                let is_tool_msg = cloned.role == "tool";

                if let Some(ref mut content) = cloned.content {
                    self.redact_json_value_internal(content, session, is_tool_msg);
                }

                for (k, v) in &mut cloned.extra {
                    if k == "reasoning_content" || k == "thought" {
                        self.redact_json_value(v, session);
                    }
                }

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
