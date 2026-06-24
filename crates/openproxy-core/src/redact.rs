//! Header / message redaction helpers.
//!
//! Persisting client-side headers to the `usage.request_headers`
//! column (and any other operator-visible surface) would leak the
//! caller's `Authorization: Bearer ***, `Cookie: ...`, and any
//! `X-Api-Key: ...` they sent — an obvious data-leakage bug.
//!
//! This module provides a single helper, [`redact_sensitive_headers`],
//! that takes a `HeaderMap` and returns a `BTreeMap<String, String>`
//! with the secret-bearing headers replaced by the literal string
//! `"[REDACTED]"`. The function is the single source of truth for
//! what counts as "sensitive" — the chat handler uses it before
//! building `PipelineRequest::request_headers`, and any other code
//! path that ingests third-party headers into a row should call
//! it too.
//!
//! ## What is considered sensitive?
//!
//! The list is the standard set of "this header carries a
//! credential" keys, case-insensitive:
//!
//! - `authorization` — `Bearer ...` tokens (OpenAI, Anthropic,
//!   Google, custom proxy tokens).
//! - `x-api-key` — the de-facto standard "API key" header used
//!   by most non-OpenAI upstreams.
//! - `proxy-authorization` — RFC 7617 credential on the proxy
//!   hop; if the operator's reverse-proxy auth is on this header
//!   it would leak the user's identity verbatim.
//! - `cookie` and `set-cookie` — session cookies.
//! - `x-auth-token` — used by a handful of providers (Kiro, etc.)
//!   and by some dashboard frameworks.
//!
//! Anything not on the list is forwarded verbatim, **including
//! the original case** (axum's `HeaderMap` lowercases all keys
//! on insert, so the persisted form is always lowercase). The
//! value is `to_str().unwrap_or("")` — non-ASCII header values
//! become empty strings, matching the previous behavior so a
//! redaction roll-out does not silently alter the wire shape.
//!
//! ## Where this is wired up
//!
//! - `crates/openproxy-server/src/handlers/chat.rs` calls it
//!   when building `PipelineRequest::request_headers`.
//! - The pipeline's `record_attempt_raw_with_tokens` does NOT
//!   call it (the chat handler is the only path that injects
//!   client headers into the BTreeMap, so the redaction at the
//!   ingress point is sufficient).
//!
//! Adding a new "this header is secret" entry is a one-line
//! change to [`SENSITIVE_HEADERS`] below. Tests in
//! [`tests`] (bottom of the file) pin the set so a future
//! refactor that accidentally drops an entry shows up as a
//! failing test rather than a silent credential leak.
use axum::http::HeaderMap;
use std::collections::BTreeMap;

/// Set of header names whose values must never be persisted
/// verbatim. The match is case-insensitive; the keys in this
/// slice are lowercase to make the intent obvious.
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "cookie",
    "set-cookie",
    "proxy-authorization",
    "x-auth-token",
];

/// Return `true` if `name` (case-insensitive) is in
/// [`SENSITIVE_HEADERS`].
pub fn is_sensitive(name: &str) -> bool {
    SENSITIVE_HEADERS
        .iter()
        .any(|sensitive| sensitive.eq_ignore_ascii_case(name))
}

/// The literal value substituted in for any redacted header.
pub const REDACTED_PLACEHOLDER: &str = "[REDACTED]";

/// Project a `HeaderMap` into a `BTreeMap<String, String>` with
/// all secret-bearing header values replaced by
/// [`REDACTED_PLACEHOLDER`].
///
/// Non-ASCII values become `""` (matching the legacy
/// `v.to_str().unwrap_or("")` behavior used at the only
/// historical call site).
///
/// ## Example
///
/// ```ignore
/// use axum::http::HeaderMap;
/// let mut headers = HeaderMap::new();
/// headers.insert("authorization", "Bearer sk-secret".parse().unwrap());
/// headers.insert("content-type", "application/json".parse().unwrap());
/// let out = redact_sensitive_headers(&headers);
/// assert_eq!(out.get("authorization").unwrap(), "[REDACTED]");
/// assert_eq!(out.get("content-type").unwrap(), "application/json");
/// ```
/// Maximum length, in bytes, of a single header value after
/// redaction. axum's request body limit (32 MiB, see
/// `openproxy_server::router`) does not bound the HeaderMap;
/// the HeaderMap is parsed before the body extractor runs, so a
/// client that sends `User-Agent: <megabyte>` makes the
/// BTreeMap grow unbounded and, when `usage.request_headers` is
/// persisted, the database row inflates to a size that the
/// admin UI can no longer display. 4 KiB is generous for any
/// legitimate header (the longest RFC-defined `User-Agent` is
/// 1 KiB; no header an LLM proxy cares about is longer).
pub const REDACTED_HEADER_VALUE_MAX: usize = 4 * 1024;

/// Truncate a header value to [`REDACTED_HEADER_VALUE_MAX`] bytes,
/// appending an ellipsis marker so the dashboard can see the
/// value was cut off.
fn truncate_header_value(v: &str) -> String {
    if v.len() <= REDACTED_HEADER_VALUE_MAX {
        v.to_string()
    } else {
        // `char_indices` is byte-accurate; cutting at a non-char
        // boundary would panic on the next `.to_string()`.
        let cut = v
            .char_indices()
            .take_while(|(i, _)| *i < REDACTED_HEADER_VALUE_MAX)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        let mut s = String::with_capacity(cut + "...[truncated]".len());
        s.push_str(&v[..cut]);
        s.push_str("...[truncated]");
        s
    }
}

pub fn redact_sensitive_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in headers.iter() {
        let value = if is_sensitive(k.as_str()) {
            REDACTED_PLACEHOLDER.to_string()
        } else {
            // `HeaderValue::to_str()` returns Err on non-ASCII
            // bytes; the pre-existing code dropped those silently
            // (empty string). Keep that behaviour but cap the
            // length so a megabyte User-Agent cannot blow up
            // the persisted `usage.request_headers` row.
            truncate_header_value(v.to_str().unwrap_or(""))
        };
        out.insert(k.as_str().to_string(), value);
    }
    out
}

/// BTreeMap-input variant of [`redact_sensitive_headers`].
///
/// `pipeline::dispatch_upstream` builds the request-headers
/// map directly from the upstream client's `HeaderMap` and
/// already has a `BTreeMap<String, String>` by the time
/// it's ready to persist. Going through the `HeaderMap`
/// round-trip just to drop the result into
/// `usage.request_headers` is wasteful and would
/// re-lowercase keys (the `HeaderMap` is case-insensitive
/// but the BTreeMap is canonical lower-case).
///
/// Case-insensitive on the key, replaces the value with
/// [`REDACTED_PLACEHOLDER`] for sensitive entries. Keys
/// are not deleted — the dashboard wants to see WHICH
/// headers were sent, not just the non-sensitive values.
pub fn redact_btreemap_sensitive(headers: BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut out = headers;
    let keys: Vec<String> = out.keys().cloned().collect();
    for k in keys {
        if is_sensitive(&k) {
            out.insert(k, REDACTED_PLACEHOLDER.to_string());
        } else {
            // Mirror the cap in `redact_sensitive_headers` so both
            // entry points produce the same shape. Without this,
            // the `dispatch_upstream` path (which builds the map
            // upstream-side) could still write a megabyte value
            // while the chat-handler path was capped.
            if let Some(v) = out.get(&k).cloned() {
                let capped = truncate_header_value(&v);
                if capped != v {
                    out.insert(k, capped);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn hmap(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        m
    }

    #[test]
    fn authorization_is_redacted_case_insensitively() {
        for raw in ["authorization", "Authorization", "AUTHORIZATION"] {
            let m = hmap(&[(raw, "Bearer sk-secret123")]);
            let out = redact_sensitive_headers(&m);
            assert_eq!(out.get("authorization").unwrap(), "[REDACTED]");
            assert_eq!(out.len(), 1);
        }
    }

    #[test]
    fn x_api_key_is_redacted() {
        let m = hmap(&[("x-api-key", "plaintext-key")]);
        let out = redact_sensitive_headers(&m);
        assert_eq!(out.get("x-api-key").unwrap(), "[REDACTED]");
    }

    #[test]
    fn cookie_and_set_cookie_are_redacted() {
        let m = hmap(&[
            ("cookie", "session=abc123"),
            ("set-cookie", "session=abc123; HttpOnly"),
        ]);
        let out = redact_sensitive_headers(&m);
        assert_eq!(out.get("cookie").unwrap(), "[REDACTED]");
        assert_eq!(out.get("set-cookie").unwrap(), "[REDACTED]");
    }

    #[test]
    fn proxy_authorization_and_x_auth_token_are_redacted() {
        let m = hmap(&[
            ("proxy-authorization", "Basic dXNlcjpwYXNz"),
            ("x-auth-token", "bearer-thing"),
        ]);
        let out = redact_sensitive_headers(&m);
        assert_eq!(out.get("proxy-authorization").unwrap(), "[REDACTED]");
        assert_eq!(out.get("x-auth-token").unwrap(), "[REDACTED]");
    }

    #[test]
    fn non_sensitive_headers_are_passed_through() {
        let m = hmap(&[
            ("authorization", "Bearer sk-secret123"),
            ("content-type", "application/json"),
            ("user-agent", "openproxy-test/0.1"),
            ("x-request-id", "abc-123"),
        ]);
        let out = redact_sensitive_headers(&m);
        assert_eq!(out.get("authorization").unwrap(), "[REDACTED]");
        assert_eq!(out.get("content-type").unwrap(), "application/json");
        assert_eq!(out.get("user-agent").unwrap(), "openproxy-test/0.1");
        assert_eq!(out.get("x-request-id").unwrap(), "abc-123");
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn non_ascii_values_become_empty_string() {
        let m = hmap(&[("authorization", "Bearer sk-secret")]);
        // Inject a non-ASCII byte sequence directly to bypass the
        // HeaderValue::from_str validator. axum's HeaderValue
        // accepts ASCII strings; for non-ASCII we have to use
        // `try_from` on bytes.
        let mut m = m;
        m.insert(
            "x-custom",
            HeaderValue::from_bytes(b"\xff\xfe raw-bytes").unwrap(),
        );
        let out = redact_sensitive_headers(&m);
        assert_eq!(out.get("x-custom").unwrap(), "");
    }

    #[test]
    fn empty_headermap_yields_empty_btreemap() {
        let m = HeaderMap::new();
        let out = redact_sensitive_headers(&m);
        assert!(out.is_empty());
    }

    #[test]
    fn is_sensitive_matches_full_list() {
        for h in SENSITIVE_HEADERS {
            assert!(is_sensitive(h), "{h} should be sensitive");
            assert!(is_sensitive(&h.to_uppercase()), "uppercase {h}");
        }
        assert!(!is_sensitive("content-type"));
        assert!(!is_sensitive("user-agent"));
        assert!(!is_sensitive("x-custom"));
    }

    /// C2 helper: `redact_btreemap_sensitive` is the
    /// BTreeMap-input variant used by
    /// `pipeline::dispatch_upstream`. The pipeline call
    /// site depends on:
    /// - mixed-case keys being redacted
    /// - non-sensitive entries being preserved verbatim
    /// - the result being a NEW BTreeMap of the same length
    #[test]
    fn redact_btreemap_sensitive_redacts_known_keys_and_passes_through() {
        let mut input: BTreeMap<String, String> = BTreeMap::new();
        input.insert("authorization".to_string(), "Bearer sk-XYZ".to_string());
        input.insert("Authorization".to_string(), "Bearer sk-MIXED".to_string());
        input.insert("content-type".to_string(), "application/json".to_string());
        input.insert("x-api-key".to_string(), "sk-abc".to_string());
        input.insert("x-request-id".to_string(), "abc-123".to_string());

        let out = redact_btreemap_sensitive(input);
        // All sensitive variants are now [REDACTED].
        assert_eq!(
            out.get("authorization"),
            Some(&REDACTED_PLACEHOLDER.to_string())
        );
        assert_eq!(
            out.get("Authorization"),
            Some(&REDACTED_PLACEHOLDER.to_string())
        );
        assert_eq!(
            out.get("x-api-key"),
            Some(&REDACTED_PLACEHOLDER.to_string())
        );
        // Non-sensitive entries untouched.
        assert_eq!(
            out.get("content-type"),
            Some(&"application/json".to_string())
        );
        assert_eq!(out.get("x-request-id"), Some(&"abc-123".to_string()));
        // Length preserved (we replace, not remove).
        assert_eq!(out.len(), 5);
    }

    // ---- LOW fix: cap header value length to REDACTED_HEADER_VALUE_MAX
    // (4 KiB) so a megabyte User-Agent cannot blow up the persisted
    // usage.request_headers row.

    #[test]
    fn header_value_under_cap_is_preserved() {
        // A normal User-Agent is well under 4 KiB and must round-trip.
        let mut h = HeaderMap::new();
        let ua = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36";
        h.insert("user-agent", HeaderValue::from_str(ua).unwrap());
        let out = redact_sensitive_headers(&h);
        assert_eq!(out.get("user-agent"), Some(&ua.to_string()));
        assert!(!out["user-agent"].ends_with("...[truncated]"));
    }

    #[test]
    fn header_value_over_cap_is_truncated_with_marker() {
        // 64 KiB User-Agent: 62× the cap. Must be truncated to
        // REDACTED_HEADER_VALUE_MAX and the marker appended.
        let mut h = HeaderMap::new();
        let huge = "x".repeat(64 * 1024);
        h.insert("user-agent", HeaderValue::from_str(&huge).unwrap());
        let out = redact_sensitive_headers(&h);
        let v = out.get("user-agent").expect("present");
        assert!(
            v.ends_with("...[truncated]"),
            "truncation marker missing, got len={}",
            v.len()
        );
        // The kept prefix must be at most REDACTED_HEADER_VALUE_MAX
        // bytes (we cut at a char boundary which is byte 4096 for
        // ASCII 'x').
        let kept = v.strip_suffix("...[truncated]").unwrap();
        assert!(kept.len() <= REDACTED_HEADER_VALUE_MAX);
        assert!(kept.len() >= REDACTED_HEADER_VALUE_MAX - 4);
    }

    #[test]
    fn btreemap_path_also_caps() {
        // dispatch_upstream builds the map directly; the cap must
        // be enforced here too or the two entry points diverge.
        use std::collections::BTreeMap;
        let mut h = BTreeMap::new();
        h.insert("user-agent".to_string(), "x".repeat(64 * 1024));
        let out = redact_btreemap_sensitive(h);
        let v = out.get("user-agent").expect("present");
        assert!(v.ends_with("...[truncated]"));
        let kept = v.strip_suffix("...[truncated]").unwrap();
        assert!(kept.len() <= REDACTED_HEADER_VALUE_MAX);
    }
}
