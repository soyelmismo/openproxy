//! Header redaction helpers.
//!
//! `usage.request_headers` is operator-visible, so persisting
//! caller credentials verbatim would leak them. [`redact_sensitive_headers`]
//! and [`redact_btreemap_sensitive`] are the single source of truth for
//! what counts as sensitive; any path that ingests third-party headers
//! into a row must go through one of them.
//!
//! Sensitive keys (case-insensitive): `authorization` (Bearer tokens),
//! `x-api-key` (de-facto standard), `api-key` (Azure OpenAI),
//! `x-goog-api-key` (Gemini, `AdapterAuthType::GoogApiKey`),
//! `proxy-authorization` (RFC 7617 — would leak the caller's identity),
//! `cookie`, `set-cookie`, `x-auth-token` (Kiro, dashboard frameworks).
//! Everything else is forwarded verbatim. Adding an entry is a one-line
//! change to [`is_sensitive`]; the tests below pin the set.
//!
//! Call sites: `crates/openproxy-server/src/handlers/chat.rs` (header ingress)
//! and `pipeline::dispatch_upstream`. `UsageRecordBuilder` does not redact
//! because the handler is the only path that injects client headers.
use http::HeaderMap;
use std::collections::BTreeMap;

/// Return `true` if `name` (case-insensitive) is a known credential header.
pub fn is_sensitive(name: &str) -> bool {
    match name.len() {
        6 => name.eq_ignore_ascii_case("cookie"),
        7 => name.eq_ignore_ascii_case("api-key"),
        9 => name.eq_ignore_ascii_case("x-api-key"),
        10 => name.eq_ignore_ascii_case("set-cookie"),
        12 => name.eq_ignore_ascii_case("x-auth-token"),
        13 => name.eq_ignore_ascii_case("authorization"),
        14 => name.eq_ignore_ascii_case("x-goog-api-key"),
        19 => name.eq_ignore_ascii_case("proxy-authorization"),
        _ => false,
    }
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
/// Maximum length, in bytes, of a single header value after redaction.
/// The 32 MiB body limit does not bound `HeaderMap` (it is parsed before
/// the body extractor runs), so without a cap a megabyte `User-Agent`
/// inflates `usage.request_headers` past what the admin UI can render.
/// 4 KiB exceeds any legitimate header (longest RFC `User-Agent`: 1 KiB).
pub const REDACTED_HEADER_VALUE_MAX: usize = 4 * 1024;

/// Truncate a header value to [`REDACTED_HEADER_VALUE_MAX`] bytes plus an
/// ellipsis marker, so the dashboard can see the value was cut off.
fn truncate_header_value(v: &str) -> std::borrow::Cow<'_, str> {
    if v.len() <= REDACTED_HEADER_VALUE_MAX {
        std::borrow::Cow::Borrowed(v)
    } else {
        // `char_indices` is byte-accurate; cutting at a non-char
        // boundary would panic on the next `.to_string()`.
        let cut = v
            .char_indices()
            .take_while(|(i, _)| *i < REDACTED_HEADER_VALUE_MAX)
            .last()
            .map_or(0, |(i, c)| i + c.len_utf8());
        let mut s = String::with_capacity(cut + "...[truncated]".len());
        s.push_str(v.get(..cut).unwrap_or(v));
        s.push_str("...[truncated]");
        std::borrow::Cow::Owned(s)
    }
}

/// Project a `HeaderMap` into a `BTreeMap<String, String>`, replacing
/// secret-bearing values with [`REDACTED_PLACEHOLDER`] and capping the rest
/// at [`REDACTED_HEADER_VALUE_MAX`]. Non-ASCII values become `""`, matching
/// the legacy `v.to_str().unwrap_or("")` behaviour of the original call site.
pub fn redact_sensitive_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for k in headers.keys() {
        let key_str = k.as_str();
        let value = if is_sensitive(key_str) {
            REDACTED_PLACEHOLDER.to_string()
        } else if let Some(v) = headers.get_all(k).iter().next_back() {
            // `to_str()` errs on non-ASCII; legacy behaviour dropped those
            // silently (empty string) — keep it, plus the value cap.
            truncate_header_value(v.to_str().unwrap_or("")).into_owned()
        } else {
            String::new()
        };
        out.insert(key_str.to_string(), value);
    }
    out
}

/// BTreeMap-input variant of [`redact_sensitive_headers`] for
/// `pipeline::dispatch_upstream`, which already holds a `BTreeMap` by the
/// time it persists — a `HeaderMap` round-trip would only re-lowercase keys.
/// Keys are never deleted: the dashboard must show WHICH headers were sent.
pub fn redact_btreemap_sensitive(
    mut headers: BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    for (k, v) in &mut headers {
        if is_sensitive(k) {
            v.clear();
            v.push_str(REDACTED_PLACEHOLDER);
        } else if v.len() > REDACTED_HEADER_VALUE_MAX {
            let truncated = truncate_header_value(v).into_owned();
            *v = truncated;
        }
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

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
        // Non-ASCII must be injected as raw bytes to bypass HeaderValue::from_str.
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
        let known_sensitive = [
            "authorization",
            "x-api-key",
            "api-key",
            "x-goog-api-key",
            "cookie",
            "set-cookie",
            "proxy-authorization",
            "x-auth-token",
        ];
        for h in known_sensitive {
            assert!(is_sensitive(h), "{h} should be sensitive");
            assert!(is_sensitive(&h.to_uppercase()), "uppercase {h}");
        }
        assert!(!is_sensitive("content-type"));
        assert!(!is_sensitive("user-agent"));
        assert!(!is_sensitive("x-custom"));
    }

    /// `redact_btreemap_sensitive` must redact mixed-case keys, preserve
    /// non-sensitive entries verbatim, and return a NEW BTreeMap of the same length.
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
        // Length preserved: values are replaced, keys never removed.
        assert_eq!(out.len(), 5);
    }

    // Values are capped at REDACTED_HEADER_VALUE_MAX (4 KiB) on both entry points.

    #[test]
    fn header_value_under_cap_is_preserved() {
        // A normal User-Agent must round-trip untruncated.
        let mut h = HeaderMap::new();
        let ua = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36";
        h.insert("user-agent", HeaderValue::from_str(ua).unwrap());
        let out = redact_sensitive_headers(&h);
        assert_eq!(out.get("user-agent"), Some(&ua.to_string()));
        assert!(!out["user-agent"].ends_with("...[truncated]"));
    }

    #[test]
    fn header_value_over_cap_is_truncated_with_marker() {
        // 64 KiB: 16x the cap, must truncate to the cap plus marker.
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
        // Kept prefix stays within the cap (byte 4096 for ASCII 'x').
        let kept = v.strip_suffix("...[truncated]").unwrap();
        assert!(kept.len() <= REDACTED_HEADER_VALUE_MAX);
        assert!(kept.len() >= REDACTED_HEADER_VALUE_MAX - 4);
    }

    #[test]
    fn multi_byte_header_value_truncation() {
        // 1024 emoji = exactly the cap; 1025 pushes it over.
        let huge = "🚀".repeat(1025);

        let mut map = BTreeMap::new();
        map.insert("user-agent".to_string(), huge);
        let out = redact_btreemap_sensitive(map);
        let v = out.get("user-agent").expect("present");
        assert!(v.ends_with("...[truncated]"));
        let kept = v.strip_suffix("...[truncated]").unwrap();
        assert!(kept.len() <= REDACTED_HEADER_VALUE_MAX);
        // Kept prefix must not end in a partial multi-byte char.
        assert!(String::from_utf8(kept.as_bytes().to_vec()).is_ok());
    }

    #[test]
    fn multi_byte_header_key_handled_safely() {
        let mut btree = BTreeMap::new();
        // Multi-byte key: `is_sensitive` must handle it safely.
        btree.insert("🚀-header".to_string(), "value".to_string());
        let out = redact_btreemap_sensitive(btree);
        assert_eq!(out.get("🚀-header").unwrap(), "value");
    }

    #[test]
    fn empty_header_value_is_handled() {
        let mut h = HeaderMap::new();
        h.insert("user-agent", HeaderValue::from_str("").unwrap());
        let out = redact_sensitive_headers(&h);
        assert_eq!(out.get("user-agent"), Some(&String::new()));
    }

    #[test]
    fn header_keys_case_insensitivity() {
        let mut h = HeaderMap::new();
        h.insert(
            "AUTHORIZATION",
            HeaderValue::from_str("Bearer secret").unwrap(),
        );
        let out = redact_sensitive_headers(&h);
        // axum lowercases HeaderMap keys, but exercise the logic anyway.
        assert_eq!(out.get("authorization").unwrap(), "[REDACTED]");

        let mut btree = BTreeMap::new();
        btree.insert("X-API-KEY".to_string(), "secret".to_string());
        let out2 = redact_btreemap_sensitive(btree);
        assert_eq!(out2.get("X-API-KEY").unwrap(), "[REDACTED]");

        let mut btree2 = BTreeMap::new();
        btree2.insert("x-AuTh-ToKeN".to_string(), "secret".to_string());
        let out3 = redact_btreemap_sensitive(btree2);
        assert_eq!(out3.get("x-AuTh-ToKeN").unwrap(), "[REDACTED]");
    }

    #[test]
    fn btreemap_path_also_caps() {
        // The cap must apply to both entry points or they diverge.
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
