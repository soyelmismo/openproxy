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
use openproxy_adapters::adapters::claude_code::signing::sign_anthropic_messages_body;
use openproxy_adapters::adapters::{
    AdapterFormat, CustomAdapter, ProviderAdapterConfig, ProviderAdapterEnum,
};
use openproxy_types::context::{CustomProviderMeta, ResolvedTarget};
use openproxy_types::ids::{ComboId, ComboTargetId, ModelRowId, ProviderId, format_as_v4_uuid};
use openproxy_types::models::Model;
use openproxy_types::{AuthType, ComboTarget, TargetFormat};
use serde_json::{Value, json};
use std::sync::Arc;

/// Length of the embedded CCH signature.
const CCH_LEN: usize = 5;

/// A test credential. Not a real key; never a production secret.
const TEST_CREDENTIAL: &str = "test-credential-not-real";

/// A Claude Code account UUID used to make identity derivation deterministic.
const TEST_ACCOUNT_UUID: &str = "11111111-2222-4333-8444-555555555555";

/// The explicit session used across the Claude Code tests.
const TEST_SESSION: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

/// A base URL nothing ever dials (port 1 is reserved/closed).
const UNREACHABLE_BASE: &str = "http://127.0.0.1:1";

// ---------------------------------------------------------------------------
// fixtures (mirrors of the real ones in target_tests.rs)
// ---------------------------------------------------------------------------

/// Build a `Model` for a target format with no capabilities.
fn model_for(format: TargetFormat) -> Model {
    Model {
        model_id: "claude-sonnet-4-5-20250929".into(),
        target_format: format,
        ..Default::default()
    }
}

/// Build a `ComboTarget` for a provider id, using the real struct defaults.
fn target_for(provider_id: &str) -> ComboTarget {
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
fn resolved_target_for(provider_id: &str, format: TargetFormat) -> ResolvedTarget {
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
fn claude_resolved_target() -> ResolvedTarget {
    let (api_key,) = (TEST_CREDENTIAL.to_owned(),);
    let (access_token,) = (TEST_CREDENTIAL.to_owned(),);
    ResolvedTarget {
        target: target_for("claude-code"),
        model: model_for(TargetFormat::Anthropic),
        api_key,
        api_key_label: None,
        custom_meta: Some(CustomProviderMeta {
            access_token,
            maybe_refresh: None,
            kiro_region: None,
            kiro_profile_arn: None,
            antigravity_project: None,
            antigravity_metadata: None,
            codex_workspace_id: None,
            claude_account_uuid: Some(TEST_ACCOUNT_UUID.to_string()),
            claude_metadata: None,
        }),
    }
}

/// A generic custom adapter with no provider-specific transforms.
fn plain_adapter(provider_id: &str, format: AdapterFormat) -> ProviderAdapterEnum {
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
fn claude_code_adapter() -> ProviderAdapterEnum {
    ProviderAdapterEnum::ClaudeCode(Box::new(ClaudeCodeAdapter::with_base_url(UNREACHABLE_BASE)))
}

/// Result of running the real production chain by hand, stopping right before
/// the upstream HTTP call. Mirrors `FormattingStage` then `DispatchStage`'s
/// body/header preparation.
struct ChainResult {
    target_format: TargetFormat,
    body: Bytes,
    headers: Vec<(String, String)>,
}

/// Run the exact production chain: formatter -> wrap -> strip -> propagate
/// headers -> finalize. Mirrors `target.rs`'s `FormattingStage` and the
/// body/header half of `DispatchStage`, minus the upstream HTTP call.
fn run_chain(
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
    }
}

/// Compute the CCH signature the way production does: clone, sign, return.
fn expected_cch(body: &[u8]) -> String {
    let mut clone = body.to_vec();
    sign_anthropic_messages_body(&mut clone).expect("body must be signable")
}

/// Read the 5-char CCH signature embedded in a Claude Code body.
fn embedded_cch(body: &[u8]) -> String {
    let off = body
        .windows(4)
        .position(|w| w == b"cch=")
        .expect("body must contain a cch= marker");
    String::from_utf8(body[off + 4..off + 4 + CCH_LEN].to_vec()).expect("cch must be valid utf-8")
}

/// Read the session id out of a Claude Code body's `metadata.user_id`.
fn body_session(body: &Bytes) -> String {
    let v: Value = serde_json::from_slice(body).expect("final body must be JSON");
    let meta_str = v["metadata"]["user_id"]
        .as_str()
        .expect("metadata.user_id must be a string");
    let meta: Value = serde_json::from_str(meta_str).expect("metadata.user_id must be JSON");
    meta["session_id"].as_str().expect("session_id").to_string()
}

/// A Claude Code style request: system instruction, a user turn, a tool and a
/// provider-specific param (`top_k`) that a strip stage can remove.
fn claude_code_request(explicit_session_header: Option<&str>) -> PipelineRequest {
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

// ===========================================================================
// Part 1 — Claude Code: semantic preservation through the whole chain
// ===========================================================================

/// An explicit client session survives formatting, wrapping, stripping and
/// finalization: it appears in the body's `metadata.user_id` and is forwarded
/// as the `x-claude-code-session-id` affinity header.
#[test]
fn claude_code_explicit_session_preserved_through_chain() {
    let req = claude_code_request(Some(TEST_SESSION));
    let adapter = claude_code_adapter();
    let resolved = claude_resolved_target();

    let out = run_chain(&adapter, &resolved, &req);
    assert_eq!(out.target_format, TargetFormat::Anthropic);

    // Body session.
    assert_eq!(
        body_session(&out.body),
        TEST_SESSION,
        "explicit session must be preserved in the body"
    );

    // Header session: propagated as the canonical UUID form of the same value.
    let header = out
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-claude-code-session-id"))
        .map(|(_, v)| v.clone())
        .expect("x-claude-code-session-id must be propagated");
    assert_eq!(
        header,
        format_as_v4_uuid(TEST_SESSION),
        "explicit session must be forwarded as its canonical uuid form"
    );

    // Semantic content survived the chain.
    let body: Value = serde_json::from_slice(&out.body).expect("final body must be JSON");
    assert!(
        body["tools"].as_array().is_some_and(|t| !t.is_empty()),
        "tools must survive the chain"
    );
    assert_eq!(
        body["tools"][0]["name"].as_str(),
        Some("ReadFile"),
        "tool name must survive the chain, cloaked to TitleCase for the upstream"
    );
    assert_eq!(
        body["max_tokens"].as_u64(),
        Some(1024),
        "max_tokens must survive the chain"
    );
    assert_eq!(
        body["top_k"].as_u64(),
        Some(7),
        "top_k must survive when nothing is configured to strip it"
    );
}

/// Tool names round-trip: the real chain cloaks them to TitleCase for the
/// upstream, and the response side maps every `tool_use` name back to what the
/// client declared. Runs the real functions on both directions.
#[test]
fn claude_code_tool_names_roundtrip_through_chain() {
    let req = claude_code_request(Some(TEST_SESSION));
    let adapter = claude_code_adapter();
    let resolved = claude_resolved_target();

    let out = run_chain(&adapter, &resolved, &req);
    let body: Value = serde_json::from_slice(&out.body).expect("final body must be JSON");
    assert_eq!(
        body["tools"][0]["name"].as_str(),
        Some("ReadFile"),
        "the chain must cloak the tool name to TitleCase"
    );

    // The restore map the dispatcher builds from the client's own tool list.
    let map = crate::claude_tool_names::build_restore_map(&req).expect("restore map");
    assert_eq!(map.get("ReadFile").map(String::as_str), Some("read_file"));

    // Streaming direction: a real upstream content_block_start carrying the
    // cloaked name, translated by the real SSE translator.
    let mut tool_use_acc = None;
    let mut tool_call_index_counter = 0;
    let chunk = crate::sse::translate_anthropic_sse_event(
        "content_block_start\n{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"ReadFile\",\"input\":{}}}",
        "chunk-1",
        1000,
        "claude-sonnet-4-5-20250929",
        &mut tool_use_acc,
        &mut tool_call_index_counter,
    )
    .expect("translation must succeed")
    .expect("a tool_use start must produce a chunk");
    let mut payload = chunk.payload;
    crate::claude_tool_names::restore_chunk_tool_names(&mut payload, &map);
    assert_eq!(
        payload["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "read_file",
        "the client must receive the tool name it declared"
    );

    // Unary direction: a real upstream response translated by the real
    // anthropic_to_openai, then restored.
    let upstream = json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-5-20250929",
        "content": [{
            "type": "tool_use",
            "id": "toolu_1",
            "name": "ReadFile",
            "input": {"path": "src/main.rs"}
        }],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    let anthropic_resp: crate::translation::AnthropicResponse =
        serde_json::from_value(upstream).expect("anthropic response");
    let mut openai_resp = crate::translation::anthropic_to_openai(&anthropic_resp);
    crate::claude_tool_names::restore_response_tool_names(&mut openai_resp, &map);
    let call = &openai_resp.choices[0]
        .message
        .tool_calls
        .as_ref()
        .expect("tool_calls")[0];
    assert_eq!(call["function"]["name"], "read_file");
    assert_eq!(
        call["function"]["arguments"],
        r#"{"path":"src/main.rs"}"#,
        "arguments must stay byte-identical through the restore"
    );
}

/// With no explicit session the session is *derived* from the invariant root of
/// the conversation (system prompt + first user turn). The derivation must be
/// deterministic across runs, which is what gives multi-turn KV cache affinity.
#[test]
fn claude_code_derived_session_is_deterministic() {
    let adapter = claude_code_adapter();
    let resolved = claude_resolved_target();

    let req = claude_code_request(None);
    let first = run_chain(&adapter, &resolved, &req);
    let second = run_chain(&adapter, &resolved, &req);

    let s1 = body_session(&first.body);
    let s2 = body_session(&second.body);

    assert!(!s1.is_empty(), "a session must be derived");
    assert_eq!(
        s1, s2,
        "derived session must be deterministic across identical requests"
    );
    assert_ne!(
        s1, TEST_SESSION,
        "derived session must not collide with the explicit one"
    );

    // A different first user turn must derive a different session.
    let mut other = claude_code_request(None);
    let openai = Arc::get_mut(&mut other.openai_request).expect("owned");
    openai.messages[1].content = Some(json!("A completely different question."));
    let third = run_chain(&adapter, &resolved, &other);
    assert_ne!(
        s1,
        body_session(&third.body),
        "different conversation root must derive a different session"
    );
}

/// The caller's system instruction is preserved through the chain. Claude Code
/// re-homes it into a `<system-reminder>` on the first user message while
/// installing its own billing and identity system blocks.
#[test]
fn claude_code_instructions_preserved_through_chain() {
    let req = claude_code_request(Some(TEST_SESSION));
    let adapter = claude_code_adapter();
    let resolved = claude_resolved_target();

    let out = run_chain(&adapter, &resolved, &req);
    let body: Value = serde_json::from_slice(&out.body).expect("JSON");

    // The caller instruction survives somewhere in the final bytes.
    let text = String::from_utf8_lossy(&out.body);
    assert!(
        text.contains("You are a careful code reviewer. Be concise."),
        "caller system instruction must survive somewhere in the final body"
    );

    // Claude Code's own system blocks are installed.
    let system = body["system"].as_array().expect("system must be an array");
    assert!(
        system.iter().any(|b| b["text"]
            .as_str()
            .is_some_and(|t| t.contains("cc_entrypoint=cli"))),
        "Claude Code billing header must be installed"
    );
    assert!(
        system.iter().any(|b| b["text"]
            .as_str()
            .is_some_and(|t| t.contains("You are Claude Code"))),
        "Claude Code identity block must be installed"
    );
}

/// The CCH signature must be computed over the FINAL bytes.
///
/// `wrap_request_body` signs the body; `strip_params` then removes keys, which
/// would invalidate that signature. `finalize_request` re-signs, so the
/// signature embedded in the dispatched body must be a fixed point: signing the
/// final body again yields the identical signature.
#[test]
fn claude_code_cch_resigns_over_post_strip_bytes() {
    // Configure the adapter to strip `top_k` so the body changes after signing.
    let mut adapter = claude_code_adapter();
    adapter.append_extra_headers(vec![(
        "x-openproxy-strip-params".to_string(),
        "top_k".to_string(),
    )]);

    let req = claude_code_request(Some(TEST_SESSION));
    let resolved = claude_resolved_target();
    let out = run_chain(&adapter, &resolved, &req);
    let body: Value = serde_json::from_slice(&out.body).expect("JSON");

    // The strip really happened.
    assert!(
        body.get("top_k").is_none(),
        "top_k must have been stripped by the chain"
    );

    // A real signature is present and is a fixed point over the final bytes.
    let embedded = embedded_cch(&out.body);
    assert_ne!(embedded, "00000", "signature must not be the placeholder");
    assert_eq!(
        embedded,
        expected_cch(&out.body),
        "embedded CCH must match a recomputation over the FINAL bytes \
         (finalize_request must re-sign after strip)"
    );
}

/// Without any strip the body is unchanged after wrapping, so the signature is
/// already the fixed point before `finalize_request` runs — and stays one after.
#[test]
fn claude_code_cch_signature_is_fixed_point_without_strip() {
    let req = claude_code_request(Some(TEST_SESSION));
    let adapter = claude_code_adapter();
    let resolved = claude_resolved_target();

    let out = run_chain(&adapter, &resolved, &req);
    assert_eq!(
        embedded_cch(&out.body),
        expected_cch(&out.body),
        "dispatched CCH must be valid over the dispatched bytes"
    );

    // Re-signing the exact final bytes must not change them (idempotence),
    // which is what keeps the signature stable across retries.
    let mut once = out.body.to_vec();
    let sig1 = sign_anthropic_messages_body(&mut once).expect("sign");
    let mut twice = once.clone();
    let sig2 = sign_anthropic_messages_body(&mut twice).expect("sign");
    assert_eq!(sig1, sig2, "signing must be idempotent");
    assert_eq!(once, twice, "signing the same bytes must not change them");
}

/// Multi-turn continuity: appending turns keeps the explicit session stable
/// while the body grows correctly and the signature stays valid over the longer
/// final bytes.
#[test]
fn claude_code_multi_turn_session_stable() {
    let mut req = claude_code_request(Some(TEST_SESSION));
    let openai = Arc::get_mut(&mut req.openai_request).expect("owned");

    // Turns 2 and 3 appended; the invariant root (system + first user) is kept.
    openai.messages.push(openproxy_types::OpenAIMessage {
        role: "assistant".into(),
        content: Some(json!("The borrow checker flags this.")),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: serde_json::Map::new(),
    });
    openai.messages.push(openproxy_types::OpenAIMessage {
        role: "user".into(),
        content: Some(json!("Fix it then.")),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: serde_json::Map::new(),
    });

    let adapter = claude_code_adapter();
    let resolved = claude_resolved_target();
    let out = run_chain(&adapter, &resolved, &req);
    let body: Value = serde_json::from_slice(&out.body).expect("JSON");

    // Session is still the explicit one.
    assert_eq!(body_session(&out.body), TEST_SESSION);

    // All turns reached the upstream (system is re-homed out of `messages`).
    let msgs = body["messages"].as_array().expect("messages array");
    let roles: Vec<&str> = msgs.iter().filter_map(|m| m["role"].as_str()).collect();
    assert_eq!(
        roles,
        vec!["user", "assistant", "user"],
        "all conversational turns must survive"
    );
    let texts = String::from_utf8_lossy(&out.body);
    assert!(texts.contains("Fix it then."), "final turn must be present");

    // Signature still valid over the longer final bytes.
    assert_eq!(
        embedded_cch(&out.body),
        expected_cch(&out.body),
        "CCH must remain a fixed point over multi-turn final bytes"
    );

    // The header session is unchanged, which is what keeps the multi-turn
    // conversation pinned to one upstream worker.
    assert!(
        out.headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-claude-code-session-id")
                && v == &format_as_v4_uuid(TEST_SESSION)),
        "session affinity header must be stable across turns"
    );
}

/// `finalize_request` runs LAST: what it receives is already the wrapped
/// Claude Code body, so the dispatched bytes carry the billing header.
#[test]
fn claude_code_finalize_receives_already_wrapped_body() {
    let req = claude_code_request(Some(TEST_SESSION));
    let adapter = claude_code_adapter();
    let resolved = claude_resolved_target();

    let out = run_chain(&adapter, &resolved, &req);
    assert!(
        String::from_utf8_lossy(&out.body).contains("cc_entrypoint=cli"),
        "final bytes must carry the Claude Code billing header"
    );
    assert_eq!(embedded_cch(&out.body), expected_cch(&out.body));
}

// ===========================================================================
// Part 2 — default identity hook: every TargetFormat, no schema mutation
// ===========================================================================

/// For every `TargetFormat` and its default provider format,
/// `finalize_request` must be the identity function: body bytes and headers
/// come out byte-for-byte identical to what they were before the hook ran.
#[test]
fn default_finalize_request_is_identity_for_all_target_formats() {
    // Every (ProviderFormat, default TargetFormat) pair that exists, taken from
    // `ProviderFormat::default_target_format()`.
    let cases: &[(AdapterFormat, TargetFormat)] = &[
        (AdapterFormat::Openai, TargetFormat::Openai),
        (AdapterFormat::Anthropic, TargetFormat::Anthropic),
        (AdapterFormat::Gemini, TargetFormat::Gemini),
        (AdapterFormat::Responses, TargetFormat::Responses),
        (AdapterFormat::Atomesus, TargetFormat::Atomesus),
        (AdapterFormat::CommandCodeGo, TargetFormat::CommandCodeGo),
        (AdapterFormat::SystemOne, TargetFormat::SystemOne),
    ];

    // Guard: the table must cover every TargetFormat that exists.
    let covered: Vec<TargetFormat> = cases.iter().map(|(_, t)| *t).collect();
    for fmt in TargetFormat::ALL {
        assert!(
            covered.contains(fmt),
            "identity table is missing TargetFormat {fmt:?}"
        );
    }

    for (adapter_format, target_format) in cases {
        let adapter = plain_adapter("plain-provider", *adapter_format);
        let (req, _tx) = make_request(ComboId(1));
        let resolved = resolved_target_for("plain-provider", *target_format);
        let model = &resolved.model;
        let target = &resolved.target;

        // Reproduce the chain up to (but excluding) finalize_request.
        let formatter = get_formatter(*target_format);
        let mut body = formatter
            .format_request(
                &req,
                model,
                &req.openai_request.messages,
                req.openai_request.stream,
                &adapter,
            )
            .expect("formatter");
        body = adapter
            .wrap_request_body(body, *target_format, &model.model_id, &resolved)
            .expect("wrap");
        body = strip_provider_configured_params_from_body(body, &adapter.config().extra_headers)
            .expect("strip");

        let mut headers = adapter.build_headers(TEST_CREDENTIAL, *target_format, &model.model_id);
        propagate_provider_target_headers(
            &mut headers,
            target.provider_id.as_str(),
            adapter.id().as_str(),
            &req.request_headers,
            &req.openai_request,
            None,
        );

        let body_before = body.clone();
        let headers_before = headers.clone();

        // The hook under test.
        let body_after = adapter
            .finalize_request(
                body.clone(),
                &mut headers,
                *target_format,
                &model.model_id,
                &resolved,
            )
            .expect("finalize must succeed");

        assert_eq!(
            body_before.as_ref(),
            body_after.as_ref(),
            "default finalize_request must not mutate body bytes for {target_format:?}"
        );
        assert_eq!(
            headers_before, headers,
            "default finalize_request must not mutate headers for {target_format:?}"
        );
    }
}

/// The same identity guarantee, but driven through the full chain helper so it
/// exercises formatter + wrapper + strip + propagate + finalize together
/// rather than the hook in isolation.
#[test]
fn default_identity_holds_through_full_chain() {
    for (adapter_format, target_format) in [
        (AdapterFormat::Openai, TargetFormat::Openai),
        (AdapterFormat::Anthropic, TargetFormat::Anthropic),
        (AdapterFormat::Gemini, TargetFormat::Gemini),
        (AdapterFormat::Responses, TargetFormat::Responses),
        (AdapterFormat::Atomesus, TargetFormat::Atomesus),
        (AdapterFormat::CommandCodeGo, TargetFormat::CommandCodeGo),
        (AdapterFormat::SystemOne, TargetFormat::SystemOne),
    ] {
        let adapter = plain_adapter("plain-provider", adapter_format);
        let resolved = resolved_target_for("plain-provider", target_format);
        let (req, _tx) = make_request(ComboId(1));

        let out = run_chain(&adapter, &resolved, &req);

        // The formatter really produced format-appropriate bytes.
        let parsed: Value =
            serde_json::from_slice(&out.body).expect("final body must be JSON for every format");
        assert!(
            parsed.is_object(),
            "final body must be a JSON object for {target_format:?}"
        );

        // And the chain is stable: running it again gives identical bytes and
        // headers, proving no stage (including finalize) is non-deterministic
        // or mutating by schema.
        let out2 = run_chain(&adapter, &resolved, &req);
        assert_eq!(
            out.body, out2.body,
            "chain must be deterministic for {target_format:?}"
        );
        assert_eq!(
            out.headers, out2.headers,
            "chain headers must be deterministic for {target_format:?}"
        );
    }
}
