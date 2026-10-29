use super::*;
use crate::client_contract::providers::codex_runtime::{
    CODEX_RUNTIME_EXTRACTOR_VERSION, CODEX_RUNTIME_RULE_SET_ID, CodexProfile, CodexRequestDefaults,
};
use crate::client_contract::{
    Candidate, ClientIdentity, ContractId, IDENTITY_ORIGIN_OFFICIAL, Snapshot,
};
use crate::spoofer::{CODEX_TEST_LOCK, reset_dynamic_codex_overrides};
use openproxy_types::{ModelId, TargetFormat};
use serde_json::json;

#[test]
fn test_patch_codex_request_object_sanitizes_input_image_parameters() {
    let mut obj = serde_json::json!({
        "model": "gpt-6.1-sol",
        "input": [
            {
                "role": "user",
                "content": [
                    {
                        "type": "input_image",
                        "image": "aGVsbG8=",
                        "mime_type": "image/png"
                    },
                    {
                        "type": "input_text",
                        "text": "what is this?"
                    }
                ]
            }
        ]
    })
    .as_object()
    .unwrap()
    .clone();

    patch_codex_request_object(&mut obj);

    let input = obj["input"].as_array().unwrap();
    let content = input[0]["content"].as_array().unwrap();
    let img_part = &content[0];

    assert_eq!(img_part["type"], "input_image");
    assert_eq!(img_part["image_url"], "data:image/png;base64,aGVsbG8=");
    assert!(
        img_part.get("image").is_none(),
        "image parameter must be stripped for Codex"
    );
    assert!(
        img_part.get("mime_type").is_none(),
        "mime_type parameter must be stripped for Codex"
    );
}

/// Chat Completions nests the JSON schema under `json_schema`; the Responses
/// backend reads `text.format.name` at the top level, so a verbatim copy is
/// rejected with 400 `missing_required_parameter: text.format.name`.
#[test]
fn test_response_format_json_schema_is_flattened_into_text_format() {
    let mut obj = serde_json::json!({
        "model": "gpt-6.1-sol",
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "goal_verdict",
                "strict": true,
                "schema": {
                    "type": "object",
                    "properties": { "met": { "type": "boolean" } },
                    "required": ["met"]
                }
            }
        }
    })
    .as_object()
    .unwrap()
    .clone();

    patch_codex_request_object(&mut obj);

    assert!(
        obj.get("response_format").is_none(),
        "response_format must not survive into the Responses body"
    );
    let format = &obj["text"]["format"];
    assert_eq!(format["type"], "json_schema");
    assert_eq!(
        format["name"], "goal_verdict",
        "name must sit at text.format.name, not under text.format.json_schema.name"
    );
    assert_eq!(format["strict"], true);
    assert_eq!(format["schema"]["type"], "object");
    assert!(
        format.get("json_schema").is_none(),
        "the Chat Completions nesting must be removed"
    );
}

#[test]
fn test_response_format_json_schema_without_name_gets_synthetic_name() {
    let mut obj = serde_json::json!({
        "model": "gpt-6.1-sol",
        "response_format": {
            "type": "json_schema",
            "json_schema": { "schema": { "type": "object" } }
        }
    })
    .as_object()
    .unwrap()
    .clone();

    patch_codex_request_object(&mut obj);

    let format = &obj["text"]["format"];
    assert_eq!(
        format["name"], "response",
        "a missing name is repaired instead of forwarded as a 400"
    );
    assert_eq!(format["schema"]["type"], "object");
}

/// `json_object` does not take a `name`; adding one would be an unsupported
/// parameter. It must pass through byte-for-byte.
#[test]
fn test_response_format_json_object_passes_through_unchanged() {
    let mut obj = serde_json::json!({
        "model": "gpt-6.1-sol",
        "response_format": { "type": "json_object" }
    })
    .as_object()
    .unwrap()
    .clone();

    patch_codex_request_object(&mut obj);

    let format = &obj["text"]["format"];
    assert_eq!(format["type"], "json_object");
    assert!(
        format.get("name").is_none(),
        "json_object must not gain a name parameter"
    );
}

/// A client that already speaks Responses keeps its explicit choice.
#[test]
fn test_existing_text_format_is_not_overwritten() {
    let mut obj = serde_json::json!({
        "model": "gpt-6.1-sol",
        "text": { "format": { "type": "text" }, "verbosity": "low" },
        "response_format": { "type": "json_object" }
    })
    .as_object()
    .unwrap()
    .clone();

    patch_codex_request_object(&mut obj);

    assert_eq!(obj["text"]["format"]["type"], "text");
    assert_eq!(obj["text"]["verbosity"], "low");
    assert!(obj.get("response_format").is_none());
}

/// Un snapshot congelado del contrato fija headers y body de la MISMA
/// generación: la versión y el user-agent salen del snapshot, y los defaults
/// verificados por el pin (`store: false`, `tool_choice: "auto"`) se aplican
/// sin pisar una decisión explícita del cliente.
#[test]
fn test_pinned_contract_drives_headers_and_body_of_one_generation() {
    let profile = CodexProfile {
        version: "0.163.0".to_string(),
        commit: "0000000000000000000000000000000000000000".to_string(),
        release_tag: "rust-v0.163.0".to_string(),
        originator: "codex_cli_rs".to_string(),
        base_url: "https://chatgpt.com/backend-api/codex".to_string(),
        residency_header: "x-openai-internal-codex-residency".to_string(),
        routing_hint_header: "x-codex-routing-hint".to_string(),
        models_endpoint_path: "models".to_string(),
        client_version_query: "client_version".to_string(),
        defaults: CodexRequestDefaults {
            tool_choice: "auto".to_string(),
            store: false,
            stream: true,
        },
        rule_set_id: CODEX_RUNTIME_RULE_SET_ID.to_string(),
        extractor_version: CODEX_RUNTIME_EXTRACTOR_VERSION,
        catalog_models: 11,
    };
    let identity = ClientIdentity::versioned("0.163.0", IDENTITY_ORIGIN_OFFICIAL);
    let candidate = Candidate::without_catalog(
        ContractId::Codex,
        identity,
        CODEX_RUNTIME_RULE_SET_ID,
        CODEX_RUNTIME_EXTRACTOR_VERSION,
        Vec::new(),
        profile,
    );
    let snapshot = Snapshot::from_candidate(candidate).expect("snapshot de prueba");

    let adapter = CodexAdapter::new().with_pinned_contract(Arc::new(snapshot));
    assert!(adapter.pinned_contract().is_some());

    let headers = adapter.build_headers(
        "tok",
        TargetFormat::Responses,
        &ModelId::new("gpt-5.6-luna"),
    );
    let find = |k: &str| {
        headers
            .iter()
            .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(
        find("version"),
        Some("0.163.0"),
        "el header version sale del snapshot congelado"
    );
    assert_eq!(
        find("user-agent"),
        Some("codex-cli/0.163.0 (Windows 10.0.26200; x64)")
    );

    let body = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({"model": "gpt-5.6-luna", "input": []})).unwrap(),
    );
    let resolved_target = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ComboTargetId(1),
            combo_id: openproxy_types::ComboId(1),
            provider_id: openproxy_types::ProviderId::new("codex"),
            model_row_id: Some(openproxy_types::ModelRowId(1)),
            weight: 100,
            ..Default::default()
        },
        model: openproxy_types::Model {
            row_id: openproxy_types::ModelRowId(1),
            provider_id: openproxy_types::ProviderId::new("codex"),
            model_id: openproxy_types::ModelId::new("gpt-5.6-luna"),
            target_format: openproxy_types::TargetFormat::Responses,
            discovered_at: openproxy_types::now_unix_secs_str().into_boxed_str(),
            context_length: Some(272_000),
            max_output_tokens: Some(32_768),
            model_type: "chat".into(),
            ..Default::default()
        },
        api_key: "tok".to_string(),
        ..Default::default()
    };
    let wrapped = adapter
        .wrap_request_body(
            body,
            TargetFormat::Responses,
            &ModelId::new("gpt-5.6-luna"),
            &resolved_target,
        )
        .expect("wrap should succeed");
    let val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    assert_eq!(val["stream"], true);
    assert_eq!(
        val["store"], false,
        "el default verificado por el pin viaja en el body"
    );
    assert_eq!(val["tool_choice"], "auto");
}

/// Sin snapshot congelado el comportamiento es el histórico: la versión la
/// anuncia el spoofer y el body no lleva los defaults del contrato.
#[test]
fn test_without_a_pinned_contract_the_body_keeps_its_shape() {
    let _guard = CODEX_TEST_LOCK.lock().unwrap();
    reset_dynamic_codex_overrides();

    let adapter = CodexAdapter::new();
    assert!(adapter.pinned_contract().is_none());
    let body = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({"model": "gpt-5.6-luna", "input": []})).unwrap(),
    );
    let resolved_target = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ComboTargetId(1),
            combo_id: openproxy_types::ComboId(1),
            provider_id: openproxy_types::ProviderId::new("codex"),
            model_row_id: Some(openproxy_types::ModelRowId(1)),
            weight: 100,
            ..Default::default()
        },
        model: openproxy_types::Model {
            row_id: openproxy_types::ModelRowId(1),
            provider_id: openproxy_types::ProviderId::new("codex"),
            model_id: openproxy_types::ModelId::new("gpt-5.6-luna"),
            target_format: openproxy_types::TargetFormat::Responses,
            discovered_at: openproxy_types::now_unix_secs_str().into_boxed_str(),
            context_length: Some(272_000),
            max_output_tokens: Some(32_768),
            model_type: "chat".into(),
            ..Default::default()
        },
        api_key: "tok".to_string(),
        ..Default::default()
    };
    let wrapped = adapter
        .wrap_request_body(
            body,
            TargetFormat::Responses,
            &ModelId::new("gpt-5.6-luna"),
            &resolved_target,
        )
        .expect("wrap should succeed");
    let val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    assert_eq!(val["stream"], true);
    assert!(
        val.get("store").is_none() && val.get("tool_choice").is_none(),
        "sin contrato congelado no se inyectan defaults"
    );
}

/// El catálogo del snapshot se filtra por lo que el pin publica de verdad:
/// modelos ocultos, no soportados por API o que exigen un cliente más nuevo
/// no se listan.
#[test]
fn test_snapshot_catalog_filters_by_visibility_support_and_minimal_client() {
    let catalog = json!({"models": [
        {"slug": "m-list", "display_name": "L", "priority": 1, "visibility": "list", "supported_in_api": true, "minimal_client_version": "0.150.0"},
        {"slug": "m-hidden", "display_name": "H", "priority": 2, "visibility": "hide", "supported_in_api": true},
        {"slug": "m-old-client", "display_name": "O", "priority": 3, "visibility": "list", "supported_in_api": true, "minimal_client_version": "0.170.0"},
        {"slug": "m-no-api", "display_name": "N", "priority": 4, "visibility": "list", "supported_in_api": false},
        {"slug": "codex-auto-review", "display_name": "R", "priority": 5, "visibility": "list", "supported_in_api": true}
    ]});
    let bytes = serde_json::to_vec(&catalog).unwrap();

    let visible = models::snapshot_catalog_models(&bytes, "0.162.1").unwrap();
    let ids: Vec<&str> = visible.iter().map(|m| m.model_id.as_str()).collect();
    assert_eq!(ids, vec!["m-list"]);

    // El camino sin contrato mantiene su comportamiento histórico: solo
    // excluye el modelo interno de review.
    let unfiltered = models::parse_codex_models_json(&bytes).unwrap();
    assert_eq!(unfiltered.len(), 4);
}

/// El catálogo real del pin, filtrado por la versión del snapshot.
#[test]
fn test_snapshot_catalog_of_the_pinned_fixture_filters_hidden_models() {
    let catalog = include_str!(
        "../../../../../../fixtures/codex-rust-v0.162.1/codex-rs/models-manager/models.json"
    );
    let all = models::parse_codex_models_json(catalog.as_bytes()).unwrap();
    let visible = models::snapshot_catalog_models(catalog.as_bytes(), "0.162.1").unwrap();
    assert_eq!(all.len(), 10, "11 modelos menos el interno de review");
    assert!(
        visible.len() < all.len(),
        "el filtrado real debe descartar modelos ocultos o no publicados"
    );
    assert!(
        !visible
            .iter()
            .any(|m| m.model_id.as_str() == "codex-auto-review")
    );
}
