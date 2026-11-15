//! Canonical secret redaction for error messages and logs.
//!
//! Single source of truth for the patterns that scrub credentials out of
//! WARN/ERROR payloads before they reach `usage.error_msg` (SQLite), the HTTP
//! error responses, the SSE error frames, the dashboard ring buffer and
//! `~/.openproxy/debug.log`.
//!
//! Security (audit H-7 + H-8): two divergent copies of this logic existed
//! (`openproxy-pricing::cost` with 4 patterns and `openproxy-db::cost` with
//! 3 — `op_live_` was missing from the DB copy, so the product's own key
//! format leaked into `usage.error_msg` in clear). Neither copy covered the
//! credential formats of the providers this gateway fronts: OAuth/JWT access
//! tokens (`eyJ…`, MiniMax/ZAI/Kiro/Codex/Claude Code), Google API keys
//! (`AIza…`), bare `Bearer <token>` mentions and Azure-style `api-key:`
//! headers. Upstream error bodies are echoed verbatim into error messages,
//! so a provider that reflects the presented token in its 401/400 body could
//! land it in logs unredacted.

use std::sync::LazyLock;

/// Hard cap on a sanitised error message (2 KiB, UTF-8 boundary safe).
pub const SANITIZED_MESSAGE_MAX_BYTES: usize = 2048;

static RE_SK: LazyLock<regex::Regex> =
    LazyLock::new(|| crate::static_regex!(r"sk-[A-Za-z0-9_\-]{10,}"));
static RE_OP_LIVE: LazyLock<regex::Regex> =
    LazyLock::new(|| crate::static_regex!(r"op_live_[A-Za-z0-9]{16,}"));
static RE_XAPIKEY: LazyLock<regex::Regex> =
    LazyLock::new(|| crate::static_regex!(r"(?i)x-api-key:\s*\S+"));
static RE_APIKEY: LazyLock<regex::Regex> =
    LazyLock::new(|| crate::static_regex!(r"(?i)api-key:\s*\S+"));
static RE_AUTH_BEARER: LazyLock<regex::Regex> =
    LazyLock::new(|| crate::static_regex!(r"(?i)authorization:\s*bearer\s+\S+"));
static RE_AUTH_SCHEME: LazyLock<regex::Regex> =
    LazyLock::new(|| crate::static_regex!(r"(?i)authorization:\s*(basic|token|digest|mutual)\s+\S+"));
static RE_AUTH_BARE: LazyLock<regex::Regex> =
    LazyLock::new(|| crate::static_regex!(r"(?i)authorization:\s*[A-Za-z0-9._~+/=-]{16,}"));
static RE_BARE_BEARER: LazyLock<regex::Regex> =
    LazyLock::new(|| crate::static_regex!(r"(?i)bearer\s+[A-Za-z0-9._~+/=-]{16,}"));
static RE_JWT: LazyLock<regex::Regex> = LazyLock::new(|| {
    crate::static_regex!(r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}[A-Za-z0-9_.-]*")
});
static RE_GOOGLE_KEY: LazyLock<regex::Regex> =
    LazyLock::new(|| crate::static_regex!(r"AIza[0-9A-Za-z_-]{30,}"));

/// The full pattern set. Order is irrelevant: each match is replaced with a
/// prefix-preserving placeholder so operators can still tell WHICH kind of
/// credential leaked, without learning its value.
static PATTERNS: &[(&LazyLock<regex::Regex>, &str)] = &[
    // OpenAI-style `sk-…` keys.
    (&RE_SK, "sk-[REDACTED]"),
    // The product's own API keys.
    (&RE_OP_LIVE, "op_live_[REDACTED]"),
    // Anthropic-style header mention.
    (&RE_XAPIKEY, "x-api-key: [REDACTED]"),
    // Azure OpenAI style header mention.
    (&RE_APIKEY, "api-key: [REDACTED]"),
    // `Authorization: Bearer …` keeps its historical replacement string
    // (dashboards and tests grep for it).
    (&RE_AUTH_BEARER, "Authorization: Bearer [REDACTED]"),
    // Other auth schemes, or a bare long credential with no scheme.
    (&RE_AUTH_SCHEME, "Authorization: [REDACTED]"),
    (&RE_AUTH_BARE, "Authorization: [REDACTED]"),
    // Bare `Bearer <credential>` without the header prefix — e.g. an OAuth
    // error body reading "token Bearer eyJ… expired". The token charset and
    // the 16-char floor keep prose like "Bearer tokens" out of the match.
    (&RE_BARE_BEARER, "Bearer [REDACTED]"),
    // JWTs / OAuth access tokens: `eyJ`-prefixed, base64url segments. The
    // signature segment may be empty (`alg: none`) or short, and JWEs carry
    // five segments — the trailing `[A-Za-z0-9_.-]*` keeps the whole token
    // inside one match.
    (&RE_JWT, "eyJ…[REDACTED]"),
    // Google API keys (`AIza` + 35 base64-ish chars).
    (&RE_GOOGLE_KEY, "AIza…[REDACTED]"),
];

/// Apply every redaction pattern to `text`.
pub fn redact_secrets(text: &str) -> String {
    let mut output = text.to_string();
    for (regex, replacement) in PATTERNS {
        if regex.is_match(&output) {
            output = regex.replace_all(&output, *replacement).into_owned();
        }
    }
    output
}

/// Sanitise an error message for persistence or display:
/// 1. redact every known credential format (see [`redact_secrets`]);
/// 2. truncate to [`SANITIZED_MESSAGE_MAX_BYTES`] on a UTF-8 char boundary.
pub fn sanitize_error_message(raw: &str) -> String {
    let redacted = redact_secrets(raw);
    truncate_utf8(&redacted, SANITIZED_MESSAGE_MAX_BYTES)
}

/// Truncate to `max_bytes` on a UTF-8 char boundary, marking the cut.
fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut idx = max_bytes;
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    let mut truncated = String::with_capacity(idx + 14);
    truncated.push_str(&text[..idx]);
    truncated.push_str("...[truncated]");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_every_credential_family() {
        let cases: &[(&str, &str)] = &[
            ("bad key sk-abcdefghijklmnopqrst", "bad key sk-[REDACTED]"),
            ("leak op_live_abcdefghijklmnopqrst", "leak op_live_[REDACTED]"),
            (
                "x-api-key: sk-abcdefghijklmnopqrst",
                "x-api-key: [REDACTED]",
            ),
            ("api-key: 32chars0000000000000000000000", "api-key: [REDACTED]"),
            (
                "Authorization: Bearer eyJhbGciOi.eyJzdWIiOi.c2ln",
                "Authorization: Bearer [REDACTED]",
            ),
            (
                "token Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig expired",
                "token Bearer [REDACTED] expired",
            ),
            (
                "quota check failed for AIzaSyA1234567890abcdefghijklmnopqrstuv",
                "quota check failed for AIza…[REDACTED]",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(&redact_secrets(input), expected, "input: {input}");
        }
    }

    #[test]
    fn redacts_jwt_access_tokens_whole() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let out = redact_secrets(&format!("OAuth refresh failed: token {jwt} expired"));
        assert!(!out.contains(".SflKxw"), "whole token must be redacted: {out}");
        assert!(out.contains("[REDACTED]"), "must carry the marker: {out}");
    }

    #[test]
    fn does_not_redact_prose() {
        let prose = "the Bearer tokens are rotated every hour by the scheduler";
        assert_eq!(redact_secrets(prose), prose);
    }

    #[test]
    fn truncates_on_utf8_boundary() {
        let long = "á".repeat(3000);
        let out = sanitize_error_message(&long);
        assert!(out.len() <= SANITIZED_MESSAGE_MAX_BYTES + 14);
        assert!(out.ends_with("...[truncated]"));
        assert!(out.chars().all(|c| c == 'á' || c.is_ascii()));
    }
}
