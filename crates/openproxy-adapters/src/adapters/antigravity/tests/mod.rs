use super::*;
use serde_json::json;

mod adversarial_tests;

#[test]
fn test_map_antigravity_physical_model() {
    assert_eq!(
        map_antigravity_physical_model("gemini-3.1-pro-high"),
        "gemini-pro-agent"
    );
    assert_eq!(
        map_antigravity_physical_model("gemini-3.1-pro-medium"),
        "gemini-pro-agent"
    );
    assert_eq!(
        map_antigravity_physical_model("gemini-3.5-flash-high"),
        "gemini-3-flash-agent"
    );
    assert_eq!(
        map_antigravity_physical_model("gemini-1.5-pro"),
        "gemini-1.5-pro"
    );
    assert_eq!(
        map_antigravity_physical_model("custom-model"),
        "custom-model"
    );
}

#[test]
fn test_parse_models_response_valid() {
    let body = json!({
        "models": {
            "gemini-1.5-pro": {
                "displayName": "Gemini 1.5 Pro",
                "maxTokens": 1000000,
                "maxOutputTokens": 8192,
                "supportsThinking": true,
                "supportsImages": true,
                "supportsCodeGeneration": true
            },
            "gemini-1.5-flash": {
                "displayName": "Gemini 1.5 Flash",
                "contextLength": 200000,
                "supportsThinking": false
            }
        }
    });

    let models = AntigravityAdapter::parse_models_response(&body).expect("should parse");
    assert_eq!(models.len(), 2);

    let pro_model = models
        .iter()
        .find(|m| m.model_id.as_str() == "gemini-1.5-pro")
        .unwrap();
    assert_eq!(pro_model.display_name.as_deref(), Some("Gemini 1.5 Pro"));
    assert_eq!(pro_model.context_length, Some(1000000));
    assert_eq!(pro_model.max_output_tokens, Some(8192));
    let caps = pro_model.capabilities.as_ref().unwrap();
    assert_eq!(caps.thinking, Some(true));
    assert_eq!(caps.vision, Some(true));

    let flash_model = models
        .iter()
        .find(|m| m.model_id.as_str() == "gemini-1.5-flash")
        .unwrap();
    assert_eq!(
        flash_model.display_name.as_deref(),
        Some("Gemini 1.5 Flash")
    );
    assert_eq!(flash_model.context_length, Some(200000));
    assert_eq!(flash_model.max_output_tokens, Some(8192));
    let flash_caps = flash_model.capabilities.as_ref().unwrap();
    assert_eq!(flash_caps.thinking, Some(false));
}

#[test]
fn test_parse_models_response_invalid() {
    let body = json!({});
    assert!(AntigravityAdapter::parse_models_response(&body).is_none());
}

#[test]
fn test_parse_antigravity_models_response_missing_models() {
    let body = json!({});
    let result = parse_antigravity_models_response(&body);
    assert!(result.is_err());
}

#[test]
fn test_parse_antigravity_models_response_valid() {
    let body = json!({
        "models": {
            "model-a": {
                "quotaInfo": {
                    "resetTime": "2023-01-01T00:00:00Z",
                    "remainingFraction": 0.5
                }
            },
            "model-b": {
                "quotaInfo": {
                    "resetTime": "2023-01-01T00:00:00Z",
                    "remainingFraction": 0.2
                }
            }
        }
    });

    let result = parse_antigravity_models_response(&body).expect("should parse");
    assert_eq!(result.plan_name.unwrap(), "Antigravity");
    assert_eq!(result.session_limit.unwrap(), 1000);
    assert_eq!(result.session_used.unwrap(), 800);

    let details = result.model_details.unwrap();
    assert_eq!(details.len(), 2);
}

#[test]
fn test_inject_sentinel_thought_signatures_flash() {
    let mut contents = json!([
        {
            "role": "model",
            "parts": [
                {
                    "functionCall": {
                        "name": "get_weather",
                        "args": {}
                    }
                }
            ]
        }
    ]);

    tokens::inject_sentinel_thought_signatures(&mut contents, "gemini-3.7-flash-high");
    let parts = contents[0]["parts"].as_array().expect("parts array");
    assert_eq!(parts.len(), 2, "must prepend placeholder thought block before functionCall");
    
    // Part 0: prepended placeholder thought
    assert_eq!(parts[0]["thought"], true);
    assert_eq!(parts[0]["text"], "...");
    assert_eq!(parts[0]["thoughtSignature"], "skip_thought_signature_validator");

    // Part 1: functionCall with sentinel signature and cleaned snake_case
    assert_eq!(
        parts[1]["thoughtSignature"],
        "skip_thought_signature_validator"
    );
    assert!(
        parts[1].get("thought_signature").is_none(),
        "snake_case thought_signature must be purged for Google Cloud Code API"
    );
}

#[test]
fn test_parse_antigravity_user_quota_summary_missing_groups() {
    let body = json!({});
    let result = parse_antigravity_user_quota_summary(&body);
    assert!(result.is_err());
}

#[test]
fn test_parse_antigravity_user_quota_summary_valid() {
    let body = json!({
        "groups": [{
            "displayName": "Pro",
            "buckets": [{
                "window": "WEEK",
                "resetTime": "2023-01-01T00:00:00Z",
                "remainingFraction": 0.5
            }, {
                "window": "DAY",
                "resetTime": "2023-01-01T00:00:00Z",
                "remainingFraction": 0.8
            }]
        }]
    });

    let result = parse_antigravity_user_quota_summary(&body).expect("should parse");
    assert_eq!(result.plan_name.unwrap(), "Pro");
    assert_eq!(result.weekly_limit.unwrap(), 1000);
    assert_eq!(result.weekly_used.unwrap(), 500);
    assert_eq!(result.session_limit.unwrap(), 1000);
    assert_eq!(result.session_used.unwrap(), 200);
}

#[test]
fn normalize_quota_fraction_unlimited() {
    assert_eq!(normalize_quota_fraction(None, Some(1.0)), (0, true));
}

#[test]
fn normalize_quota_fraction_with_reset_no_fraction() {
    assert_eq!(
        normalize_quota_fraction(Some("2023-01-01T00:00:00Z"), None),
        (1000, false)
    );
}

#[test]
fn normalize_quota_fraction_no_reset_with_fraction() {
    assert_eq!(normalize_quota_fraction(None, Some(0.5)), (500, false));
}

#[test]
fn normalize_quota_fraction_with_reset_with_fraction() {
    assert_eq!(
        normalize_quota_fraction(Some("2023-01-01T00:00:00Z"), Some(0.3)),
        (700, false)
    );
}

#[test]
fn count_tokens_wraps_request_only_no_project() {
    let inner = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}]
    });
    let wrapped = serde_json::json!({ "request": inner });

    assert_eq!(wrapped.get("project"), None);
    assert_eq!(wrapped.get("model"), None);
    assert_eq!(wrapped.get("requestType"), None);
    assert_eq!(wrapped.get("enabledCreditTypes"), None);
    assert_eq!(
        wrapped["request"]["contents"].as_array().map(Vec::len),
        Some(1)
    );
}

#[test]
fn parse_total_tokens_flat() {
    let body = json!({"totalTokens": 42});
    assert_eq!(parse_total_tokens(&body), Some(42));
}

#[test]
fn parse_total_tokens_nested() {
    let body = json!({"response": {"totalTokens": 7}});
    assert_eq!(parse_total_tokens(&body), Some(7));
}

#[test]
fn parse_total_tokens_missing_returns_none() {
    assert_eq!(parse_total_tokens(&json!({})), None);
    assert_eq!(parse_total_tokens(&json!({"response": {}})), None);
    assert_eq!(parse_total_tokens(&json!({"totalTokens": "42"})), None);
}

#[cfg(feature = "upstream-hyper")]
#[tokio::test]
async fn count_tokens_propagates_4xx() {
    use crate::upstream::tests_helper as mock_helper;
    use std::sync::Arc;

    let upstream: Arc<crate::upstream::UpstreamClient> =
        mock_helper::build_mock_upstream_returning_status(401, "auth required").await;
    let res = count_tokens(
        &upstream,
        "fake-token",
        &serde_json::json!({"contents": []}),
    )
    .await;
    let err = res.expect_err("must error on 4xx");
    assert!(
        err.contains("401"),
        "error msg should mention status: {err}"
    );
    assert!(
        err.contains("auth required"),
        "body should be in msg: {err}"
    );
}
