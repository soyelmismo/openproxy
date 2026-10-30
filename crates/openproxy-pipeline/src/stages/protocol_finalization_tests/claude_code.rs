use super::common::*;
use crate::PipelineRequest;
use openproxy_types::TargetFormat;
use openproxy_types::ids::format_as_v4_uuid;
use serde_json::{Value, json};
use std::sync::Arc;

// ===========================================================================
// Part 1 — Claude Code: semantic preservation through the whole chain
// ===========================================================================

/// Run the full production chain against the standard Claude Code adapter
/// and resolved-target fixtures.
fn run_claude_code_chain(req: &PipelineRequest) -> ChainResult {
    run_chain(&claude_code_adapter(), &claude_resolved_target(), req)
}

/// An explicit client session survives formatting, wrapping, stripping and
/// finalization: it appears in the body's `metadata.user_id` and is forwarded
/// as the `x-claude-code-session-id` affinity header.
#[test]
fn claude_code_explicit_session_preserved_through_chain() {
    let req = claude_code_request(Some(TEST_SESSION));

    let out = run_claude_code_chain(&req);
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

    let out = run_claude_code_chain(&req);
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
        payload["choices"][0]["delta"]["tool_calls"][0]["function"]["name"], "read_file",
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
        call["function"]["arguments"], r#"{"path":"src/main.rs"}"#,
        "arguments must stay byte-identical through the restore"
    );
}

/// With no explicit session the session is *derived* from the invariant root of
/// the conversation (system prompt + first user turn). The derivation must be
/// deterministic across runs, which is what gives multi-turn KV cache affinity.
#[test]
fn claude_code_derived_session_is_deterministic() {
    let req = claude_code_request(None);
    let first = run_claude_code_chain(&req);
    let second = run_claude_code_chain(&req);

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
    let third = run_claude_code_chain(&other);
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

    let out = run_claude_code_chain(&req);
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

    let out = run_claude_code_chain(&req);
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

    let out = run_claude_code_chain(&req);
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

    let out = run_claude_code_chain(&req);
    assert!(
        String::from_utf8_lossy(&out.body).contains("cc_entrypoint=cli"),
        "final bytes must carry the Claude Code billing header"
    );
    assert_eq!(embedded_cch(&out.body), expected_cch(&out.body));
}
