//! Devin thought-signature decoding.
//!
//! Assistant signatures arrive in several client spellings; these helpers
//! normalize them into the `(wire bytes, signature_type)` pair the
//! `ChatMessage` prompt carries at fields 12/18. Port of upstream
//! `parseSignatureBytes` (prefix layer + base64-inner literal markers).

use serde_json::Value;

/// Decode an assistant signature blob into `(wire bytes, signature_type)`.
///
/// Port of upstream `parseSignatureBytes`: `claude#`/`gpt#`/`gemini#`
/// prefixes strip and tag, `sealed.v1.` stays as-is, and opaque strings are
/// base64-sniffed for the embedded family markers (`CAQS`/`CAIS` → anthropic,
/// `gAAAA` → openai, leading 0x01 → gemini). A `{signature, signature_type}`
/// JSON object is also accepted for clients that carry the pair explicitly.
pub(crate) fn parse_signature_value(raw: &str) -> (Vec<u8>, String) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return (Vec::new(), String::new());
    }

    // JSON forms: {"signature": "...", "signature_type": "..."} or
    // {"sig": "...", "type": "..."}.
    if trimmed.starts_with('{')
        && let Ok(v) = serde_json::from_str::<Value>(trimmed)
        && let Some(obj) = v.as_object()
    {
        let sig = obj
            .get("signature")
            .or_else(|| obj.get("sig"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let kind = obj
            .get("signature_type")
            .or_else(|| obj.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let (bytes, detected) = parse_signature_value(sig);
        let kind = if kind.is_empty() {
            detected
        } else {
            kind.to_string()
        };
        return (bytes, kind);
    }

    for (prefix, kind) in [
        ("sealed.v1.", "sealed"),
        ("claude#", "anthropic"),
        ("gpt#", "openai"),
        ("gemini#", "gemini"),
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let bytes = if kind == "sealed" {
                trimmed.as_bytes().to_vec()
            } else {
                rest.as_bytes().to_vec()
            };
            return (bytes, kind.to_string());
        }
    }
    if trimmed.starts_with("AY") {
        return (trimmed.as_bytes().to_vec(), "gemini".to_string());
    }
    if trimmed.starts_with("CAQS") || trimmed.starts_with("CAIS") {
        return (trimmed.as_bytes().to_vec(), "anthropic".to_string());
    }
    if trimmed.starts_with("gAAAA") {
        return (trimmed.as_bytes().to_vec(), "openai".to_string());
    }

    // Base64-inside-a-string carriers: decode once and classify the payload.
    use base64::Engine;
    if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(trimmed)
        && !decoded.is_empty()
    {
        if let Ok(text) = std::str::from_utf8(&decoded) {
            let text = text.trim();
            if let Some(inner) = text.strip_prefix("cpa-codex-gemini-carrier-v1:") {
                let inner = inner.trim();
                if inner.starts_with("sealed.v1.") {
                    return (inner.as_bytes().to_vec(), "sealed".to_string());
                }
                // Upstream drops carrier payloads whose inner blob is not a
                // recognized provider signature.
                let (bytes, kind) = parse_signature_value(inner);
                return if kind == "sealed" {
                    (Vec::new(), String::new())
                } else {
                    (bytes, kind)
                };
            }
            if text.starts_with("sealed.v1.") {
                return (decoded, "sealed".to_string());
            }
            if text.starts_with("CAQS") || text.starts_with("CAIS") {
                return (decoded, "anthropic".to_string());
            }
            if text.starts_with("gAAAA") {
                return (decoded, "openai".to_string());
            }
        }
        if decoded[0] == 0x01 {
            return (trimmed.as_bytes().to_vec(), "gemini".to_string());
        }
    }

    // Opaque blobs upstream cannot classify are stored under the generic
    // "sealed" type (`detectSignatureType` residual branch).
    (trimmed.as_bytes().to_vec(), "sealed".to_string())
}
