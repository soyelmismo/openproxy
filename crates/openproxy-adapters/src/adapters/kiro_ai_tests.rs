use super::*;
use openproxy_types::{OpenAIMessage, OpenAIRequest};
use serde_json::json;

#[test]
fn test_split_history_empty() {
    let req = OpenAIRequest {
        messages: vec![],
        ..Default::default()
    };
    let (history, current) = split_history(&req);
    assert!(history.is_empty());
    assert!(current.is_none());
}

#[test]
fn test_split_history_single_user() {
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
    let (history, current) = split_history(&req);
    assert!(history.is_empty());
    assert_eq!(current.unwrap().role, "user");
    assert_eq!(current.unwrap().content.as_ref().unwrap(), &json!("hello"));
}

#[test]
fn test_split_history_multiple_turns() {
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
    let (history, current) = split_history(&req);
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].role, "user");
    assert_eq!(history[1].role, "assistant");

    let current_msg = current.unwrap();
    assert_eq!(current_msg.role, "user");
    assert_eq!(
        current_msg.content.as_ref().unwrap(),
        &json!("how are you?")
    );
}

#[test]
fn test_split_history_no_user_message() {
    let req = OpenAIRequest {
        messages: vec![
            OpenAIMessage {
                role: "system".to_string(),
                content: Some(json!("You are helpful.")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            },
            OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(json!("I am ready.")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            },
        ],
        ..Default::default()
    };
    // fallback to using the last message when there is no user message.
    let (history, current) = split_history(&req);
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].role, "system");

    let current_msg = current.unwrap();
    assert_eq!(current_msg.role, "assistant");
    assert_eq!(current_msg.content.as_ref().unwrap(), &json!("I am ready."));
}
