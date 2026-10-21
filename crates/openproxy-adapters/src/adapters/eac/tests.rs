use super::*;
use openproxy_types::{ModelId, TargetFormat};

#[test]
fn test_rfc4231_hmac_sha256_vector() {
    // RFC 4231 test case 1
    let key = [0x0b; 20];
    let data = b"Hi There";
    let expected_hex = "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7";
    let out = hmac_sha256(&key, data);
    assert_eq!(to_hex_lower(&out), expected_hex);
}

#[test]
fn test_extract_pathname() {
    assert_eq!(
        extract_pathname("https://ourfreemodel.beichenapi.com/eac/v1/chat/completions"),
        "/eac/v1/chat/completions"
    );
    assert_eq!(
        extract_pathname("https://ourfreemodel.beichenapi.com/eac/v1/models?limit=10#frag"),
        "/eac/v1/models"
    );
    assert_eq!(
        extract_pathname("http://localhost:8080/v1/chat/completions"),
        "/v1/chat/completions"
    );
    assert_eq!(extract_pathname("https://ourfreemodel.beichenapi.com"), "/");
    assert_eq!(
        extract_pathname("/eac/v1/chat/completions"),
        "/eac/v1/chat/completions"
    );
}

#[test]
fn test_compute_eac_signature() {
    let secret = DEFAULT_EAC_SECRET;
    let ts = 1_700_000_000_000_u64;
    let method = "POST";
    let pathname = "/eac/v1/chat/completions";
    let body = br#"{"model":"deepseek-ai/deepseek-v4.1-flash","messages":[]}"#;

    let sig1 = compute_eac_signature(secret, ts, method, pathname, body);
    assert_eq!(sig1.len(), 64);
    assert!(
        sig1.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );

    // Deterministic signature
    let sig2 = compute_eac_signature(secret, ts, method, pathname, body);
    assert_eq!(sig1, sig2);

    // Changing any parameter alters the signature
    let sig_diff_ts = compute_eac_signature(secret, ts + 1, method, pathname, body);
    assert_ne!(sig1, sig_diff_ts);

    let sig_diff_method = compute_eac_signature(secret, ts, "GET", pathname, body);
    assert_ne!(sig1, sig_diff_method);

    let sig_diff_path = compute_eac_signature(secret, ts, method, "/eac/v1/other", body);
    assert_ne!(sig1, sig_diff_path);

    let sig_diff_body = compute_eac_signature(secret, ts, method, pathname, b"{}");
    assert_ne!(sig1, sig_diff_body);
}

#[test]
fn test_sign_request_headers_injects_and_overrides() {
    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Authorization".to_string(), "Bearer sk-test".to_string()),
    ];
    let body = b"test payload";
    sign_request_headers(
        DEFAULT_EAC_SECRET,
        "POST",
        "https://ourfreemodel.beichenapi.com/eac/v1/chat/completions",
        &mut headers,
        body,
    );

    let ts = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(HEADER_OFM_TIMESTAMP))
        .map(|(_, v)| v.as_str());
    let sig = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(HEADER_OFM_SIGNATURE))
        .map(|(_, v)| v.as_str());
    let user = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(HEADER_OFM_USER))
        .map(|(_, v)| v.as_str());
    let ua = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
        .map(|(_, v)| v.as_str());

    assert!(ts.is_some(), "x-ofm-timestamp missing");
    assert!(sig.is_some(), "x-ofm-signature missing");
    assert_eq!(user, Some("sk-test"), "x-ofm-user missing or incorrect");
    assert_eq!(
        ua,
        Some(DEFAULT_EAC_USER_AGENT),
        "user-agent missing or incorrect"
    );

    let ts_val = ts.unwrap().parse::<u64>().expect("valid timestamp integer");
    let sig_val = sig.unwrap();
    assert_eq!(sig_val.len(), 64);

    let expected = compute_eac_signature(
        DEFAULT_EAC_SECRET,
        ts_val,
        "POST",
        "/eac/v1/chat/completions",
        body,
    );
    assert_eq!(sig_val, expected);

    // Call again, verify in-place update rather than duplicate headers
    sign_request_headers(
        DEFAULT_EAC_SECRET,
        "POST",
        "https://ourfreemodel.beichenapi.com/eac/v1/chat/completions",
        &mut headers,
        body,
    );
    let count_ts = headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(HEADER_OFM_TIMESTAMP))
        .count();
    let count_sig = headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(HEADER_OFM_SIGNATURE))
        .count();
    let count_user = headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(HEADER_OFM_USER))
        .count();
    let count_ua = headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
        .count();
    assert_eq!(count_ts, 1);
    assert_eq!(count_sig, 1);
    assert_eq!(count_user, 1);
    assert_eq!(count_ua, 1);
}

#[test]
fn test_sign_request_headers_preserves_explicit_user_and_custom_ua() {
    let mut headers = vec![
        ("user-agent".to_string(), "CustomClient/1.0".to_string()),
        ("x-ofm-user".to_string(), "explicit-user-token".to_string()),
        (
            "Authorization".to_string(),
            "Bearer other-token".to_string(),
        ),
    ];
    let body = b"data";
    sign_request_headers(
        DEFAULT_EAC_SECRET,
        "POST",
        "https://ourfreemodel.beichenapi.com/eac/v1/chat/completions",
        &mut headers,
        body,
    );

    let user = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(HEADER_OFM_USER))
        .map(|(_, v)| v.as_str());
    assert_eq!(user, Some("explicit-user-token"));

    let ua = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
        .map(|(_, v)| v.as_str());
    assert_eq!(ua, Some("CustomClient/1.0"));
}

#[test]
fn test_sign_request_headers_custom_secret_header_and_cleanup() {
    let mut headers = vec![
        (
            "x-ofm-secret".to_string(),
            "custom-override-secret".to_string(),
        ),
        ("Authorization".to_string(), "Bearer sk-123".to_string()),
    ];
    let body = b"hello";
    sign_request_headers(
        DEFAULT_EAC_SECRET,
        "POST",
        "https://ourfreemodel.beichenapi.com/eac/v1/chat/completions",
        &mut headers,
        body,
    );

    // Transient secret header must be stripped so it does not leak upstream
    assert!(
        !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("x-ofm-secret"))
    );

    let sig = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(HEADER_OFM_SIGNATURE))
        .unwrap()
        .1
        .clone();
    let ts = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(HEADER_OFM_TIMESTAMP))
        .unwrap()
        .1
        .parse::<u64>()
        .unwrap();

    let expected = compute_eac_signature(
        "custom-override-secret",
        ts,
        "POST",
        "/eac/v1/chat/completions",
        body,
    );
    assert_eq!(sig, expected);
}

#[test]
fn test_eac_adapter_basics() {
    let adapter = EacAdapter::new();
    assert_eq!(adapter.id().as_str(), "eac");
    assert_eq!(adapter.secret(), DEFAULT_EAC_SECRET);
    assert_eq!(adapter.format(), AdapterFormat::Openai);
    assert_eq!(adapter.auth_type(), AdapterAuthType::OAuth);

    let chat_url = adapter.build_chat_url(
        TargetFormat::Openai,
        &ModelId::new("deepseek-ai/deepseek-v4.1-flash"),
    );
    assert_eq!(
        chat_url,
        "https://ourfreemodel.beichenapi.com/eac/v1/chat/completions"
    );

    let models = eac_builtin_models();
    let model_ids: Vec<&str> = models.iter().map(|m| m.model_id.as_str()).collect();
    assert!(model_ids.contains(&"deepseek-ai/deepseek-v4.1-flash"));
    assert!(model_ids.contains(&"moonshotai/kimi-k3"));
    assert!(model_ids.contains(&"z-ai/glm-5.3-flash"));
}
