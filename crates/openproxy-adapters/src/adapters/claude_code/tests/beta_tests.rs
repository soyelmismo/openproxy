use super::finalize_tests::*;
use super::*;
#[test]
fn test_betas_from_body_display_updates_drops_redact_thinking() {
    let json = captured_cli_body();
    let betas = body_betas_for("claude-opus-5-5", &json);

    // The display-update gate beta is present exactly once.
    assert_eq!(
        betas
            .iter()
            .filter(|b| *b == cloaking::BETA_THINKING_DISPLAY_UPDATES)
            .count(),
        1,
        "captured cli body must emit the display-update beta once: {betas:?}"
    );
    // Thinking is active, so redact-thinking is withdrawn.
    assert!(
        !betas.iter().any(|b| b == "redact-thinking-2026-02-12"),
        "redact-thinking must be dropped when thinking is active: {betas:?}"
    );
    // The captured base set is preserved verbatim.
    for expected in [
        "claude-code-20250219",
        "oauth-2025-04-20",
        cloaking::BETA_MID_CONV_SYSTEM,
        cloaking::BETA_MID_CONV_TOOL,
        "per-turn-control-2026-07-01",
        "dangerous-tool-use-2026-09-03",
        "afk-mode-2026-01-31",
        "effort-2025-11-24",
    ] {
        assert!(
            betas.iter().any(|b| b == expected),
            "missing {expected}: {betas:?}"
        );
    }
}

#[test]
fn test_betas_from_body_extended_ttl_gate() {
    // cli-00 style: no thinking, no 1h TTL.
    let without_ttl = serde_json::json!({
        "model": "claude-opus-5-5",
        "messages": [],
        "system": [{"type": "text", "text": "x"}],
    });
    let betas = body_betas_for("claude-opus-5-5", &without_ttl);
    assert!(
        !betas.iter().any(|b| b == "extended-cache-ttl-2025-04-11"),
        "no 1h TTL in body means no extended-cache-ttl beta: {betas:?}"
    );

    // A 1h TTL anywhere in the final body enables it.
    let with_ttl = serde_json::json!({
        "model": "claude-opus-5-5",
        "messages": [],
        "system": [{"type": "text", "text": "x", "cache_control": {"type": "ephemeral", "ttl": "1h"}}],
    });
    let betas = body_betas_for("claude-opus-5-5", &with_ttl);
    assert!(
        betas.iter().any(|b| b == "extended-cache-ttl-2025-04-11"),
        "1h TTL in body must add extended-cache-ttl: {betas:?}"
    );

    // A caller-pinned 5m TTL is never promoted by the beta set.
    let five_minutes = serde_json::json!({
        "model": "claude-opus-5-5",
        "messages": [],
        "system": [{"type": "text", "text": "x", "cache_control": {"type": "ephemeral", "ttl": "5m"}}],
    });
    let betas = body_betas_for("claude-opus-5-5", &five_minutes);
    assert!(
        !betas.iter().any(|b| b == "extended-cache-ttl-2025-04-11"),
        "explicit 5m TTL must not gain the 1h TTL beta: {betas:?}"
    );
}

#[test]
fn test_betas_structured_outputs_from_body() {
    // cli-00.json carries an output_config.format json_schema and no thinking.
    let json = serde_json::json!({
        "model": "claude-opus-5-5",
        "messages": [],
        "output_config": {
            "effort": "medium",
            "format": {"type": "json_schema", "schema": {"type": "object"}}
        },
    });
    let betas = body_betas_for("claude-opus-5-5", &json);
    assert!(
        betas.iter().any(|b| b == "structured-outputs-2025-12-15"),
        "structured output body must advertise structured-outputs: {betas:?}"
    );
    // No thinking present, so redact-thinking stays and the display beta is gone.
    assert!(
        betas.iter().any(|b| b == "redact-thinking-2026-02-12"),
        "redact-thinking is the default when thinking is off: {betas:?}"
    );
    assert!(
        !betas
            .iter()
            .any(|b| b == cloaking::BETA_THINKING_DISPLAY_UPDATES),
        "display updates need active updates thinking: {betas:?}"
    );
}

#[test]
fn test_legacy_models_do_not_get_thinking_defaults() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-3-7-sonnet-20250219");
    let raw =
        br#"{"model":"claude-3-7-sonnet-20250219","messages":[{"role":"user","content":"hi"}]}"#;
    let resolved = resolved_for(model.as_str());
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");
    let json: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();

    // Legacy models never receive adaptive thinking or an effort default.
    assert!(
        json.get("thinking").is_none(),
        "legacy model must not gain thinking: {json}"
    );
    assert!(
        json.get("output_config").is_none(),
        "legacy model must not gain effort defaults: {json}"
    );

    let betas = body_betas_for(model.as_str(), &json);
    assert!(
        !betas.iter().any(|b| b == cloaking::BETA_MID_CONV_SYSTEM),
        "legacy model must not get the mid-conversation system beta: {betas:?}"
    );
    assert!(
        !betas
            .iter()
            .any(|b| b == cloaking::BETA_THINKING_DISPLAY_UPDATES),
        "legacy model must never get display updates: {betas:?}"
    );
    assert!(
        !betas.iter().any(|b| b == "effort-2025-11-24"),
        "legacy model must not advertise the effort beta: {betas:?}"
    );
}

#[test]
fn test_thinking_defaults_are_injected_for_supported_models() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}]}"#;
    let resolved = resolved_for(model.as_str());
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");
    let json: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();

    assert_eq!(json["thinking"]["type"], "adaptive");
    assert_eq!(json["thinking"]["display"], "updates");
    assert_eq!(json["output_config"]["effort"], "high");
    assert_eq!(
        json["context_management"]["edits"][0]["type"],
        "clear_thinking_20251015"
    );
    assert_eq!(json["context_management"]["edits"][0]["keep"], "all");
    // max_tokens is never invented here.
    assert!(json.get("max_tokens").is_none());
}

#[test]
fn test_explicit_user_fields_are_never_overwritten() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}],"max_tokens":2048,"thinking":{"type":"disabled"},"output_config":{"effort":"medium"},"context_management":{"edits":[]},"tool_choice":{"type":"tool","name":"GetWeather"}}"#;
    let resolved = resolved_for(model.as_str());
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");
    let json: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();

    assert_eq!(json["max_tokens"], 2048, "caller max_tokens must survive");
    assert_eq!(
        json["thinking"]["type"], "disabled",
        "explicit thinking preserved"
    );
    assert_eq!(
        json["output_config"]["effort"], "medium",
        "explicit effort preserved"
    );
    assert_eq!(
        json["context_management"]["edits"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "empty context management preserved"
    );
    // No active thinking means a tool-pinned tool_choice is not demoted.
    assert_eq!(json["tool_choice"]["type"], "tool");
    assert_eq!(json["tool_choice"]["name"], "GetWeather");
}

#[test]
fn test_cache_ttl_promotion_preserves_explicit_ttl() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}],"system":[{"type":"text","text":"prompt","cache_control":{"type":"ephemeral","ttl":"5m"}}]}"#;
    let resolved = resolved_for(model.as_str());
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");
    let json: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();

    // The injected identity breakpoint carries the native 1h TTL.
    let identity = json["system"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["text"].as_str() == Some(cloaking::CLAUDE_CODE_CLI_IDENTITY))
        .expect("identity block");
    assert_eq!(identity["cache_control"]["type"], "ephemeral");
    assert_eq!(identity["cache_control"]["ttl"], "5m");

    // A caller's explicit 5m TTL on their own prompt block is never promoted.
    let caller_ttl = json["system"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|b| b.get("cache_control").and_then(|c| c.get("ttl")))
        .filter_map(|t| t.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        caller_ttl,
        vec!["5m"],
        "explicit 5m TTL must survive verbatim"
    );
}

#[test]
fn test_finalizer_pins_billing_version_and_identity_headers() {
    let _guard = crate::spoofer::claude_code::CLAUDE_CODE_TEST_LOCK
        .lock()
        .unwrap();
    crate::spoofer::claude_code::reset_dynamic_claude_code_overrides();

    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let resolved = resolved_for(model.as_str());
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}]}"#;
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");

    let mut headers = adapter.build_headers("test-token", TargetFormat::Anthropic, &model);
    // A stale UA from an older profile must be brought back into agreement.
    for (k, v) in &mut headers {
        if k.eq_ignore_ascii_case("user-agent") {
            *v = "claude-cli/2.1.200 (external, cli)".into();
        }
    }

    let finalized = adapter
        .finalize_request(
            wrapped,
            &mut headers,
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("finalize");
    let json: serde_json::Value = serde_json::from_slice(&finalized).unwrap();

    let version = crate::spoofer::claude_code::current_claude_code_version();
    let billing = json["system"][0]["text"].as_str().unwrap();
    assert!(
        billing.starts_with(&format!(
            "x-anthropic-billing-header: cc_version={version}."
        )),
        "billing version must match the pinned snapshot: {billing}"
    );

    let get = |key: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.clone())
    };
    assert_eq!(
        get("user-agent").as_deref(),
        Some(crate::spoofer::claude_code::DEFAULT_CLAUDE_CODE_UA)
    );
    assert_eq!(
        get("x-stainless-package-version").as_deref(),
        Some(crate::spoofer::claude_code::DEFAULT_CLAUDE_CODE_PACKAGE_VERSION)
    );
    assert_eq!(
        get("x-stainless-runtime-version").as_deref(),
        Some(crate::spoofer::claude_code::DEFAULT_CLAUDE_CODE_RUNTIME_VERSION)
    );

    // The body is signed over the final bytes.
    assert_ne!(cch_of(&json), "00000");
    let mut resigned = finalized.to_vec();
    signing::sign_anthropic_messages_body(&mut resigned).expect("re-sign");
    assert_eq!(resigned, finalized.to_vec(), "cch must be stable");
}

#[test]
fn test_finalizer_does_not_invent_identity_headers() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let resolved = resolved_for(model.as_str());
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}]}"#;
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");

    let mut headers = vec![("x-claude-code-session-id".to_string(), "s".to_string())];
    let finalized = adapter
        .finalize_request(
            wrapped,
            &mut headers,
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("finalize");
    let _ = finalized;

    assert!(
        !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("user-agent")),
        "finalizer must not inject an identity header the caller never sent"
    );
    assert_eq!(
        headers.len(),
        2,
        "only session and betas headers remain: {headers:?}"
    );
}

#[test]
fn test_finalizer_betas_derived_from_post_strip_body() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let resolved = resolved_for(model.as_str());
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"adaptive","display":"omitted"}}"#;

    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");

    let mut headers = adapter.build_headers("k", TargetFormat::Anthropic, &model);
    // Simulate the pipeline slotting a provider param that strips right before
    // the finalizer runs.
    let mut body: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    body.as_object_mut()
        .unwrap()
        .insert("temperature".into(), serde_json::json!(0.3));
    let stripped = bytes::Bytes::from(serde_json::to_vec(&body).unwrap());
    let mut stripped: serde_json::Value = serde_json::from_slice(&stripped).unwrap();
    stripped.as_object_mut().unwrap().remove("temperature");
    stripped.as_object_mut().unwrap().remove("fallbacks");
    let stripped = bytes::Bytes::from(serde_json::to_vec(&stripped).unwrap());

    let finalized = adapter
        .finalize_request(
            stripped,
            &mut headers,
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("finalize");
    let json: serde_json::Value = serde_json::from_slice(&finalized).unwrap();

    let betas = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
        .map(|(_, v)| {
            beta_list(v)
                .into_iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .expect("beta header");
    // An explicit omitted display must not advertise update streaming.
    assert!(
        !betas
            .iter()
            .any(|b| b == "thinking-display-updates-2026-08-18"),
        "unexpected display updates beta: {betas:?}"
    );
    assert!(
        !betas.iter().any(|b| b == "redact-thinking-2026-02-12"),
        "thinking is active so redact-thinking must be absent: {betas:?}"
    );
    assert!(
        !betas.iter().any(|b| b == "fallback-credit-2026-06-01"),
        "stale fallback betas must be dropped after strip: {betas:?}"
    );
    assert!(
        json.get("temperature").is_none(),
        "stripped param must not be re-filled after strip"
    );
}

#[test]
fn test_finalizer_reconciles_ua_with_pinned_version() {
    let _guard = crate::spoofer::claude_code::CLAUDE_CODE_TEST_LOCK
        .lock()
        .unwrap();
    crate::spoofer::claude_code::reset_dynamic_claude_code_overrides();
    crate::spoofer::claude_code::set_dynamic_claude_code_version("9.9.999");
    crate::spoofer::claude_code::set_dynamic_claude_code_ua("claude-cli/1.0.0 (external, cli)");

    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let resolved = resolved_for(model.as_str());
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}]}"#;
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");
    let mut headers = adapter.build_headers("k", TargetFormat::Anthropic, &model);

    let finalized = adapter
        .finalize_request(
            wrapped,
            &mut headers,
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("finalize");
    let json: serde_json::Value = serde_json::from_slice(&finalized).unwrap();

    // The UA is left exactly as overridden; it is never rewritten.
    let ua = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
        .map(|(_, v)| v.clone())
        .expect("user-agent present");
    assert_eq!(ua, "claude-cli/1.0.0 (external, cli)");

    // The billing cc_version follows the UA marker, so payload and UA agree
    // even though the version override disagreed.
    let billing = json["system"][0]["text"].as_str().unwrap();
    assert!(
        billing.starts_with("x-anthropic-billing-header: cc_version=1.0.0."),
        "expected UA-derived version 1.0.0, got: {billing}"
    );

    let billing = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-version"))
        .map(|(_, v)| v.clone());
    assert!(
        billing.is_some(),
        "anthropic-version header must survive: {headers:?}"
    );

    crate::spoofer::claude_code::reset_dynamic_claude_code_overrides();
}

#[test]
fn test_finalizer_preserves_explicit_ua_override() {
    let _guard = crate::spoofer::claude_code::CLAUDE_CODE_TEST_LOCK
        .lock()
        .unwrap();
    crate::spoofer::claude_code::reset_dynamic_claude_code_overrides();
    crate::spoofer::claude_code::set_dynamic_claude_code_ua("custom-agent/7.7.7");

    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let resolved = resolved_for(model.as_str());
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}]}"#;
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");
    let mut headers = adapter.build_headers("k", TargetFormat::Anthropic, &model);

    let finalized = adapter
        .finalize_request(
            wrapped,
            &mut headers,
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("finalize");
    let json: serde_json::Value = serde_json::from_slice(&finalized).unwrap();

    let ua = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
        .map(|(_, v)| v.clone())
        .expect("user-agent present");
    // A UA that is not in the CLI's derived shape is an explicit override and
    // is left untouched.
    assert_eq!(ua, "custom-agent/7.7.7");

    // Without a claude-cli marker the profile version still drives billing.
    let billing = json["system"][0]["text"].as_str().unwrap();
    let profile_version = crate::spoofer::claude_code_profile::current_profile().cli_version;
    assert!(
        billing.starts_with(&format!(
            "x-anthropic-billing-header: cc_version={profile_version}."
        )),
        "expected profile version {profile_version}, got: {billing}"
    );

    crate::spoofer::claude_code::reset_dynamic_claude_code_overrides();
}

/// El pin congela UNA generación: headers, body, betas y TTL salen del snapshot
/// de la petición, no del perfil activo del runtime. Así un refresco en
/// paralelo no puede mezclar la versión de los headers con los defaults del
/// body.
#[test]
fn test_pinned_contract_drives_headers_and_body_of_one_generation() {
    use crate::client_contract::{
        Candidate, ClientIdentity, ContractId, IDENTITY_ORIGIN_OFFICIAL, Snapshot,
    };
    use crate::spoofer::claude_code_profile::ClaudeCodeProfile;

    let _guard = crate::spoofer::claude_code::CLAUDE_CODE_TEST_LOCK
        .lock()
        .unwrap();
    crate::spoofer::claude_code::reset_dynamic_claude_code_overrides();

    // Una generación distinta a la activa, y con un TTL que delata la fuga.
    let profile = ClaudeCodeProfile {
        cli_version: "2.1.999".to_string(),
        package_version: "0.999.0".to_string(),
        runtime_version: "v99.0.0".to_string(),
        cache_ttl: "5m".to_string(),
        betas: vec!["claude-code-20250219".to_string()],
        request_defaults: serde_json::json!({
            "thinking": {"type": "adaptive", "display": "updates"},
            "output_config": {"effort": "high"},
            "context_management": {}
        }),
    };
    let candidate = Candidate::without_catalog(
        ContractId::ClaudeCode,
        ClientIdentity::versioned("2.1.999", IDENTITY_ORIGIN_OFFICIAL),
        crate::spoofer::claude_code_profile::CLAUDE_CODE_RUNTIME_RULE_SET_ID,
        crate::spoofer::claude_code_profile::CLAUDE_CODE_RUNTIME_EXTRACTOR_VERSION,
        Vec::new(),
        profile,
    );
    let snapshot = Snapshot::from_candidate(candidate).expect("snapshot de prueba");

    let adapter = ClaudeCodeAdapter::new().with_pinned_contract(Arc::new(snapshot));
    assert!(adapter.pinned_contract().is_some());

    let model = ModelId::new("claude-opus-5-5");
    let resolved = resolved_for(model.as_str());
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}],"system":[{"type":"text","text":"base"}]}"#;
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");
    let mut headers = adapter.build_headers("k", TargetFormat::Anthropic, &model);
    let finalized = adapter
        .finalize_request(
            wrapped,
            &mut headers,
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("finalize");
    let json: serde_json::Value = serde_json::from_slice(&finalized).unwrap();

    // La UA y las versiones stainless son del snapshot, no del runtime.
    let find = |k: &str| {
        headers
            .iter()
            .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.clone())
    };
    assert_eq!(
        find("user-agent").as_deref(),
        Some("claude-cli/2.1.999 (external, cli)")
    );
    assert_eq!(
        find("x-stainless-package-version").as_deref(),
        Some("0.999.0")
    );
    assert_eq!(
        find("x-stainless-runtime-version").as_deref(),
        Some("v99.0.0")
    );

    // El billing viaja con la MISMA versión que los headers.
    let billing = json["system"][0]["text"].as_str().unwrap();
    assert!(
        billing.starts_with("x-anthropic-billing-header: cc_version=2.1.999."),
        "expected pinned version 2.1.999, got: {billing}"
    );

    // Los defaults y el TTL del cache también son de la generación congelada.
    assert_eq!(json["thinking"]["display"], "updates");
    assert_eq!(json["output_config"]["effort"], "high");
    let cc = &json["system"][1]["cache_control"];
    assert_eq!(
        cc["ttl"], "5m",
        "el TTL debe salir del snapshot congelado: {cc}"
    );

    crate::spoofer::claude_code::reset_dynamic_claude_code_overrides();
}
