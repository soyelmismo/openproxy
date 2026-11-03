use super::*;
use openproxy_types::{OpenAIMessage, OpenAIRequest};
use serde_json::json;

#[test]
fn test_build_kiro_request_empty() {
    let req = OpenAIRequest {
        messages: vec![],
        ..Default::default()
    };
    let kiro_req = build_kiro_request(&req, None);
    assert_eq!(
        kiro_req
            .conversation_state
            .current_message
            .user_input_message
            .content,
        "(empty)"
    );
    assert!(kiro_req.conversation_state.history.is_none());
}

#[test]
fn test_build_kiro_request_single_user() {
    let req = OpenAIRequest {
        messages: vec![OpenAIMessage {
            role: "user".to_string(),
            content: Some(json!("hello")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        }],
        ..Default::default()
    };
    let kiro_req = build_kiro_request(&req, None);
    assert_eq!(
        kiro_req
            .conversation_state
            .current_message
            .user_input_message
            .content,
        "hello"
    );
    assert!(kiro_req.conversation_state.history.is_none());
}

#[test]
fn test_build_kiro_request_multiple_turns() {
    let req = OpenAIRequest {
        messages: vec![
            OpenAIMessage {
                role: "user".to_string(),
                content: Some(json!("hi")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            },
            OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(json!("hello there")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            },
            OpenAIMessage {
                role: "user".to_string(),
                content: Some(json!("how are you?")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            },
        ],
        ..Default::default()
    };
    let kiro_req = build_kiro_request(&req, None);
    let history = kiro_req.conversation_state.history.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(
        kiro_req
            .conversation_state
            .current_message
            .user_input_message
            .content,
        "how are you?"
    );
}

#[test]
fn test_extract_agentic_request_limits_credit() {
    let data = json!({
        "usageBreakdownList": [{
            "resourceType": "CREDIT",
            "currentUsageWithPrecision": 0.76,
            "usageLimitWithPrecision": 50.0
        }]
    });
    let (used, limit) = extract_agentic_request_limits(&data);
    assert_eq!(used, Some(1));
    assert_eq!(limit, Some(50));
}
