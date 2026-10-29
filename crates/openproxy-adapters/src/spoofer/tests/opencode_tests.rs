use super::*;
use crate::spoofer::opencode::{
    OPENCODE_ID_SUFFIX_LEN, OPENCODE_ID_TOTAL_LEN, OPENCODE_SPOOFING_HEADERS, OPENCODE_TEST_LOCK,
    OpenCodeSpoofer, current_opencode_ua, current_opencode_version, has_valid_opencode_version,
    is_valid_opencode_request_id, is_valid_opencode_session_id, reset_dynamic_opencode_overrides,
    set_dynamic_opencode_extra_header, set_dynamic_opencode_version, translate_session_id,
};
use crate::upstream::UpstreamRequest;
use http::HeaderValue;

pub(super) fn assert_opencode_id(prefix: &str, id: &str) {
    assert!(id.starts_with(prefix), "expected prefix {prefix}, got {id}");
    assert_eq!(
        id.len(),
        OPENCODE_ID_TOTAL_LEN,
        "total id length mismatch: {id}"
    );
    let suffix = &id[prefix.len()..];
    assert_eq!(
        suffix.len(),
        OPENCODE_ID_SUFFIX_LEN,
        "id suffix length mismatch"
    );
    let (hex_part, b62_part) = suffix.split_at(12);
    assert!(
        hex_part
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "first 12 chars of suffix must be lowercase hex: {id}"
    );
    assert!(
        b62_part.chars().all(|c| c.is_ascii_alphanumeric()),
        "trailing 14 chars must be base62 alphanumeric: {id}"
    );
    // Check that adversarial 30-byte UTF-8 inputs with multibyte characters at index 12 do not panic
    let adversarial_session = format!("ses_{}é{}", "a".repeat(11), "a".repeat(13));
    assert_eq!(adversarial_session.len(), 30);
    assert!(!is_valid_opencode_session_id(&adversarial_session));

    let adversarial_request = format!("msg_{}é{}", "a".repeat(11), "a".repeat(13));
    assert_eq!(adversarial_request.len(), 30);
    assert!(!is_valid_opencode_request_id(&adversarial_request));

    let translated = translate_session_id(&adversarial_session, None);
    assert!(is_valid_opencode_session_id(&translated));
    assert_ne!(translated, adversarial_session);

    if prefix == "ses_" {
        assert!(is_valid_opencode_session_id(id), "invalid session id: {id}");
    } else if prefix == "msg_" {
        assert!(is_valid_opencode_request_id(id), "invalid request id: {id}");
    }
}

#[test]
fn test_opencode_spoofer() {
    let _guard = OPENCODE_TEST_LOCK.lock().unwrap();
    reset_dynamic_opencode_overrides();
    let spoofer = OpenCodeSpoofer;
    let headers = spoofer.headers();

    // Static headers preserved as-is.
    for &(k, v) in OPENCODE_SPOOFING_HEADERS {
        if k == "User-Agent" {
            let cur_ua = current_opencode_ua();
            assert!(
                headers
                    .iter()
                    .any(|(name, val)| name == k && val == &cur_ua),
                "missing dynamic header {k}={cur_ua}"
            );
        } else {
            assert!(
                headers.iter().any(|(name, val)| name == k && val == v),
                "missing static header {k}={v}"
            );
        }
    }

    // Dynamic session ID follows ses_ + 12 hex + 14 Base62 chars.
    let session_id = headers
        .iter()
        .find(|(k, _)| k == "x-opencode-session")
        .map(|(_, v)| v.as_str())
        .expect("missing x-opencode-session");
    assert_opencode_id("ses_", session_id);

    // x-opencode-request must NOT be present (not sent by real CLI)
    assert!(
        !headers.iter().any(|(k, _)| k == "x-opencode-request"),
        "x-opencode-request should not be sent"
    );

    // Session aliases must all carry the same value as x-opencode-session
    for alias in [
        "x-opencode-session-id",
        "x-session-affinity",
        "x-session-id",
    ] {
        let alias_val = headers
            .iter()
            .find(|(k, _)| k == alias)
            .map_or_else(|| panic!("missing {alias}"), |(_, v)| v.as_str());
        assert_eq!(
            alias_val, session_id,
            "{alias} must match x-opencode-session"
        );
    }

    // W3C tracing headers must be present
    assert!(headers.iter().any(|(k, _)| k == "b3"), "missing b3 header");
    assert!(
        headers.iter().any(|(k, _)| k == "traceparent"),
        "missing traceparent header"
    );

    // Client flag is fixed to "cli".
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "x-opencode-client" && v == "cli")
    );

    // Two successive calls produce different session IDs.
    let second_headers = spoofer.headers();
    let second_session = second_headers
        .iter()
        .find(|(k, _)| k == "x-opencode-session")
        .map(|(_, v)| v.as_str())
        .expect("missing x-opencode-session");
    assert_ne!(session_id, second_session);
}

#[test]
fn test_opencode_spoofer_apply_to_request() {
    let _guard = OPENCODE_TEST_LOCK.lock().unwrap();
    reset_dynamic_opencode_overrides();
    let spoofer = OpenCodeSpoofer;
    let mut req = UpstreamRequest::get("https://dummy.url");
    spoofer.apply_to_request(&mut req);

    let session_id = req
        .headers
        .get("x-opencode-session")
        .expect("missing dynamic x-opencode-session");
    assert_opencode_id("ses_", session_id.to_str().expect("session id not ascii"));

    // x-opencode-request must NOT be present
    assert!(req.headers.get("x-opencode-request").is_none());

    // Session aliases present with same value
    let session_val = session_id.to_str().unwrap();
    for alias in [
        "x-opencode-session-id",
        "x-session-affinity",
        "x-session-id",
    ] {
        assert_eq!(
            req.headers.get(alias).map(|v| v.to_str().unwrap_or("")),
            Some(session_val),
            "missing or wrong {alias}"
        );
    }

    // W3C tracing headers present
    assert!(req.headers.get("b3").is_some(), "missing b3");
    assert!(
        req.headers.get("traceparent").is_some(),
        "missing traceparent"
    );

    assert_eq!(
        req.headers.get("x-opencode-client").unwrap(),
        HeaderValue::from_str("cli").unwrap()
    );
    assert_eq!(
        req.headers.get("x-opencode-project").unwrap(),
        HeaderValue::from_str("global").unwrap()
    );
    assert_eq!(
        req.headers.get("User-Agent").unwrap(),
        current_opencode_ua().as_str()
    );
    assert!(req.headers.get("opencode-version").is_none());
    assert!(req.headers.get("openai-beta").is_none());
}

#[test]
fn test_opencode_translate_session_id() {
    // Preserves already-valid OpenCode session IDs.
    let valid = "ses_f534dfae8ffeCy4Ee4tLWNygDc";
    assert_eq!(translate_session_id(valid, None), valid);
    assert_eq!(translate_session_id(&format!("  {valid}  "), None), valid);

    // Translates foreign / UUID identities deterministically into valid canonical IDs.
    let raw_uuid = "claude:550e8400-e29b-41d4-a716-446655440000";
    let translated1 = translate_session_id(raw_uuid, Some("claude"));
    let translated2 = translate_session_id(raw_uuid, Some("claude"));
    assert_eq!(translated1, translated2);
    assert_opencode_id("ses_", &translated1);

    // Different tools or session inputs produce distinct sessions.
    let different_tool = translate_session_id(raw_uuid, Some("cursor"));
    assert_ne!(translated1, different_tool);
    assert_opencode_id("ses_", &different_tool);
}

#[test]
fn test_opencode_version_validation() {
    assert!(has_valid_opencode_version("opencode/1.18.31"));
    assert!(has_valid_opencode_version("opencode/1.17.0"));
    assert!(has_valid_opencode_version("opencode/1.19.0"));
    assert!(has_valid_opencode_version("opencode/2.0.0"));
    assert!(has_valid_opencode_version(
        "opencode/1.18.31 ai-sdk/provider-utils/4.0.40 runtime/bun/1.3.14"
    ));

    // Outdated (< 1.17) or non-OpenCode versions return false.
    assert!(!has_valid_opencode_version("opencode/1.16.9"));
    assert!(!has_valid_opencode_version("opencode/1.15.0"));
    assert!(!has_valid_opencode_version("opencode"));
    assert!(!has_valid_opencode_version("Claude-Code/1.0"));
    assert!(!has_valid_opencode_version("curl/7.68.0"));
}

#[test]
fn test_opencode_apply_to_header_map_upgrades_and_preserves() {
    let _guard = OPENCODE_TEST_LOCK.lock().unwrap();
    reset_dynamic_opencode_overrides();
    let spoofer = OpenCodeSpoofer;

    // Upgrades foreign User-Agent and translates foreign session.
    let mut req1 = UpstreamRequest::get("https://dummy.url");
    req1.headers.insert(
        http::header::USER_AGENT,
        HeaderValue::from_static("Claude-Code/1.0"),
    );
    req1.headers.insert(
        http::header::HeaderName::from_static("x-opencode-session"),
        HeaderValue::from_static("uuid-1234-5678"),
    );
    spoofer.apply_to_request(&mut req1);

    assert_eq!(
        req1.headers.get(http::header::USER_AGENT).unwrap(),
        current_opencode_ua().as_str()
    );
    let session1 = req1
        .headers
        .get("x-opencode-session")
        .unwrap()
        .to_str()
        .unwrap();
    assert_opencode_id("ses_", session1);
    assert_ne!(session1, "uuid-1234-5678");

    // Preserves authentic downstream OpenCode User-Agent and valid session.
    let mut req2 = UpstreamRequest::get("https://dummy.url");
    req2.headers.insert(
        http::header::USER_AGENT,
        HeaderValue::from_static("opencode/1.19.0 custom-flag"),
    );
    let valid_session = "ses_f534dfae8ffeCy4Ee4tLWNygDc";
    req2.headers.insert(
        http::header::HeaderName::from_static("x-opencode-session"),
        HeaderValue::from_static(valid_session),
    );
    spoofer.apply_to_request(&mut req2);

    assert_eq!(
        req2.headers.get(http::header::USER_AGENT).unwrap(),
        HeaderValue::from_static("opencode/1.19.0 custom-flag")
    );
    assert_eq!(
        req2.headers.get("x-opencode-session").unwrap(),
        HeaderValue::from_static(valid_session)
    );

    // Translates candidate x-session-id when x-opencode-session is absent.
    let mut req3 = UpstreamRequest::get("https://dummy.url");
    req3.headers.insert(
        http::header::HeaderName::from_static("x-session-id"),
        HeaderValue::from_static("foreign-session-abc"),
    );
    spoofer.apply_to_request(&mut req3);
    let session3 = req3
        .headers
        .get("x-opencode-session")
        .unwrap()
        .to_str()
        .unwrap();
    assert_opencode_id("ses_", session3);
    assert_eq!(session3, translate_session_id("foreign-session-abc", None));
}

#[test]
fn test_opencode_dynamic_version_and_extra_headers() {
    let _guard = OPENCODE_TEST_LOCK.lock().unwrap();
    reset_dynamic_opencode_overrides();

    assert_eq!(current_opencode_version(), "2.0.22");
    assert_eq!(current_opencode_ua(), "opencode/latest/2.0.22/cli");

    set_dynamic_opencode_version("1.25.4");
    assert_eq!(current_opencode_version(), "1.25.4");
    assert_eq!(current_opencode_ua(), "opencode/latest/1.25.4/cli");

    set_dynamic_opencode_extra_header("x-opencode-canary", "alpha-3");
    let headers = OpenCodeSpoofer.headers();
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "User-Agent" && v == "opencode/latest/1.25.4/cli")
    );
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "x-opencode-canary" && v == "alpha-3")
    );

    let mut req = UpstreamRequest::get("https://dummy.url");
    OpenCodeSpoofer.apply_to_request(&mut req);
    assert_eq!(
        req.headers.get("User-Agent").unwrap(),
        HeaderValue::from_static("opencode/latest/1.25.4/cli")
    );
    assert_eq!(
        req.headers.get("x-opencode-canary").unwrap(),
        HeaderValue::from_static("alpha-3")
    );

    reset_dynamic_opencode_overrides();
    assert_eq!(current_opencode_version(), "2.0.22");
}
