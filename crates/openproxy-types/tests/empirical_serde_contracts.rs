//! Empirical verification test suite for Milestone 1 serde contracts and consolidated structs.
#![allow(clippy::unwrap_used)]

use openproxy_types::combos::AddTargetInput;
use openproxy_types::error::{CoreError, OptionExt, ResultExt};
use openproxy_types::ids::{AccountId, ComboId, ModelRowId, ProviderId};
use openproxy_types::images::{HordeAsyncSubmitResponse, HordeCheckResponse, HordeStatusResponse};
use openproxy_types::oauth::{DeviceAuthorizationResponse, TokenResponse};
use openproxy_types::providers::{AuthType, NewProvider, ProviderFormat, RateLimitScope};

#[test]
fn test_token_response_snake_case_deserialization() {
    let raw = r#"{
        "access_token": "ya29.a0AfH6SM...",
        "token_type": "Bearer",
        "expires_in": 3599,
        "refresh_token": "1//0e...",
        "scope": "https://www.googleapis.com/auth/cloud-platform",
        "id_token": "eyJhbGciOi..."
    }"#;

    let resp: TokenResponse = serde_json::from_str(raw).expect("deserialize snake_case token");
    assert_eq!(resp.access_token, "ya29.a0AfH6SM...");
    assert_eq!(resp.token_type, "Bearer");
    assert_eq!(resp.expires_in, Some(3599));
    assert_eq!(resp.refresh_token.as_deref(), Some("1//0e..."));
    assert_eq!(
        resp.scope.as_deref(),
        Some("https://www.googleapis.com/auth/cloud-platform")
    );
    assert_eq!(resp.id_token.as_deref(), Some("eyJhbGciOi..."));
}

#[test]
fn test_token_response_camel_case_deserialization() {
    let raw = r#"{
        "accessToken": "eyJhbGciOi...",
        "tokenType": "Bearer",
        "expiresIn": 7200,
        "refreshToken": "refresh-token-xyz",
        "scope": "read:models",
        "idToken": "jwt-token-abc"
    }"#;

    let resp: TokenResponse = serde_json::from_str(raw).expect("deserialize camelCase token");
    assert_eq!(resp.access_token, "eyJhbGciOi...");
    assert_eq!(resp.token_type, "Bearer");
    assert_eq!(resp.expires_in, Some(7200));
    assert_eq!(resp.refresh_token.as_deref(), Some("refresh-token-xyz"));
    assert_eq!(resp.scope.as_deref(), Some("read:models"));
    assert_eq!(resp.id_token.as_deref(), Some("jwt-token-abc"));
}

#[test]
fn test_token_response_mixed_case_and_defaults() {
    // access_token in snake, expiresIn in camel, missing others
    let raw = r#"{
        "access_token": "token-123",
        "expiresIn": 1800
    }"#;

    let resp: TokenResponse = serde_json::from_str(raw).expect("deserialize mixed token");
    assert_eq!(resp.access_token, "token-123");
    assert_eq!(resp.token_type, "");
    assert_eq!(resp.expires_in, Some(1800));
    assert_eq!(resp.refresh_token, None);
    assert_eq!(resp.scope, None);
    assert_eq!(resp.id_token, None);
}

#[test]
fn test_token_response_null_optional_fields() {
    let raw = r#"{
        "access_token": "tok_null",
        "expires_in": null,
        "refresh_token": null,
        "scope": null,
        "id_token": null
    }"#;
    let resp: TokenResponse = serde_json::from_str(raw).expect("deserialize null optional fields");
    assert_eq!(resp.access_token, "tok_null");
    assert_eq!(resp.token_type, "");
    assert_eq!(resp.expires_in, None);
    assert_eq!(resp.refresh_token, None);
    assert_eq!(resp.scope, None);
    assert_eq!(resp.id_token, None);
}

#[test]
fn test_token_response_serialization_and_roundtrip() {
    let original = TokenResponse {
        access_token: "tok_abc".to_string(),
        token_type: "Bearer".to_string(),
        expires_in: Some(3600),
        refresh_token: Some("tok_ref".to_string()),
        scope: Some("user".to_string()),
        id_token: Some("tok_id".to_string()),
    };

    let serialized = serde_json::to_string(&original).expect("serialize token response");
    assert!(serialized.contains(r#""access_token":"tok_abc""#));
    assert!(serialized.contains(r#""token_type":"Bearer""#));
    assert!(serialized.contains(r#""expires_in":3600"#));

    let deserialized: TokenResponse =
        serde_json::from_str(&serialized).expect("roundtrip token response");
    assert_eq!(original, deserialized);
}

#[test]
fn test_token_response_extra_fields_ignored() {
    let raw = r#"{
        "access_token": "tok_1",
        "unknown_claim": "ignored_value",
        "other_metric": 42
    }"#;
    let resp: TokenResponse = serde_json::from_str(raw).expect("ignore unknown fields");
    assert_eq!(resp.access_token, "tok_1");
}

#[test]
fn test_device_authorization_response_camel_case() {
    let raw = r#"{
        "deviceCode": "gm_dev_12345",
        "userCode": "WDJB-MJHT",
        "verificationUri": "https://www.google.com/device",
        "verificationUriComplete": "https://www.google.com/device?user_code=WDJB-MJHT",
        "expiresIn": 1800,
        "interval": 5
    }"#;

    let resp: DeviceAuthorizationResponse =
        serde_json::from_str(raw).expect("deserialize camelCase device auth");
    assert_eq!(resp.device_code, "gm_dev_12345");
    assert_eq!(resp.user_code, "WDJB-MJHT");
    assert_eq!(resp.verification_uri, "https://www.google.com/device");
    assert_eq!(
        resp.verification_uri_complete.as_deref(),
        Some("https://www.google.com/device?user_code=WDJB-MJHT")
    );
    assert_eq!(resp.expires_in, Some(1800));
    assert_eq!(resp.interval, Some(5));
}

#[test]
fn test_device_authorization_response_snake_case() {
    let raw = r#"{
        "device_code": "dev_999",
        "user_code": "ABCD-1234",
        "verification_uri": "https://github.com/login/device",
        "verification_uri_complete": "https://github.com/login/device?code=ABCD-1234",
        "expires_in": 900,
        "interval": 10
    }"#;

    let resp: DeviceAuthorizationResponse =
        serde_json::from_str(raw).expect("deserialize snake_case device auth");
    assert_eq!(resp.device_code, "dev_999");
    assert_eq!(resp.user_code, "ABCD-1234");
    assert_eq!(resp.verification_uri, "https://github.com/login/device");
    assert_eq!(
        resp.verification_uri_complete.as_deref(),
        Some("https://github.com/login/device?code=ABCD-1234")
    );
    assert_eq!(resp.expires_in, Some(900));
    assert_eq!(resp.interval, Some(10));
}

#[test]
fn test_device_authorization_response_minimal_and_roundtrip() {
    let raw = r#"{
        "deviceCode": "dc_min",
        "userCode": "uc_min",
        "verificationUri": "https://auth.com"
    }"#;

    let resp: DeviceAuthorizationResponse =
        serde_json::from_str(raw).expect("deserialize minimal device auth");
    assert_eq!(resp.device_code, "dc_min");
    assert_eq!(resp.user_code, "uc_min");
    assert_eq!(resp.verification_uri, "https://auth.com");
    assert_eq!(resp.verification_uri_complete, None);
    assert_eq!(resp.expires_in, None);
    assert_eq!(resp.interval, None);

    let serialized = serde_json::to_string(&resp).expect("serialize device auth");
    let deserialized: DeviceAuthorizationResponse =
        serde_json::from_str(&serialized).expect("roundtrip device auth");
    assert_eq!(resp, deserialized);
}

#[test]
fn test_add_target_input_flat_and_subcombo_serde() {
    let flat_target = AddTargetInput {
        combo_id: ComboId::from(1),
        provider_id: ProviderId::from("openai"),
        account_id: Some(AccountId::from(10)),
        model_row_id: Some(ModelRowId::from(42)),
        sub_combo_id: None,
        priority_order: 1,
    };

    let flat_json = serde_json::to_string(&flat_target).expect("serialize flat target");
    let flat_roundtrip: AddTargetInput =
        serde_json::from_str(&flat_json).expect("deserialize flat target");
    assert_eq!(flat_target, flat_roundtrip);

    let combo_target = AddTargetInput {
        combo_id: ComboId::from(1),
        provider_id: ProviderId::from("combo"),
        account_id: None,
        model_row_id: None,
        sub_combo_id: Some(ComboId::from(99)),
        priority_order: -5,
    };

    let combo_json = serde_json::to_string(&combo_target).expect("serialize combo target");
    let combo_roundtrip: AddTargetInput =
        serde_json::from_str(&combo_json).expect("deserialize combo target");
    assert_eq!(combo_target, combo_roundtrip);
}

#[test]
fn test_add_target_input_priority_boundaries() {
    for priority in [i32::MIN, -1, 0, 1, i32::MAX] {
        let target = AddTargetInput {
            combo_id: ComboId::from(1),
            provider_id: ProviderId::from("p1"),
            account_id: None,
            model_row_id: Some(ModelRowId::from(1)),
            sub_combo_id: None,
            priority_order: priority,
        };
        let s = serde_json::to_string(&target).unwrap();
        let d: AddTargetInput = serde_json::from_str(&s).unwrap();
        assert_eq!(target.priority_order, d.priority_order);
    }
}

#[test]
fn test_new_provider_construction_and_borrowing() {
    let pid = ProviderId::from("custom-llm");
    let name = "Custom LLM Provider";
    let base_url = "https://api.custom.com/v1";
    let headers = r#"{"Authorization":"Bearer custom"}"#;
    let keyword = "custom-llm";

    let new_p = NewProvider {
        id: &pid,
        name,
        base_url,
        auth_type: AuthType::Bearer,
        format: ProviderFormat::Openai,
        extra_headers_json: Some(headers),
        auto_activate_keyword: Some(keyword),
        rate_limit_scope: RateLimitScope::Account,
    };

    // Verify Copy & Clone
    let copied = new_p;
    assert_eq!(new_p, copied);
    assert_eq!(copied.id.as_str(), "custom-llm");
    assert_eq!(copied.name, "Custom LLM Provider");
    assert_eq!(copied.auth_type, AuthType::Bearer);
    assert_eq!(copied.format, ProviderFormat::Openai);
}

#[test]
fn test_horde_async_submit_response_serde() {
    let empty_json = "{}";
    let empty_resp: HordeAsyncSubmitResponse =
        serde_json::from_str(empty_json).expect("deserialize empty submit");
    assert_eq!(empty_resp.id, None);
    assert_eq!(empty_resp.message, None);
    assert_eq!(empty_resp.error, None);

    let success_json = r#"{
        "id": "c1f7b036-7c09-4f9e-a0f5-566d8e2cf53c",
        "message": "Generation request accepted"
    }"#;
    let success_resp: HordeAsyncSubmitResponse =
        serde_json::from_str(success_json).expect("deserialize success submit");
    assert_eq!(
        success_resp.id.as_deref(),
        Some("c1f7b036-7c09-4f9e-a0f5-566d8e2cf53c")
    );
    assert_eq!(
        success_resp.message.as_deref(),
        Some("Generation request accepted")
    );
    assert_eq!(success_resp.error, None);

    let roundtrip: HordeAsyncSubmitResponse =
        serde_json::from_str(&serde_json::to_string(&success_resp).unwrap()).unwrap();
    assert_eq!(success_resp, roundtrip);
}

#[test]
fn test_horde_check_response_serde() {
    let check_json = r#"{
        "done": false,
        "finished": 0,
        "faulted": false,
        "wait_time": 18,
        "queue_position": 4,
        "kudos": 12.5,
        "is_possible": true
    }"#;
    let resp: HordeCheckResponse =
        serde_json::from_str(check_json).expect("deserialize horde check");
    assert_eq!(resp.done, Some(false));
    assert_eq!(resp.finished, Some(0));
    assert_eq!(resp.faulted, Some(false));
    assert_eq!(resp.wait_time, Some(18));
    assert_eq!(resp.queue_position, Some(4));
    assert_eq!(resp.error, None);

    let roundtrip: HordeCheckResponse =
        serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
    assert_eq!(resp, roundtrip);
}

#[test]
fn test_horde_status_response_with_generations() {
    let status_json = r#"{
        "done": true,
        "faulted": false,
        "generations": [
            {
                "worker_id": "gpu-node-42",
                "worker_name": "RTX-4090-cluster",
                "model": "SDXL_1.0",
                "state": "ok",
                "img": "https://horde.koboldai.net/images/sample.webp",
                "censored": false
            }
        ],
        "shared": false
    }"#;

    let resp: HordeStatusResponse =
        serde_json::from_str(status_json).expect("deserialize horde status");
    assert_eq!(resp.done, Some(true));
    assert_eq!(resp.faulted, Some(false));
    assert_eq!(resp.error, None);

    let gens = resp.generations.as_ref().expect("has generations");
    assert_eq!(gens.len(), 1);
    let item = &gens[0];
    assert_eq!(item.worker_id.as_deref(), Some("gpu-node-42"));
    assert_eq!(item.worker_name.as_deref(), Some("RTX-4090-cluster"));
    assert_eq!(item.model.as_deref(), Some("SDXL_1.0"));
    assert_eq!(item.state.as_deref(), Some("ok"));
    assert_eq!(
        item.img.as_deref(),
        Some("https://horde.koboldai.net/images/sample.webp")
    );
    assert_eq!(item.censored, Some(false));

    let roundtrip: HordeStatusResponse =
        serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
    assert_eq!(resp, roundtrip);
}

#[test]
fn test_result_ext_and_option_ext_empirically() {
    let io_err: std::result::Result<i32, std::io::Error> = Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "file.txt",
    ));

    let core_err = io_err.ctx_not_found("cache entry").unwrap_err();
    match core_err {
        CoreError::NotFound { what, id } => {
            assert_eq!(what, "cache entry");
            assert!(id.contains("file.txt"));
        }
        other => panic!("expected NotFound, got {other:?}"),
    }

    let none_val: Option<String> = None;
    let opt_err = none_val
        .ctx_validation("required header missing")
        .unwrap_err();
    match opt_err {
        CoreError::Validation(msg) => assert_eq!(msg, "required header missing"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[test]
fn test_zai_token_response_and_metadata_serde_contract() {
    // Simulates ZCode token response structure
    let zcode_token_raw = r#"{
        "access_token": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.zcode-jwt",
        "token_type": "Bearer",
        "expires_in": 2592000,
        "id_token": "{\"zcode_jwt_token\":\"eyJ...\",\"zai_access_token\":\"zai-tok-123\",\"user_id\":\"u100\",\"email\":\"dev@z.ai\",\"name\":\"ZCode Developer\"}"
    }"#;

    let token_resp: TokenResponse =
        serde_json::from_str(zcode_token_raw).expect("deserialize zcode token response");
    assert_eq!(
        token_resp.access_token,
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.zcode-jwt"
    );
    assert_eq!(token_resp.token_type, "Bearer");
    assert_eq!(token_resp.expires_in, Some(2592000));

    // Verify metadata deserialization from id_token
    let id_tok = token_resp.id_token.expect("id_token present");
    let meta: serde_json::Value =
        serde_json::from_str(&id_tok).expect("deserialize zai account meta");
    assert_eq!(meta.get("email").and_then(|v| v.as_str()), Some("dev@z.ai"));
    assert_eq!(
        meta.get("zai_access_token").and_then(|v| v.as_str()),
        Some("zai-tok-123")
    );
    assert_eq!(
        meta.get("name").and_then(|v| v.as_str()),
        Some("ZCode Developer")
    );
}
