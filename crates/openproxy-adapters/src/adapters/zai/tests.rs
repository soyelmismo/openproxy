use super::*;
use openproxy_types::{ModelId, TargetFormat};

#[test]
fn zai_build_chat_url_various_base_urls() {
    let a = ZaiAdapter::new();
    let model = ModelId::new("GLM-5.3");
    assert_eq!(
        a.build_chat_url(TargetFormat::Anthropic, &model),
        "https://api.z.ai/api/anthropic/v1/messages"
    );

    let a_v1 = ZaiAdapter::with_base_url("https://api.z.ai/api/anthropic/v1");
    assert_eq!(
        a_v1.build_chat_url(TargetFormat::Anthropic, &model),
        "https://api.z.ai/api/anthropic/v1/messages"
    );

    let a_msg = ZaiAdapter::with_base_url("https://api.z.ai/api/anthropic/v1/messages");
    assert_eq!(
        a_msg.build_chat_url(TargetFormat::Anthropic, &model),
        "https://api.z.ai/api/anthropic/v1/messages"
    );
}

#[test]
fn zai_build_headers() {
    let a = ZaiAdapter::new();
    let headers = a.build_headers(
        "mock-token-xyz",
        TargetFormat::Anthropic,
        &ModelId::new("GLM-5.3"),
    );

    let find = |key: &str| -> Option<&str> {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find("x-api-key"), Some("mock-token-xyz"));
    assert_eq!(find("Authorization"), Some("Bearer mock-token-xyz"));
    assert_eq!(find("anthropic-version"), Some("2023-06-01"));
    assert_eq!(find("Content-Type"), Some("application/json"));
    assert!(find("User-Agent").unwrap().contains("ZCode/"));
}

#[test]
fn zai_tokens_extraction_from_meta() {
    let raw = r#"{
        "zcode_jwt_token": "jwt-abc-123",
        "business_access_token": "biz-tok-456",
        "zai_access_token": "zai-tok-789"
    }"#;

    let tokens = ZaiTokens::from_provider_specific_and_args("", None, Some(raw));
    assert_eq!(tokens.zcode_jwt_token.as_deref(), Some("jwt-abc-123"));
    assert_eq!(tokens.business_access_token.as_deref(), Some("biz-tok-456"));
    assert_eq!(tokens.zai_access_token.as_deref(), Some("zai-tok-789"));

    // Fallback to api_key
    let tokens2 = ZaiTokens::from_provider_specific_and_args("raw-api-key", None, None);
    assert_eq!(tokens2.zcode_jwt_token.as_deref(), Some("raw-api-key"));
    assert_eq!(
        tokens2.business_access_token.as_deref(),
        Some("raw-api-key")
    );
}

#[test]
fn zai_parse_coding_plan_quota_full() {
    let sub_json = serde_json::json!({
        "code": 200,
        "msg": "Operation successful",
        "data": [
            {
                "id": 8888,
                "productId": "coding-plan-pro",
                "productName": "Z.ai Coding Plan Pro",
                "status": "VALID",
                "inCurrentPeriod": true,
                "periodStartTime": 1726000000000_u64,
                "periodEndTime": 1728600000000_u64
            }
        ],
        "success": true
    });

    let limit_json = serde_json::json!({
        "code": 0,
        "data": {
            "level": "Pro",
            "limits": [
                {
                    "type": "TIME_LIMIT",
                    "number": 100000000,
                    "usage": 250000,
                    "remaining": 99750000,
                    "percentage": 0.9975,
                    "nextResetTime": 1728600000000_u64,
                    "usageDetails": [
                        { "modelCode": "GLM-5.3", "usage": 150000 },
                        { "modelCode": "GLM-5.3-Flash", "usage": 100000 }
                    ]
                }
            ]
        },
        "success": true
    });

    let quota =
        parse_zai_coding_plan_quota(Some(&sub_json), Some(&limit_json)).expect("quota present");

    assert_eq!(quota.plan_name.as_deref(), Some("Z.ai Coding Plan Pro"));
    assert_eq!(quota.session_limit, Some(100000000));
    assert_eq!(quota.session_used, Some(250000));
    assert_eq!(quota.session_reset_at.as_deref(), Some("1728600000"));

    let details = quota.model_details.expect("details present");
    assert_eq!(details.len(), 2);
    assert_eq!(details[0].model_id, "GLM-5.3");
    assert_eq!(details[0].session_used, 150000);
    assert_eq!(details[1].model_id, "GLM-5.3-Flash");
    assert_eq!(details[1].session_used, 100000);
}

#[test]
fn zai_parse_coding_plan_quota_no_active_plan() {
    // Empty subscription list and no limit data
    let sub_json = serde_json::json!({
        "code": 200,
        "msg": "Operation successful",
        "data": [],
        "success": true
    });

    let res = parse_zai_coding_plan_quota(Some(&sub_json), None);
    assert!(res.is_none());
}

#[test]
fn zai_builtin_models_contract() {
    let models = zai_builtin_models();
    assert!(!models.is_empty());

    let has_model = |id: &str| models.iter().any(|m| m.model_id.as_str() == id);
    assert!(has_model("GLM-5.3"));
    assert!(has_model("GLM-5.3-Flash"));
    assert!(has_model("GLM-5.2"));
    assert!(has_model("GLM-5-Turbo"));
    assert!(has_model("GLM-5.1"));
    assert!(has_model("GLM-4.7"));
    assert!(has_model("GLM-4.6"));

    for m in &models {
        assert_eq!(m.target_format, TargetFormat::Anthropic);
        assert!(m.context_length.is_some_and(|l| l >= 128_000));
    }
}

#[test]
fn zai_pick_org_and_project_success() {
    let json = serde_json::json!({
        "organizations": [
            {
                "organizationName": "Non-default Org",
                "organizationId": "org-other",
                "isDefault": false,
                "projects": [
                    {
                        "projectName": "Other Proj",
                        "projectId": "proj-other",
                        "isDefault": true,
                        "projectType": 1
                    }
                ]
            },
            {
                "organizationName": "默认机构(069859)",
                "organizationId": "org-857c428203bE4c22be25c082CAc34ecc",
                "isDefault": true,
                "projects": [
                    {
                        "projectName": "team project",
                        "projectId": "proj-team",
                        "projectType": 2
                    },
                    {
                        "projectName": "默认项目",
                        "projectId": "proj_4050cF9b4bDb4fe98ee6f39Dfcef34b5",
                        "isDefault": true,
                        "projectType": 1
                    }
                ]
            }
        ]
    });

    let res = pick_org_and_project(&json).expect("should pick org and project");
    assert_eq!(res.organization_id, "org-857c428203bE4c22be25c082CAc34ecc");
    assert_eq!(res.project_id, "proj_4050cF9b4bDb4fe98ee6f39Dfcef34b5");
}
