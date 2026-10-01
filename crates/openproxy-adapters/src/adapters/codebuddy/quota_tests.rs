use super::*;

#[test]
fn test_codebuddy_candidate_origins_default_and_custom() {
    let origins = codebuddy_candidate_origins();
    assert!(origins.contains(&"https://www.codebuddy.ai".to_string()));
    assert!(origins.contains(&"https://www.codebuddy.cn".to_string()));
    assert!(origins.contains(&"https://copilot.tencent.com".to_string()));
    assert_eq!(
        codebuddy_origin_from_base_url("https://www.codebuddy.ai/v2"),
        "https://www.codebuddy.ai"
    );
    assert_eq!(
        codebuddy_origin_from_base_url("https://www.codebuddy.cn/v2/"),
        "https://www.codebuddy.cn"
    );
}

#[test]
fn test_parse_codebuddy_provider_specific() {
    let meta = r#"{"credit_balance":85,"total_credits":100,"provider":"codebuddy"}"#;
    let (bal, tot) = parse_codebuddy_provider_specific(Some(meta));
    assert_eq!(bal, Some(85));
    assert_eq!(tot, Some(100));

    let empty: Option<&str> = None;
    assert_eq!(parse_codebuddy_provider_specific(empty), (None, None));
}

#[test]
fn test_is_codebuddy_auth_error() {
    let json_expired = serde_json::json!({"code": 14015, "msg": "License expired"});
    assert!(is_codebuddy_auth_error(&json_expired));

    let json_forbidden = serde_json::json!({"code": 11140, "msg": "Auth forbidden"});
    assert!(is_codebuddy_auth_error(&json_forbidden));

    let json_msg = serde_json::json!({"code": 500, "msg": "Token expired, please login again"});
    assert!(is_codebuddy_auth_error(&json_msg));

    let json_ok = serde_json::json!({"code": 0, "msg": "OK"});
    assert!(!is_codebuddy_auth_error(&json_ok));
}

#[test]
fn test_build_codebuddy_accounts_request_with_url() {
    let req = build_codebuddy_accounts_request_with_url(
        "https://www.codebuddy.cn/v2/accounts",
        "test-token-cn",
        Some("http://proxy.local:8080"),
    );
    assert_eq!(req.proxy.as_deref(), Some("http://proxy.local:8080"));
    assert_eq!(
        req.headers
            .get(http::header::AUTHORIZATION)
            .unwrap()
            .to_str()
            .unwrap(),
        "Bearer test-token-cn"
    );
    assert_eq!(
        req.headers
            .get("x-no-enterprise-id")
            .unwrap()
            .to_str()
            .unwrap(),
        "true"
    );
}

#[test]
fn test_parse_cst_datetime_to_unix_secs() {
    assert_eq!(
        parse_cst_datetime_to_unix_secs("2025-01-01 00:00:00"),
        Some(1735660800)
    );
    assert_eq!(parse_cst_datetime_to_unix_secs("invalid-date"), None);
}

#[test]
fn test_parse_codebuddy_resource_quota() {
    let payload = serde_json::json!({
        "data": {
            "Response": {
                "Data": {
                    "Accounts": [
                        {
                            "Status": 0,
                            "CapacitySize": 1000,
                            "CapacityRemain": 750,
                            "CapacityUsed": 250,
                            "PackageName": "Pro Package",
                            "CycleEndTime": "2099-01-01 00:00:00"
                        }
                    ]
                }
            }
        }
    });

    let quota = parse_codebuddy_resource_quota(&payload).expect("should parse resource quota");
    assert_eq!(quota.session_limit, Some(1000));
    assert_eq!(quota.session_used, Some(250));
    assert_eq!(
        quota.plan_name.as_deref(),
        Some("CodeBuddy: Pro Package (1000 credits)")
    );
    assert!(quota.model_details.is_some());
}

#[test]
fn test_parse_codebuddy_accounts_quota() {
    let payload = serde_json::json!({
        "data": {
            "accounts": [
                {
                    "pluginEnabled": true,
                    "type": "enterprise",
                    "total_credits": 5000,
                    "credit_balance": 3500
                }
            ]
        }
    });

    let quota = parse_codebuddy_accounts_quota(&payload);
    assert_eq!(quota.session_limit, Some(5000));
    assert_eq!(quota.session_used, Some(1500));
    assert_eq!(quota.plan_name.as_deref(), Some("CodeBuddy Enterprise"));
    assert!(quota.model_details.is_some());
}
