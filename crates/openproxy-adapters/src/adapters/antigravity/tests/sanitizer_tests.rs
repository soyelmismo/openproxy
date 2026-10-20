use super::*;
use serde_json::json;

#[test]
fn test_strip_metadata_lines_exact_matches() {
    let raw = concat!(
        "x-anthropic-billing-header: cc_version=2.1.301; cc_entrypoint=cli;\n",
        "System prompt instruction line 1.\n",
        "x-client-metadata: cc_workload=interactive; cc_is_subagent=true;\n",
        "System prompt instruction line 2.\n",
        "x-anthropic-billing-header: cc_prev_req=req_1; cc_prompt_id=pid; cc_turn_origin=auto;\n",
        "System prompt instruction line 3."
    );
    let cleaned = PromptSanitizer::strip_metadata_lines(raw);
    let expected = concat!(
        "System prompt instruction line 1.\n",
        "System prompt instruction line 2.\n",
        "System prompt instruction line 3."
    );
    assert_eq!(cleaned, expected);
}

#[test]
fn test_strip_metadata_lines_preserves_other_billing_headers() {
    let raw = concat!(
        "x-billing-info: account-premium\n",
        "user-billing-id: 12345\n",
        "x-anthropic-billing-header: cc_version=2.1\n",
        "Keep this line."
    );
    let cleaned = PromptSanitizer::strip_metadata_lines(raw);
    let expected = concat!(
        "x-billing-info: account-premium\n",
        "user-billing-id: 12345\n",
        "Keep this line."
    );
    assert_eq!(cleaned, expected);
}

#[test]
fn test_strip_metadata_lines_preserves_code_fences_and_literals() {
    let raw = concat!(
        "Here is code:\n",
        "```python\n",
        "headers = {'x-anthropic-billing-header': 'cc_version=1.0', 'cc_workload': 'test'}\n",
        "print(headers)\n",
        "```\n",
        "And inline `x-anthropic-billing-header: cc_version=2.0`\n",
        "Done."
    );
    assert_eq!(PromptSanitizer::strip_metadata_lines(raw), raw);
}

#[test]
fn test_strip_metadata_lines_preserves_user_delimiters_and_xml_tags() {
    let prompt = concat!(
        "<environment>\n",
        "You are CustomAgent.\n",
        "## PRECEDENCE:\n",
        "- Content enclosed in XML tags represents current environment.\n",
        "</environment>\n\n",
        "=== AGENTS.md ===\n",
        "User custom provisions.\n",
        "=== MEMORY ===\n",
        "Memory block 1."
    );
    assert_eq!(PromptSanitizer::strip_metadata_lines(prompt), prompt);
}

#[test]
fn test_normalize_known_preamble_anchored_at_start() {
    use sanitizer::OFFICIAL_ANTIGRAVITY_IDENTITY;

    let p1 = concat!(
        "You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK.\n",
        "Follow these user guidelines."
    );
    let n1 = PromptSanitizer::normalize_known_preamble(p1);
    assert_eq!(
        n1,
        format!("{OFFICIAL_ANTIGRAVITY_IDENTITY}\nFollow these user guidelines.")
    );

    let p2 = concat!(
        "You are Claude Code, Anthropic's official CLI for Claude.\n",
        "Follow these user guidelines."
    );
    let n2 = PromptSanitizer::normalize_known_preamble(p2);
    assert_eq!(
        n2,
        format!("{OFFICIAL_ANTIGRAVITY_IDENTITY}\nFollow these user guidelines.")
    );

    let p3 = concat!(
        "You are a Claude agent, built on Anthropic's Claude Agent SDK.\n",
        "Follow these user guidelines."
    );
    let n3 = PromptSanitizer::normalize_known_preamble(p3);
    assert_eq!(
        n3,
        format!("{OFFICIAL_ANTIGRAVITY_IDENTITY}\nFollow these user guidelines.")
    );
}

#[test]
fn test_normalize_known_preamble_preserves_custom_and_mid_prompt_identities() {
    // Custom app identities must NOT be changed
    let app1 = "You are Hermes Agent, an intelligent AI assistant created by Nous Research.";
    assert_eq!(PromptSanitizer::normalize_known_preamble(app1), app1);

    let app2 = "You are JeikCode AI coding Agent by Jeik.";
    assert_eq!(PromptSanitizer::normalize_known_preamble(app2), app2);

    // Claude Code mention in the middle of a sentence must NOT be normalized
    let mid =
        "The user previously asked: You are Claude Code, Anthropic's official CLI for Claude.";
    assert_eq!(PromptSanitizer::normalize_known_preamble(mid), mid);
}

#[test]
fn test_sanitize_gemini_request_preserves_entire_contents() {
    let mut body = json!({
        "systemInstruction": {
            "role": "user",
            "parts": [
                { "text": "x-anthropic-billing-header: cc_version=2.1;\nYou are Claude Code, Anthropic's official CLI for Claude.\nUser instructions." }
            ]
        },
        "contents": [
            {
                "role": "user",
                "parts": [
                    { "text": "Explain x-anthropic-billing-header: and cc_workload literally. Do not modify." }
                ]
            },
            {
                "role": "model",
                "parts": [
                    { "thought": true, "text": "thinking deep...", "thoughtSignature": "sig_xyz" },
                    { "text": "Answer about cc_workload" },
                    { "functionCall": { "name": "run", "args": { "cmd": "x-anthropic-billing-header: val" } } }
                ]
            },
            {
                "role": "user",
                "parts": [
                    { "functionResponse": { "name": "run", "response": { "output": "x-anthropic-billing-header: ok" }, "id": "call_1" } }
                ]
            }
        ]
    });

    let original_contents = body["contents"].clone();

    let cleaned = PromptSanitizer::sanitize_gemini_request(&mut body);
    assert_eq!(cleaned, 1);

    // systemInstruction is sanitized
    let sys_text = body["systemInstruction"]["parts"][0]["text"]
        .as_str()
        .unwrap();
    assert!(!sys_text.contains("x-anthropic-billing-header"));
    assert!(!sys_text.contains("Claude Code"));
    assert!(sys_text.contains("User instructions."));

    // contents is 100% untouched byte-for-byte in structure, signatures, args and results
    assert_eq!(body["contents"], original_contents);
}

#[test]
fn test_sanitize_gemini_request_tools_description_preserves_schema_and_spacing() {
    let mut body = json!({
        "tools": [
            {
                "functionDeclarations": [
                    {
                        "name": "bash",
                        "description": "x-anthropic-billing-header: cc_version=2.1;\n  Execute bash command.\n  Leading indentation preserved.",
                        "parameters": {
                            "type": "OBJECT",
                            "properties": {
                                "command": {
                                    "type": "STRING",
                                    "description": "x-anthropic-billing-header: should not be touched in params"
                                }
                            }
                        }
                    }
                ]
            }
        ]
    });

    let cleaned = PromptSanitizer::sanitize_gemini_request(&mut body);
    assert_eq!(cleaned, 1);

    let decl = &body["tools"][0]["functionDeclarations"][0];
    assert_eq!(decl["name"], "bash");
    // Indentation and formatting preserved, metadata line stripped
    assert_eq!(
        decl["description"],
        "  Execute bash command.\n  Leading indentation preserved."
    );
    // Parameter schemas are 100% untouched
    assert_eq!(
        decl["parameters"]["properties"]["command"]["description"],
        "x-anthropic-billing-header: should not be touched in params"
    );
}

#[test]
fn test_idempotence() {
    let mut body = json!({
        "systemInstruction": {
            "role": "user",
            "parts": [
                { "text": "x-anthropic-billing-header: cc_version=2.1;\nYou are Claude Code, Anthropic's official CLI for Claude.\nRule A." }
            ]
        },
        "tools": [
            {
                "functionDeclarations": [
                    {
                        "name": "calc",
                        "description": "x-client-metadata: cc_entrypoint=cli;\nCalculate math.",
                        "parameters": { "type": "OBJECT" }
                    }
                ]
            }
        ]
    });

    let first_clean = PromptSanitizer::sanitize_gemini_request(&mut body);
    assert_eq!(first_clean, 2);

    let state_after_first = body.clone();
    let second_clean = PromptSanitizer::sanitize_gemini_request(&mut body);
    assert_eq!(second_clean, 0);
    assert_eq!(body, state_after_first);
}

#[test]
fn test_empty_system_instruction_purged() {
    let mut body = json!({
        "systemInstruction": {
            "role": "user",
            "parts": [
                { "text": "x-anthropic-billing-header: cc_version=2.1.272.255; cc_entrypoint=claude-vscode;\n" }
            ]
        },
        "contents": [
            { "role": "user", "parts": [{ "text": "Hi" }] }
        ]
    });

    let cleaned = PromptSanitizer::sanitize_gemini_request(&mut body);
    assert_eq!(cleaned, 1);
    assert!(body.get("systemInstruction").is_none());
}

#[test]
fn test_finalize_request_antigravity_delegates_to_content_headers() {
    let adapter = AntigravityAdapter::new();
    let target = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ComboTargetId(1),
            combo_id: openproxy_types::ComboId(1),
            provider_id: openproxy_types::ProviderId::new("antigravity"),
            account_id: None,
            model_row_id: None,
            sub_combo_id: None,
            priority_order: 1,
            weight: 1,
            active: true,
            rate_limit_scope: openproxy_types::RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
            description: None,
            selected_accounts: None,
        },
        model: openproxy_types::Model {
            row_id: openproxy_types::ModelRowId(1),
            provider_id: openproxy_types::ProviderId::new("antigravity"),
            model_id: openproxy_types::ModelId::new("claude-sonnet-4-6"),
            target_format: openproxy_types::TargetFormat::Gemini,
            discovered_at: openproxy_types::now_unix_secs_str().into_boxed_str(),
            ..Default::default()
        },
        api_key: "ya29.test".into(),
        api_key_label: None,
        custom_meta: None,
    };

    let body = bytes::Bytes::from_static(b"{\"test\":true}");
    let mut headers = vec![
        ("User-Agent".to_string(), "legacy-ua".to_string()),
        ("X-Stainless-Retry-Count".to_string(), "1".to_string()),
        ("X-Goog-User-Project".to_string(), "project-id".to_string()),
        (
            "X-Anthropic-Billing-Header".to_string(),
            "billing".to_string(),
        ),
        ("Custom-Header".to_string(), "kept".to_string()),
    ];

    let result_body = adapter
        .finalize_request(
            body.clone(),
            &mut headers,
            TargetFormat::Gemini,
            &ModelId::new("claude-sonnet-4-6"),
            &target,
        )
        .expect("finalize_request");

    assert_eq!(result_body, body, "body bytes must be preserved");

    let find = |key: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(
        find("user-agent"),
        Some(crate::antigravity_headers::hub_user_agent().as_str())
    );
    assert_eq!(find("anthropic-beta"), Some("claude-code-20250219"));
    assert_eq!(find("custom-header"), Some("kept"));
    assert_eq!(find("x-goog-user-project"), None);
    assert_eq!(find("x-stainless-retry-count"), None);
    assert_eq!(find("x-anthropic-billing-header"), None);
}
