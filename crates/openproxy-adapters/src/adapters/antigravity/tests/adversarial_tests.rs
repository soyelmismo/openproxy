use super::super::{COUNT_TOKENS_URL, normalize_quota_fraction, parse_total_tokens};
use serde_json::json;

// =====================================================================
// ADVERSARIAL TESTS — dedup-antigravity-gemini refactor (D6)
// =====================================================================

#[test]
fn normalize_quota_fraction_both_none_is_unlimited() {
    assert_eq!(normalize_quota_fraction(None, None), (0, true));
}

#[test]
fn normalize_quota_fraction_empty_reset_time_treated_as_some() {
    assert_eq!(normalize_quota_fraction(Some(""), Some(0.5)), (500, false));
}

#[test]
fn normalize_quota_fraction_huge_reset_time_no_panic() {
    let huge = "x".repeat(10 * 1024 * 1024);
    let (used, is_unlimited) = normalize_quota_fraction(Some(&huge), Some(0.5));
    assert_eq!(used, 500);
    assert!(!is_unlimited);
}

#[test]
fn normalize_quota_fraction_nan_fraction_treated_as_unlimited_when_no_reset() {
    let (used, is_unlimited) = normalize_quota_fraction(None, Some(f64::NAN));
    assert!(is_unlimited, "NaN with no reset → 1.0 fraction → unlimited");
    assert_eq!(used, 0);
}

#[test]
fn normalize_quota_fraction_infinity_with_no_reset_is_unlimited() {
    assert_eq!(
        normalize_quota_fraction(None, Some(f64::INFINITY)),
        (0, true)
    );
}

#[test]
fn normalize_quota_fraction_infinity_with_reset_clamped_to_fully_used() {
    let (used, is_unlimited) =
        normalize_quota_fraction(Some("2023-01-01T00:00:00Z"), Some(f64::INFINITY));
    assert!(!is_unlimited);
    assert_eq!(used, 1000, "inf+reset clamps to fully used (no underflow)");
}

#[test]
fn normalize_quota_fraction_negative_zero_treated_as_zero() {
    let (used, is_unlimited) = normalize_quota_fraction(None, Some(-0.0));
    assert!(!is_unlimited);
    assert_eq!(used, 1000);
}

#[test]
fn normalize_quota_fraction_negative_fraction_clamps_to_base() {
    let (used, is_unlimited) = normalize_quota_fraction(None, Some(-0.5));
    assert!(!is_unlimited);
    assert_eq!(used, 1000, "negative fraction clamps to fully used");
}

#[test]
fn normalize_quota_fraction_greater_than_1_with_reset_clamps_to_zero() {
    let (used, is_unlimited) = normalize_quota_fraction(Some("2023-01-01T00:00:00Z"), Some(1.5));
    assert!(!is_unlimited, "reset_time is Some, so never unlimited");
    assert_eq!(used, 0, "fraction > 1.0 with reset clamps used to 0");
}

#[test]
fn normalize_quota_fraction_greater_than_1_no_reset_is_unlimited() {
    let (used, is_unlimited) = normalize_quota_fraction(None, Some(1.5));
    assert!(is_unlimited);
    assert_eq!(used, 0);
}

#[test]
fn normalize_quota_fraction_huge_fraction_is_unlimited() {
    let (used, is_unlimited) = normalize_quota_fraction(None, Some(2f64.powi(53)));
    assert!(is_unlimited);
    assert_eq!(used, 0);
}

#[test]
fn normalize_quota_fraction_negative_infinity_no_reset_is_unlimited() {
    let (used, is_unlimited) = normalize_quota_fraction(None, Some(f64::NEG_INFINITY));
    assert!(is_unlimited, "-inf + no reset → 1.0 fraction → unlimited");
    assert_eq!(used, 0);
}

#[test]
fn normalize_quota_fraction_negative_infinity_with_reset_clamps_to_base() {
    let (used, is_unlimited) =
        normalize_quota_fraction(Some("2023-01-01T00:00:00Z"), Some(f64::NEG_INFINITY));
    assert!(!is_unlimited);
    assert_eq!(used, 1000, "-inf+reset clamps to fully used (no overflow)");
}

#[test]
fn normalize_quota_fraction_zero_fraction_with_reset_fully_used() {
    assert_eq!(
        normalize_quota_fraction(Some("2023-01-01T00:00:00Z"), Some(0.0)),
        (1000, false)
    );
}

#[test]
fn normalize_quota_fraction_zero_fraction_no_reset_fully_used() {
    assert_eq!(normalize_quota_fraction(None, Some(0.0)), (1000, false));
}

#[test]
fn normalize_quota_fraction_exactly_1_with_reset_is_not_unlimited() {
    assert_eq!(
        normalize_quota_fraction(Some("2023-01-01T00:00:00Z"), Some(1.0)),
        (0, false)
    );
}

#[test]
fn normalize_quota_fraction_subnormal_positive_f64_does_not_panic() {
    let (used, is_unlimited) = normalize_quota_fraction(None, Some(5e-324_f64));
    assert!(!is_unlimited);
    assert_eq!(used, 1000);
}

#[test]
fn normalize_quota_fraction_just_below_one_used_almost_full() {
    assert_eq!(
        normalize_quota_fraction(None, Some(0.999_999_999)),
        (1, false)
    );
}

#[test]
fn normalize_quota_fraction_tiny_remaining_truncates_to_zero() {
    assert_eq!(
        normalize_quota_fraction(None, Some(0.000_001)),
        (1000, false)
    );
}

#[test]
fn normalize_quota_fraction_is_pure_idempotent() {
    let args = (Some("2023-01-01T00:00:00Z"), Some(0.7));
    assert_eq!(
        normalize_quota_fraction(args.0, args.1),
        normalize_quota_fraction(args.0, args.1),
    );
}

#[test]
fn normalize_quota_fraction_unicode_reset_time_unaffected() {
    let unicode_reset: &str = "2024-01-01T00:00:00Z \u{1F511}";
    let (used, is_unlimited) = normalize_quota_fraction(Some(unicode_reset), Some(0.5));
    assert_eq!(used, 500);
    assert!(!is_unlimited);
}

#[test]
fn normalize_quota_fraction_clamps_negative_fraction() {
    let (used, is_unlimited) = normalize_quota_fraction(Some("2024-01-01"), Some(-0.5));
    assert_eq!(used, 1000);
    assert!(!is_unlimited);
}

#[test]
fn normalize_quota_fraction_clamps_fraction_greater_than_one_with_reset() {
    let (used, is_unlimited) = normalize_quota_fraction(Some("2024-01-01"), Some(1.5));
    assert_eq!(used, 0);
    assert!(!is_unlimited);
}

#[test]
fn normalize_quota_fraction_treats_nan_as_zero_fraction_with_reset() {
    let (used, is_unlimited) = normalize_quota_fraction(Some("2024-01-01"), Some(f64::NAN));
    assert_eq!(used, 1000);
    assert!(!is_unlimited);
}

// ============================================================
// GAP-3: Adversarial tests for count_tokens / parse_total_tokens
// ============================================================

#[test]
fn adv_parse_total_tokens_negative_value() {
    let body = json!({"totalTokens": -1});
    assert_eq!(parse_total_tokens(&body), Some(-1));
}

#[test]
fn adv_parse_total_tokens_zero_value() {
    let body = json!({"totalTokens": 0});
    assert_eq!(parse_total_tokens(&body), Some(0));
}

#[test]
fn adv_parse_total_tokens_max_i64_value() {
    let body = json!({"totalTokens": i64::MAX});
    assert_eq!(parse_total_tokens(&body), Some(i64::MAX));
}

#[test]
fn adv_parse_total_tokens_overflow_i64_returns_none() {
    let body = json!({"totalTokens": u64::MAX});
    assert_eq!(parse_total_tokens(&body), None);
}

#[test]
fn adv_parse_total_tokens_float_returns_none() {
    let body = json!({"totalTokens": 3.15});
    assert_eq!(parse_total_tokens(&body), None);
}

#[test]
fn adv_parse_total_tokens_string_number_returns_none() {
    let body = json!({"totalTokens": "42"});
    assert_eq!(parse_total_tokens(&body), None);
}

#[test]
fn adv_parse_total_tokens_bool_returns_none() {
    let body = json!({"totalTokens": true});
    assert_eq!(parse_total_tokens(&body), None);
}

#[test]
fn adv_parse_total_tokens_array_returns_none() {
    let body = json!({"totalTokens": [42]});
    assert_eq!(parse_total_tokens(&body), None);
}

#[test]
fn adv_parse_total_tokens_nested_negative() {
    let body = json!({"response": {"totalTokens": -1}});
    assert_eq!(parse_total_tokens(&body), Some(-1));
}

#[test]
fn adv_parse_total_tokens_both_flat_and_nested_prefers_nested() {
    let body = json!({
        "response": {"totalTokens": 7},
        "totalTokens": 100
    });
    assert_eq!(parse_total_tokens(&body), Some(7));
}

#[test]
fn adv_parse_total_tokens_nested_with_wrong_inner_type() {
    let body = json!({"response": {"totalTokens": "7"}});
    assert_eq!(parse_total_tokens(&body), None);
}

#[test]
fn adv_parse_total_tokens_null_response() {
    let body = json!({"response": null});
    assert_eq!(parse_total_tokens(&body), None);
}

#[test]
fn adv_parse_total_tokens_response_is_array() {
    let body = json!({"response": []});
    assert_eq!(parse_total_tokens(&body), None);
}

#[test]
fn adv_wrap_invariants_no_top_level_project() {
    let inner = json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}]
    });
    let wrapped = json!({ "request": inner });
    assert!(wrapped.get("project").is_none());
    assert!(wrapped.get("model").is_none());
    assert!(wrapped.get("requestType").is_none());
    assert!(wrapped.get("enabledCreditTypes").is_none());
    assert!(wrapped.get("userAgent").is_none());
}

#[test]
fn adv_wrap_preserves_nested_request_object() {
    let inner = json!({
        "contents": [
            {"role": "user", "parts": [{"text": "a"}]},
            {"role": "model", "parts": [{"text": "b"}]}
        ]
    });
    let wrapped = json!({ "request": inner });
    let inner_from_wrapped = &wrapped["request"];
    assert_eq!(inner_from_wrapped["contents"].as_array().unwrap().len(), 2);
    assert_eq!(inner_from_wrapped["contents"][0]["parts"][0]["text"], "a");
}

#[test]
fn adv_count_tokens_url_constant() {
    assert_eq!(
        COUNT_TOKENS_URL,
        "https://daily-cloudcode-pa.googleapis.com/v1internal:countTokens"
    );
}
