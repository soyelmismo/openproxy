use super::super::super::models::*;
use super::super::super::protoc;
use super::super::super::quota::*;
use super::super::super::wire::*;
use super::super::super::{DEVIN_CHAT_PATH, DevinAdapter};
use super::super::{Fv, as_str, collect, get, msg, req_with};
use super::decode_request;
use crate::adapters::ProviderAdapter;
use bytes::Bytes;
use openproxy_types::OpenAIRequest;
use serde_json::json;

// ---------------------------------------------------------------------------
// user status → quota
// ---------------------------------------------------------------------------

fn golden_user_status() -> Vec<u8> {
    let mut plan = Vec::new();
    protoc::append_string(&mut plan, 2, "Devin Teams");
    let mut org = Vec::new();
    protoc::append_string(&mut org, 4, "org-1");
    protoc::append_string(&mut org, 8, "Acme");
    protoc::append_message(&mut plan, 33, &org);

    let mut ts_start = Vec::new();
    protoc::append_varint_field(&mut ts_start, 1, 1_700_000_000);
    let mut ts_end = Vec::new();
    protoc::append_varint_field(&mut ts_end, 1, 1_800_000_000);

    let mut plan_status = Vec::new();
    protoc::append_message(&mut plan_status, 1, &plan);
    protoc::append_message(&mut plan_status, 2, &ts_start);
    protoc::append_message(&mut plan_status, 3, &ts_end);
    protoc::append_varint_field(&mut plan_status, 14, 73);
    protoc::append_varint_field(&mut plan_status, 15, 40);
    protoc::append_varint_field(&mut plan_status, 17, 1_700_086_400);
    protoc::append_varint_field(&mut plan_status, 18, 1_700_604_800);

    let mut us = Vec::new();
    protoc::append_string(&mut us, 3, "devin-user");
    protoc::append_string(&mut us, 5, "team-1");
    protoc::append_string(&mut us, 7, "u@x.io");
    protoc::append_message(&mut us, 13, &plan_status);
    protoc::append_string(&mut us, 36, "uid-1");

    let mut body = Vec::new();
    protoc::append_message(&mut body, 1, &us);
    body
}

#[test]
fn user_status_parse_to_quota() {
    let body = golden_user_status();
    let us = parse_user_status(&body).unwrap();
    assert_eq!(us.user_name, "devin-user");
    assert_eq!(us.team_id, "team-1");
    assert_eq!(us.email, "u@x.io");
    assert_eq!(us.user_id, "uid-1");
    assert_eq!(us.plan_name, "Devin Teams");
    assert_eq!(us.org_id, "org-1");
    assert_eq!(us.org_name, "Acme");
    assert_eq!(us.plan_start_secs, Some(1_700_000_000));
    assert_eq!(us.plan_end_secs, Some(1_800_000_000));
    assert_eq!(us.daily_remaining_pct, Some(73));
    assert_eq!(us.weekly_remaining_pct, Some(40));
    assert_eq!(us.daily_reset_unix, Some(1_700_086_400));
    assert_eq!(us.weekly_reset_unix, Some(1_700_604_800));

    let q = to_account_quota(&us);
    assert_eq!(q.session_limit, Some(100));
    assert_eq!(q.session_used, Some(27));
    assert_eq!(q.weekly_limit, Some(100));
    assert_eq!(q.weekly_used, Some(60));
    assert_eq!(q.plan_name.as_deref(), Some("Devin Teams"));
    assert!(q.session_reset_at.is_some());
    assert!(q.weekly_reset_at.is_some());
    let details = q.model_details.unwrap();
    assert_eq!(details.len(), 2);
    assert!((details[0].remaining_fraction - 0.73).abs() < 1e-9);
    assert!((details[1].remaining_fraction - 0.40).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// adapter surface
// ---------------------------------------------------------------------------

#[test]
fn adapter_headers_and_url() {
    let adapter = DevinAdapter::new();
    assert_eq!(adapter.config().id.as_str(), "devin");
    let url = adapter.build_chat_url(
        openproxy_types::TargetFormat::Devin,
        &openproxy_types::ModelId::new("swe-2"),
    );
    assert_eq!(url, format!("https://server.codeium.com{DEVIN_CHAT_PATH}"));

    let headers = adapter.build_headers(
        "tok1",
        openproxy_types::TargetFormat::Devin,
        &openproxy_types::ModelId::new("swe-2"),
    );
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    assert_eq!(get("authorization").as_deref(), Some("Basic tok1-tok1"));
    assert_eq!(
        get("content-type").as_deref(),
        Some("application/connect+proto")
    );
    assert_eq!(get("connect-protocol-version").as_deref(), Some("1"));
    assert_eq!(get("accept").as_deref(), Some("*/*"));
    assert!(get("sentry-trace").is_some());
    // Upstream ships an explicit empty User-Agent; empty also suppresses
    // openproxy's default UA injection downstream.
    assert_eq!(get("user-agent").as_deref(), Some(""));
}

#[test]
fn adapter_two_stage_body_build() {
    let adapter = DevinAdapter::new();
    let req = req_with(vec![msg("user", json!("hi"))]);
    let model = openproxy_types::ModelId::new("swe-2");
    let carrier = adapter
        .format_request(
            openproxy_types::TargetFormat::Devin,
            &req,
            &model,
            &req.messages,
            true,
        )
        .unwrap();
    // carrier is JSON
    assert!(serde_json::from_slice::<OpenAIRequest>(&carrier).is_ok());

    let resolved = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::ComboTarget::default(),
        model: openproxy_types::models::Model::default(),
        api_key: "secret-tok".into(),
        api_key_label: None,
        custom_meta: None,
    };
    let body = adapter
        .wrap_request_body(
            carrier,
            openproxy_types::TargetFormat::Devin,
            &model,
            &resolved,
        )
        .unwrap();
    let fields = decode_request(&body);
    let meta = collect(match get(&fields, 1).unwrap() {
        Fv::Bytes(b) => b,
        _ => panic!(),
    });
    assert_eq!(as_str(get(&meta, 3).unwrap()), "secret-tok");
    assert_eq!(as_str(get(&fields, 21).unwrap()), "swe-2-high");

    // an already-encoded envelope passes through untouched
    let env = Bytes::from(wrap_connect_envelope(b"\x08\x01"));
    let passthrough = adapter
        .wrap_request_body(
            env.clone(),
            openproxy_types::TargetFormat::Devin,
            &model,
            &resolved,
        )
        .unwrap();
    assert_eq!(passthrough, env);
}

#[test]
fn oauth_account_embeds_access_token_in_metadata() {
    // OAuth accounts carry an empty api_key; the session token lives in
    // custom_meta.access_token. ClientMetadata field 3 must embed that same
    // credential or Devin denies the call (permission_denied) even though
    // the Authorization header was right.
    let adapter = DevinAdapter::new();
    let req = req_with(vec![msg("user", json!("hi"))]);
    let model = openproxy_types::ModelId::new("swe-2");
    let carrier = adapter
        .format_request(
            openproxy_types::TargetFormat::Devin,
            &req,
            &model,
            &req.messages,
            true,
        )
        .unwrap();

    let meta = openproxy_types::context::CustomProviderMeta {
        access_token: "devin-session-token$oauth".into(),
        maybe_refresh: None,
        kiro_region: None,
        kiro_profile_arn: None,
        antigravity_project: None,
        antigravity_metadata: None,
        codex_workspace_id: None,
        claude_account_uuid: None,
        claude_metadata: None,
        devin_device_seed: None,
    };
    let resolved = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::ComboTarget::default(),
        model: openproxy_types::models::Model::default(),
        api_key: String::new(),
        api_key_label: None,
        custom_meta: Some(meta),
    };
    let body = adapter
        .wrap_request_body(
            carrier,
            openproxy_types::TargetFormat::Devin,
            &model,
            &resolved,
        )
        .unwrap();
    let fields = decode_request(&body);
    let client_meta = collect(match get(&fields, 1).unwrap() {
        Fv::Bytes(b) => b,
        _ => panic!(),
    });
    assert_eq!(
        as_str(get(&client_meta, 3).unwrap()),
        "devin-session-token$oauth"
    );
}

#[test]
fn devin_builtin_models_catalog() {
    let models = devin_builtin_models();
    assert_eq!(models.len(), 46);
    // swe-2 lists as swe-2:free (free cost tier on the wire).
    let swe2 = models
        .iter()
        .find(|m| m.model_id.as_str() == "swe-2:free")
        .unwrap();
    assert_eq!(swe2.display_name.as_deref(), Some("SWE-2"));
    assert_eq!(swe2.target_format, openproxy_types::TargetFormat::Devin);
    assert_eq!(swe2.context_length, Some(262_000));
    assert_eq!(swe2.max_output_tokens, Some(64_000));
    assert_eq!(swe2.family.as_deref(), Some("cognition"));
    assert_eq!(swe2.model_type.as_deref(), Some("chat"));
}

#[test]
fn free_models_emit_full_fraction_quota_detail() {
    use super::super::super::catalog::parse_client_model_configs;
    // A user_status whose catalog marks swe-2 free (cost tier 4; promo is
    // irrelevant — paid models carry active promos too) and leaves a paid
    // sibling (glm-5.3) untouched.
    let mut swe2_cfg = Vec::new();
    protoc::append_fixed32(&mut swe2_cfg, 3, 9.0f32.to_bits());
    protoc::append_string(&mut swe2_cfg, 22, "swe-2-high");
    let mut swe2_info = Vec::new();
    protoc::append_string(&mut swe2_info, 23, "swe-2");
    protoc::append_message(&mut swe2_cfg, 23, &swe2_info);
    protoc::append_varint_field(&mut swe2_cfg, 24, 4);
    let mut promo = Vec::new();
    protoc::append_varint_field(&mut promo, 1, 1);
    protoc::append_message(&mut swe2_cfg, 19, &promo);

    let mut paid_cfg = Vec::new();
    protoc::append_fixed32(&mut paid_cfg, 3, 1.0f32.to_bits());
    protoc::append_string(&mut paid_cfg, 22, "glm-5-3-low");
    let mut paid_info = Vec::new();
    protoc::append_string(&mut paid_info, 23, "glm-5.3");
    protoc::append_message(&mut paid_cfg, 23, &paid_info);
    protoc::append_varint_field(&mut paid_cfg, 24, 1);

    let mut catalog_blob = Vec::new();
    protoc::append_message(&mut catalog_blob, 1, &swe2_cfg);
    protoc::append_message(&mut catalog_blob, 1, &paid_cfg);

    let mut us_msg = Vec::new();
    protoc::append_message(&mut us_msg, 33, &catalog_blob);
    let mut body = Vec::new();
    protoc::append_message(&mut body, 1, &us_msg);

    let us = parse_user_status(&body).unwrap();
    assert_eq!(us.model_configs.len(), 2);

    let q = to_account_quota(&us);
    let details = q.model_details.unwrap();
    let free = details
        .iter()
        .find(|d| d.model_id == "swe-2:free")
        .expect("swe-2:free detail");
    assert_eq!(free.remaining_fraction, 1.0);
    assert_eq!(free.session_limit, 9_999);
    assert!(
        !details.iter().any(|d| d.model_id == "glm-5-3:free"),
        "paid models get no free detail"
    );
    // Catalog parsing cross-check: the fixture really did mark swe-2 free.
    assert!(parse_client_model_configs(&catalog_blob)[0].is_free());
}
