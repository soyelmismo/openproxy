use super::*;
use crate::adapters::commandcode::billing::{humanize_commandcode_plan, parse_commandcode_window};
use serde_json::json;

#[test]
fn test_commandcode_dynamic_version() {
    assert_eq!(
        get_commandcode_cli_version(),
        DEFAULT_COMMANDCODE_CLI_VERSION
    );
    set_commandcode_cli_version("2.0.0".into());
    assert_eq!(get_commandcode_cli_version(), "2.0.0");
    set_commandcode_cli_version(DEFAULT_COMMANDCODE_CLI_VERSION.into());
}

#[test]
fn test_commandcode_wrap_request_body() {
    let adapter = CommandCodeGoAdapter::new();
    let body = json!({
        "model": "claude-sonnet-5",
        "messages": [
            { "role": "system", "content": "You are helpful." },
            { "role": "user", "content": "Hello world" }
        ],
        "temperature": 0.7
    });
    let bytes = bytes::Bytes::from(serde_json::to_vec(&body).unwrap());
    let target = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ComboTargetId(1),
            combo_id: openproxy_types::ComboId(1),
            provider_id: openproxy_types::ProviderId::new("commandcodego"),
            account_id: None,
            model_row_id: Some(openproxy_types::ModelRowId(1)),
            sub_combo_id: None,
            priority_order: 0,
            weight: 100,
            active: true,
            rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
        },
        model: openproxy_types::Model {
            row_id: openproxy_types::ModelRowId(1),
            provider_id: openproxy_types::ProviderId::new("commandcodego"),
            target_format: openproxy_types::TargetFormat::CommandCodeGo,
            discovered_at: openproxy_types::now_unix_secs_str().into_boxed_str(),
            expires_at: None,
            model_id: openproxy_types::ModelId::new("claude-sonnet-5"),
            display_name: None,
            context_length: None,
            max_output_tokens: None,
            model_type: "chat".into(),
            family: None,
            input_modalities_json: None,
            output_modalities_json: None,
            capabilities_json: None,
            timeout_overrides_json: None,
            active: true,
            last_test_status: None,
            last_test_at: None,
            custom: false,
            ..Default::default()
        },
        api_key: "dummy".to_string(),
        api_key_label: None,
        custom_meta: None,
    };

    let wrapped = adapter
        .wrap_request_body(
            bytes,
            TargetFormat::CommandCodeGo,
            &ModelId::new("claude-sonnet-5"),
            &target,
        )
        .unwrap();

    let v: Value = serde_json::from_slice(&wrapped).unwrap();
    assert!(v.get("config").is_some());
    assert!(v.get("threadId").is_some());
    let params = v.get("params").unwrap();
    assert_eq!(params["model"].as_str(), Some("claude-sonnet-5"));
    assert_eq!(params["system"].as_str(), Some("You are helpful."));
    assert_eq!(params["stream"].as_bool(), Some(true));
    assert_eq!(params["temperature"].as_f64(), Some(0.7));
}

#[test]
fn test_commandcode_wrap_request_body_with_tool_calls_and_results() {
    let adapter = CommandCodeGoAdapter::new();
    let body = json!({
        "model": "claude-sonnet-5",
        "messages": [
            { "role": "system", "content": "System directive" },
            { "role": "user", "content": "Check files" },
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {
                        "id": "call_abc123",
                        "type": "function",
                        "function": {
                            "name": "list_files",
                            "arguments": "{\"path\":\"/root\"}"
                        }
                    }
                ]
            },
            {
                "role": "tool",
                "tool_call_id": "call_abc123",
                "content": "file1.txt\nfile2.txt"
            },
            { "role": "user", "content": "Now read file1" }
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "list_files",
                    "description": "List directory contents",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" }
                        }
                    }
                }
            }
        ],
        "tool_choice": "auto"
    });
    let bytes = bytes::Bytes::from(serde_json::to_vec(&body).unwrap());
    let target = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ComboTargetId(1),
            combo_id: openproxy_types::ComboId(1),
            provider_id: openproxy_types::ProviderId::new("commandcodego"),
            account_id: None,
            model_row_id: Some(openproxy_types::ModelRowId(1)),
            sub_combo_id: None,
            priority_order: 0,
            weight: 100,
            active: true,
            rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
        },
        model: openproxy_types::Model {
            row_id: openproxy_types::ModelRowId(1),
            provider_id: openproxy_types::ProviderId::new("commandcodego"),
            target_format: openproxy_types::TargetFormat::CommandCodeGo,
            discovered_at: openproxy_types::now_unix_secs_str().into_boxed_str(),
            expires_at: None,
            model_id: openproxy_types::ModelId::new("claude-sonnet-5"),
            display_name: None,
            context_length: None,
            max_output_tokens: None,
            model_type: "chat".into(),
            family: None,
            input_modalities_json: None,
            output_modalities_json: None,
            capabilities_json: None,
            timeout_overrides_json: None,
            active: true,
            last_test_status: None,
            last_test_at: None,
            custom: false,
            ..Default::default()
        },
        api_key: "dummy".to_string(),
        api_key_label: None,
        custom_meta: None,
    };

    let wrapped = adapter
        .wrap_request_body(
            bytes,
            TargetFormat::CommandCodeGo,
            &ModelId::new("claude-sonnet-5"),
            &target,
        )
        .unwrap();

    let v: Value = serde_json::from_slice(&wrapped).unwrap();
    let params = v.get("params").unwrap();
    let msgs = params["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 4);

    // Turn 1: user
    assert_eq!(msgs[0]["role"], "user");
    let u1_blocks = msgs[0]["content"].as_array().unwrap();
    assert_eq!(u1_blocks[0]["type"], "text");
    assert_eq!(u1_blocks[0]["text"], "Check files");

    // Turn 2: assistant with tool-call
    assert_eq!(msgs[1]["role"], "assistant");
    let a_blocks = msgs[1]["content"].as_array().unwrap();
    assert_eq!(a_blocks[0]["type"], "tool-call");
    assert_eq!(a_blocks[0]["toolCallId"], "call_abc123");
    assert_eq!(a_blocks[0]["toolName"], "list_files");
    assert_eq!(a_blocks[0]["input"]["path"], "/root");

    // Turn 3: tool result
    assert_eq!(msgs[2]["role"], "tool");
    let t_blocks = msgs[2]["content"].as_array().unwrap();
    assert_eq!(t_blocks[0]["type"], "tool-result");
    assert_eq!(t_blocks[0]["toolCallId"], "call_abc123");
    assert_eq!(t_blocks[0]["toolName"], "list_files");
    assert_eq!(t_blocks[0]["output"]["type"], "text");
    assert_eq!(t_blocks[0]["output"]["value"], "file1.txt\nfile2.txt");

    // Turn 4: user text
    assert_eq!(msgs[3]["role"], "user");
    let u2_blocks = msgs[3]["content"].as_array().unwrap();
    assert_eq!(u2_blocks[0]["type"], "text");
    assert_eq!(u2_blocks[0]["text"], "Now read file1");

    // Tools: Anthropic format (no "type": "function")
    let tools = params["tools"].as_array().unwrap();
    assert_eq!(tools[0]["name"], "list_files");
    assert!(tools[0].get("type").is_none());
    assert!(tools[0].get("input_schema").is_some());
}

#[test]
fn test_parse_commandcode_window_normalizes_to_percentages() {
    // 5-hour window: used 0.21 out of 3.0 cap -> 7%
    let win_5h = json!({
        "used": 0.21,
        "cap": 3.0,
        "exceeded": false,
        "resetAt": 0
    });
    let (used, limit, reset) = parse_commandcode_window(Some(&win_5h));
    assert_eq!(used, Some(7));
    assert_eq!(limit, Some(100));
    assert_eq!(reset, None);

    // Weekly window: used 0.196657126 out of 6.0 cap -> 3%
    let win_weekly = json!({
        "used": 0.196657126,
        "cap": 6.0,
        "exceeded": false,
        "resetAt": 1789984055925i64
    });
    let (w_used, w_limit, w_reset) = parse_commandcode_window(Some(&win_weekly));
    assert_eq!(w_used, Some(3));
    assert_eq!(w_limit, Some(100));
    assert_eq!(w_reset.as_deref(), Some("1789984055925"));

    // Exceeded window clamps to 100%
    let win_exceeded = json!({
        "used": 3.5,
        "cap": 3.0,
        "exceeded": true,
        "resetAt": "1789984000000"
    });
    let (e_used, e_limit, _) = parse_commandcode_window(Some(&win_exceeded));
    assert_eq!(e_used, Some(100));
    assert_eq!(e_limit, Some(100));

    // Zero cap returns None
    let win_zero = json!({ "used": 0.0, "cap": 0.0 });
    let (z_used, z_limit, _) = parse_commandcode_window(Some(&win_zero));
    assert_eq!(z_used, None);
    assert_eq!(z_limit, None);
}

#[test]
fn test_humanize_commandcode_plan() {
    assert_eq!(
        humanize_commandcode_plan(Some("individual-go"), Some("Monthly: 2% · resets Oct 14")),
        "Command Code · Go · Monthly: 2% · resets Oct 14"
    );

    assert_eq!(
        humanize_commandcode_plan(Some("individual-pro"), None),
        "Command Code · Pro"
    );

    assert_eq!(humanize_commandcode_plan(None, None), "Command Code");
}
