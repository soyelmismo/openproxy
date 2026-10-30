//! Pipeline regression tests for request finalization on the FINAL bytes.
//!
//! These tests exercise the whole production chain, not an isolated hook:
//!
//! ```text
//! real formatter (get_formatter)
//!   -> wrap_request_body        (provider adapter, &ResolvedTarget)
//!   -> strip_params             (strip_provider_configured_params_from_body)
//!   -> propagate_provider_target_headers
//!   -> finalize_request         (provider adapter, runs LAST on final bytes)
//! ```
//!
//! Two guarantees are enforced:
//!
//! 1. **Claude Code semantic preservation** — session (explicit and derived),
//!    billing header, tools, instructions and multi-turn history all survive
//!    every step, and the CCH signature is recomputed over the post-strip
//!    bytes so re-signing the dispatched body is a fixed point.
//! 2. **Default-hook neutrality** — for every `TargetFormat` and its default
//!    provider format, `finalize_request` is the identity function: it must
//!    not touch the body bytes or the headers. Schema must never mutate the
//!    request.
//!
//! The chain is driven with the real production functions; the only transport
//! involved is the (unused) loopback base URL. No network egress, no
//! credentials: every credential-shaped value is a test literal.

use crate::PipelineRequest;
use crate::formatting::{get_formatter, strip_provider_configured_params_from_body};
use crate::stages::target_headers::propagate_provider_target_headers;
use crate::test_utils::make_request;
use bytes::Bytes;
use openproxy_adapters::adapters::claude_code::ClaudeCodeAdapter;
pub(crate) use openproxy_adapters::adapters::claude_code::signing::sign_anthropic_messages_body;
use openproxy_adapters::adapters::{
    AdapterFormat, CustomAdapter, ProviderAdapterConfig, ProviderAdapterEnum,
};
use openproxy_types::context::{CustomProviderMeta, ResolvedTarget};
use openproxy_types::ids::{ComboId, ComboTargetId, ModelRowId, ProviderId};
use openproxy_types::models::Model;
use openproxy_types::{AuthType, ComboTarget, TargetFormat};
use serde_json::{Value, json};
use std::sync::Arc;

#[test]
fn antigravity_full_chain_cleans_claude_metadata_and_keeps_user_content() {
    let adapter = ProviderAdapterEnum::Antigravity(Box::default());
    let mut resolved = resolved_target_for("antigravity", TargetFormat::Gemini);
    let mut meta = blank_custom_meta("test-token".into());
    meta.antigravity_project = Some("project-contract-test".into());
    resolved.custom_meta = Some(meta);
    let (mut req, _tx) = make_request(ComboId(1));
    Arc::get_mut(&mut req.openai_request).expect("fresh request").messages = serde_json::from_value(json!([
        {"role":"system","content":"You are Claude Code, Anthropic's official CLI for Claude.\nPreserve this application instruction."},
        {"role":"user","content":"Explain cc_workload literally. Do not modify my source code."}
    ])).unwrap();
    req.request_headers
        .insert("x-stainless-lang".into(), "js".into());
    req.request_headers
        .insert("x-anthropic-billing-header".into(), "billing".into());
    let out = run_chain(&adapter, &resolved, &req);
    let parsed: Value = serde_json::from_slice(&out.body).unwrap();
    assert_eq!(parsed["project"], "project-contract-test");
    let text = String::from_utf8_lossy(&out.body);
    assert!(!text.contains("You are Claude Code, Anthropic's official CLI"));
    assert!(text.contains("Preserve this application instruction."));
    assert!(text.contains("Explain cc_workload literally. Do not modify my source code."));
    let header = |name: &str| {
        out.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(
        header("user-agent"),
        Some(openproxy_adapters::antigravity_headers::hub_user_agent().as_str())
    );
    assert_eq!(header("x-goog-user-project"), None);
    assert_eq!(header("x-client-version"), None);
    assert_eq!(header("x-stainless-lang"), None);
    assert_eq!(header("x-anthropic-billing-header"), None);
}

/// Length of the embedded CCH signature.
pub(crate) const CCH_LEN: usize = 5;

/// A test credential. Not a real key; never a production secret.
pub(crate) const TEST_CREDENTIAL: &str = "test-credential-not-real";

/// A Claude Code account UUID used to make identity derivation deterministic.
pub(crate) const TEST_ACCOUNT_UUID: &str = "11111111-2222-4333-8444-555555555555";

/// The explicit session used across the Claude Code tests.
pub(crate) const TEST_SESSION: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

/// A base URL nothing ever dials (port 1 is reserved/closed).
pub(crate) const UNREACHABLE_BASE: &str = "http://127.0.0.1:1";

/// A `CustomProviderMeta` holding only an access token; every
/// provider-specific field stays empty so tests opt in per field.
fn blank_custom_meta(access_token: String) -> CustomProviderMeta {
    CustomProviderMeta {
        access_token,
        maybe_refresh: None,
        kiro_region: None,
        kiro_profile_arn: None,
        antigravity_project: None,
        antigravity_metadata: None,
        codex_workspace_id: None,
        claude_account_uuid: None,
        claude_metadata: None,
        devin_device_seed: None,
    }
}

// ---------------------------------------------------------------------------
// fixtures (mirrors of the real ones in target_tests.rs)
// ---------------------------------------------------------------------------

/// Build a `Model` for a target format with no capabilities.
pub(crate) fn model_for(format: TargetFormat) -> Model {
    Model {
        model_id: "claude-sonnet-4-5-20250929".into(),
        target_format: format,
        ..Default::default()
    }
}

/// Build a `ComboTarget` for a provider id, using the real struct defaults.
pub(crate) fn target_for(provider_id: &str) -> ComboTarget {
    ComboTarget {
        id: ComboTargetId(1),
        combo_id: ComboId(1),
        provider_id: ProviderId::new(provider_id),
        account_id: None,
        model_row_id: Some(ModelRowId(1)),
        priority_order: 1,
        ..Default::default()
    }
}

/// Build a `ResolvedTarget` for a provider id and format.
pub(crate) fn resolved_target_for(provider_id: &str, format: TargetFormat) -> ResolvedTarget {
    let (api_key,) = (TEST_CREDENTIAL.to_owned(),);
    ResolvedTarget {
        target: target_for(provider_id),
        model: model_for(format),
        api_key,
        api_key_label: None,
        custom_meta: None,
    }
}

/// Build a `ResolvedTarget` carrying Claude Code account metadata.
pub(crate) fn claude_resolved_target() -> ResolvedTarget {
    let mut resolved = resolved_target_for("claude-code", TargetFormat::Anthropic);
    let mut meta = blank_custom_meta(TEST_CREDENTIAL.to_owned());
    meta.claude_account_uuid = Some(TEST_ACCOUNT_UUID.to_string());
    resolved.custom_meta = Some(meta);
    resolved
}

/// A generic custom adapter with no provider-specific transforms.
pub(crate) fn plain_adapter(provider_id: &str, format: AdapterFormat) -> ProviderAdapterEnum {
    ProviderAdapterEnum::Custom(Box::new(CustomAdapter::from_config(
        ProviderAdapterConfig {
            id: ProviderId::new(provider_id),
            name: String::from("generic-test-adapter"),
            base_url: UNREACHABLE_BASE.to_string(),
            auth_type: AuthType::Bearer,
            format,
            extra_headers: vec![],
            anonymous_fallback: false,
            rate_limit_scope: "none".to_string(),
        },
    )))
}

/// A Claude Code adapter pointed at a base URL we never dial.
pub(crate) fn claude_code_adapter() -> ProviderAdapterEnum {
    ProviderAdapterEnum::ClaudeCode(Box::new(ClaudeCodeAdapter::with_base_url(UNREACHABLE_BASE)))
}

/// Result of running the real production chain by hand, stopping right before
/// the upstream HTTP call. Mirrors `FormattingStage` then `DispatchStage`'s
/// body/header preparation.
pub(crate) struct ChainResult {
    pub(crate) target_format: TargetFormat,
    pub(crate) body: Bytes,
    pub(crate) headers: Vec<(String, String)>,
}

/// Run the exact production chain: formatter -> wrap -> strip -> propagate
/// headers -> finalize. Mirrors `target.rs`'s `FormattingStage` and the
/// body/header half of `DispatchStage`, minus the upstream HTTP call.
pub(crate) fn run_chain(
    adapter: &ProviderAdapterEnum,
    resolved: &ResolvedTarget,
    req: &PipelineRequest,
) -> ChainResult {
    let model = &resolved.model;
    let target = &resolved.target;

    // --- real formatter ---------------------------------------------------
    let target_format = resolve_format(adapter, model.target_format);
    let formatter = get_formatter(target_format);
    let mut body = formatter
        .format_request(
            req,
            model,
            &req.openai_request.messages,
            req.openai_request.stream,
            adapter,
        )
        .expect("formatter must produce a body");

    // --- real wrap_request_body (&ResolvedTarget) -------------------------
    body = adapter
        .wrap_request_body(body, target_format, &model.model_id, resolved)
        .expect("wrap_request_body must succeed");

    // --- real strip_params ------------------------------------------------
    body = strip_provider_configured_params_from_body(body, &adapter.config().extra_headers)
        .expect("strip must succeed");

    // --- real header build + propagate -----------------------------------
    let mut headers = adapter.build_headers(TEST_CREDENTIAL, target_format, &model.model_id);
    let codex_ws = resolved
        .custom_meta
        .as_ref()
        .and_then(|m| m.codex_workspace_id.as_deref());
    propagate_provider_target_headers(
        &mut headers,
        target.provider_id.as_str(),
        adapter.id().as_str(),
        &req.request_headers,
        &req.openai_request,
        codex_ws,
    );

    // --- real finalize_request (LAST, on the final bytes) -----------------
    body = adapter
        .finalize_request(body, &mut headers, target_format, &model.model_id, resolved)
        .expect("finalize_request must succeed");

    ChainResult {
        target_format,
        body,
        headers,
    }
}

/// Mirror of `resolve_target_format` in `target.rs` (private there, so the
/// test re-derives it from the same `AdapterFormat` mapping).
fn resolve_format(adapter: &ProviderAdapterEnum, model_format: TargetFormat) -> TargetFormat {
    match adapter.format() {
        AdapterFormat::Mixed => model_format,
        AdapterFormat::Openai => TargetFormat::Openai,
        AdapterFormat::Anthropic => TargetFormat::Anthropic,
        AdapterFormat::Gemini => TargetFormat::Gemini,
        AdapterFormat::Responses => TargetFormat::Responses,
        AdapterFormat::Atomesus => TargetFormat::Atomesus,
        AdapterFormat::CommandCodeGo => TargetFormat::CommandCodeGo,
        AdapterFormat::SystemOne => TargetFormat::SystemOne,
        AdapterFormat::Devin => TargetFormat::Devin,
    }
}

/// Compute the CCH signature the way production does: clone, sign, return.
pub(crate) fn expected_cch(body: &[u8]) -> String {
    let mut clone = body.to_vec();
    sign_anthropic_messages_body(&mut clone).expect("body must be signable")
}

/// Read the 5-char CCH signature embedded in a Claude Code body.
pub(crate) fn embedded_cch(body: &[u8]) -> String {
    let off = body
        .windows(4)
        .position(|w| w == b"cch=")
        .expect("body must contain a cch= marker");
    String::from_utf8(body[off + 4..off + 4 + CCH_LEN].to_vec()).expect("cch must be valid utf-8")
}

/// Read the session id out of a Claude Code body's `metadata.user_id`.
pub(crate) fn body_session(body: &Bytes) -> String {
    let v: Value = serde_json::from_slice(body).expect("final body must be JSON");
    let meta_str = v["metadata"]["user_id"]
        .as_str()
        .expect("metadata.user_id must be a string");
    let meta: Value = serde_json::from_str(meta_str).expect("metadata.user_id must be JSON");
    meta["session_id"].as_str().expect("session_id").to_string()
}

/// A Claude Code style request: system instruction, a user turn, a tool and a
/// provider-specific param (`top_k`) that a strip stage can remove.
pub(crate) fn claude_code_request(explicit_session_header: Option<&str>) -> PipelineRequest {
    let (mut req, _tx) = make_request(ComboId(1));
    let openai = Arc::get_mut(&mut req.openai_request).expect("fresh request is uniquely owned");

    openai.model = "claude-sonnet-4-5-20250929".into();
    openai.messages = vec![
        openproxy_types::OpenAIMessage {
            role: "system".into(),
            content: Some(json!("You are a careful code reviewer. Be concise.")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        },
        openproxy_types::OpenAIMessage {
            role: "user".into(),
            content: Some(json!("Review this Rust function for correctness.")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        },
    ];
    openai.tools = Some(vec![json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read a file from disk",
            "parameters": {
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }
        }
    })]);
    openai.tool_choice = Some(json!({"type": "auto"}));
    openai.top_k = Some(7);
    openai.max_tokens = Some(1024);
    // `user` feeds the derived-session fallback, so keep it empty here and let
    // the explicit case come from the session header instead.
    openai.user = None;

    if let Some(sid) = explicit_session_header {
        req.request_headers
            .insert("x-claude-code-session-id".to_string(), sid.to_string());
    }
    req
}
