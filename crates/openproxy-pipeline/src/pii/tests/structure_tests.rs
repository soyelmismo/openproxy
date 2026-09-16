use super::super::*;
use openproxy_types::config::PiiEntity;
use openproxy_types::message::{OpenAIChoice, OpenAIMessage, OpenAIResponse};

fn msg(role: &str, content: &str) -> OpenAIMessage {
    OpenAIMessage {
        role: role.into(),
        content: Some(serde_json::Value::String(content.into())),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: Default::default(),
    }
}

#[test]
fn test_preservation_of_code_blocks_urls_json_keys_and_markdown() {
    let engine = PiiEngine::new(&PiiEntity::ALL);
    let mut session = PiiSession::new(true);
    let complex_text = "# API Doc\n```python\nsupport_email = \"internal@test.com\"\nclient = OpenAI(api_key=\"sk-proj-secretinsidecodeblock12345\")\n```\n`secret_key=\"sk-proj-inlinecode12345\"` https://api.service.com/users/alice@example.com\n{\"user_email\": \"real_user@domain.com\", \"user_ip\": \"10.0.0.1\"}\nPlease email real_user@domain.com";

    let red = engine.redact_text(complex_text, &mut session);
    assert!(
        !red.contains("internal@test.com")
            && !red.contains("sk-proj-secretinsidecodeblock12345")
            && !red.contains("sk-proj-inlinecode12345")
    );
    assert!(
        red.contains("https://api.service.com/users/alice@example.com")
            && red.contains("\"user_email\": ")
            && (red.contains("10.240.0.1"))
    );
    assert_eq!(session.restore_text(&red), complex_text);
}

#[test]
fn test_all_messages_including_system_and_user_tool_redacted() {
    let engine = PiiEngine::new(&PiiEntity::ALL);
    let mut session = PiiSession::new(true);
    let messages = vec![
        msg(
            "system",
            "System prompt with system@admin.internal instructions.",
        ),
        msg(
            "user",
            "My email is customer@gmail.com and phone is +1-800-555-0199.",
        ),
        OpenAIMessage {
            role: "tool".into(),
            content: Some(serde_json::json!(
                "Tool output: result for customer@gmail.com."
            )),
            name: None,
            tool_call_id: Some("call_1".into()),
            tool_calls: None,
            extra: Default::default(),
        },
    ];
    let red = engine.redact_messages(&messages, &mut session);
    assert_eq!(
        red[0].content,
        Some(serde_json::json!(
            "System prompt with alex.turner1@fastmail.com instructions."
        ))
    );
    assert_eq!(
        red[1].content,
        Some(serde_json::json!(
            "My email is jordan.lee2@outlook.com and phone is +1-202-555-0111."
        ))
    );
    assert_eq!(
        red[2].content,
        Some(serde_json::json!(
            "Tool output: result for jordan.lee2@outlook.com."
        ))
    );
}

#[test]
fn test_nested_markdown_code_fences_preservation() {
    let engine = PiiEngine::new(&PiiEntity::ALL);
    let mut session = PiiSession::new(true);
    let nested = "````markdown\n```rust\nlet email = \"alice@example.com\";\n```\n````";
    let red = engine.redact_text(nested, &mut session);
    assert!(!red.contains("alice@example.com") && red.contains("alex.turner1@fastmail.com"));
    assert_eq!(session.restore_text(&red), nested);
}

#[test]
fn test_tool_calls_and_tool_messages_redaction_roundtrip() {
    let engine = PiiEngine::new(&PiiEntity::ALL);
    let mut session = PiiSession::new(true);
    let messages = vec![
        msg("system", "You are an AI assistant. Call tools when needed."),
        msg("user", "Please query account for alice@example.com"),
        OpenAIMessage {
            role: "assistant".into(),
            content: None,
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![serde_json::json!({
                "id": "call_abc123", "type": "function",
                "function": { "name": "lookup_user", "arguments": "{\"email\": \"alice@example.com\", \"api_key\": \"sk-test1234567890abcdef1234567890abcdef\"}" }
            })]),
            extra: Default::default(),
        },
        OpenAIMessage {
            role: "tool".into(),
            content: Some(serde_json::json!(
                "{\"status\": \"success\", \"user_phone\": \"+1-555-019-2834\"}"
            )),
            name: None,
            tool_call_id: Some("call_abc123".into()),
            tool_calls: None,
            extra: Default::default(),
        },
    ];

    let red = engine.redact_messages(&messages, &mut session);
    let fake_email = session.forward.get("alice@example.com").cloned().unwrap();
    let fake_key = session
        .forward
        .get("sk-test1234567890abcdef1234567890abcdef")
        .cloned()
        .unwrap();
    let fake_phone = session.forward.get("+1-555-019-2834").cloned().unwrap();

    let args = red[2].tool_calls.as_ref().unwrap()[0]["function"]["arguments"]
        .as_str()
        .unwrap();
    assert!(
        args.contains(&fake_email)
            && args.contains(&fake_key)
            && !args.contains("alice@example.com")
    );

    let mut response = OpenAIResponse {
        id: "resp_test".into(),
        object: "chat.completion".into(),
        created: 123456,
        model: "test-model".into(),
        choices: vec![OpenAIChoice {
            index: 0,
            message: OpenAIMessage {
                role: "assistant".into(),
                content: Some(serde_json::json!(format!(
                    "Found user {fake_email} with phone {fake_phone}"
                ))),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![serde_json::json!({
                    "id": "call_next", "type": "function",
                    "function": { "name": "notify", "arguments": format!("{{\"target\": \"{fake_email}\", \"key\": \"{fake_key}\"}}") }
                })]),
                extra: Default::default(),
            },
            finish_reason: Some("tool_calls".into()),
        }],
        usage: None,
    };
    session.restore_openai_response(&mut response);
    assert_eq!(
        response.choices[0]
            .message
            .content
            .as_ref()
            .unwrap()
            .as_str()
            .unwrap(),
        "Found user alice@example.com with phone +1-555-019-2834"
    );
}

#[test]
fn test_tool_calls_nested_json_arguments_preserves_syntax() {
    let engine = PiiEngine::new(&[PiiEntity::Secret]);
    let mut session = PiiSession::new(true);
    let original_args = r#"{"new_string":"def build_subtitle_manifest(files: list, family_token: str) -> dict:\n    url = f\"https://v.listo.click/serve?id={f['ID']}&token=my_real_secret_token_12345678\"\n    return url"}"#;
    let msg_item = OpenAIMessage {
        role: "assistant".into(),
        content: None,
        name: None,
        tool_call_id: None,
        tool_calls: Some(vec![serde_json::json!({
            "id": "call_patch_123", "type": "function", "function": { "name": "patch", "arguments": original_args }
        })]),
        extra: serde_json::Map::new(),
    };

    let red = engine.redact_messages(&[msg_item], &mut session);
    let parsed: serde_json::Value = serde_json::from_str(
        red[0].tool_calls.as_ref().unwrap()[0]["function"]["arguments"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let new_str = parsed["new_string"].as_str().unwrap();
    assert!(
        !new_str.contains("my_real_secret_token_12345678")
            && new_str.contains("sec_")
            && new_str.contains("family_token: str")
    );

    let mut resp = OpenAIResponse {
        id: "resp_123".into(),
        object: "chat.completion".into(),
        created: 1234567890,
        model: "gpt-4".into(),
        choices: vec![OpenAIChoice {
            index: 0,
            message: red[0].clone(),
            finish_reason: Some("tool_calls".into()),
        }],
        usage: None,
    };
    session.restore_openai_response(&mut resp);
    let restored_parsed: serde_json::Value = serde_json::from_str(
        resp.choices[0].message.tool_calls.as_ref().unwrap()[0]["function"]["arguments"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        restored_parsed["new_string"].as_str().unwrap(),
        serde_json::from_str::<serde_json::Value>(original_args).unwrap()["new_string"]
            .as_str()
            .unwrap()
    );
}

#[test]
fn test_url_sensitive_param_and_template_hardened() {
    let engine = PiiEngine::new(&[PiiEntity::Secret]);
    let mut session = PiiSession::new(true);
    let code = "def fetch(family_token: str):\n    url = f\"https://v.listo.click/serve?id={f['ID']}&token={family_token}\"\n    return url";
    assert_eq!(engine.redact_text(code, &mut session), code);

    let json_esc = r#"\"url\": \"https://example.com/api?token=real_secret_key_89012345\",\n"#;
    let red_esc = engine.redact_text(json_esc, &mut session);
    assert!(
        !red_esc.contains("real_secret_key_89012345")
            && red_esc.contains("token=sec_")
            && red_esc.contains(r#"\",\n"#)
    );
    assert_eq!(session.restore_text(&red_esc), json_esc);
}

#[test]
fn test_code_block_and_backtick_redacts_emails_ips_and_secrets_reversibly() {
    let engine = PiiEngine::new(&PiiEntity::ALL);
    let mut session = PiiSession::new(true);
    let input = "\nSoy `hermeona@navi.land`. Envío correos con Himalaya.\nBash:\n```bash\nsshpass -p0 ssh root@100.66.0.2 -p 2369\n```\nServers: | **rot** (PC Miguel) | `100.109.155.87` |\nSSH: `sshpass -p0 ssh rot@100.109.155.87`. Aparece como `miguel-pc-linux`\n";
    let red = engine.redact_text(input, &mut session);
    assert!(
        !red.contains("hermeona@navi.land")
            && !red.contains("100.66.0.2")
            && !red.contains("-p0")
            && !red.contains("PC Miguel")
    );
    assert!(red.contains("`miguel-pc-linux`"));
    assert_eq!(session.restore_text(&red), input);
}

#[test]
fn test_telegram_chat_and_user_numeric_ids_remain_valid_json_integers_and_strings() {
    let engine = PiiEngine::new(&[PiiEntity::Secret]);
    let mut session = PiiSession::new(true);
    let input = r#"{"platform": "telegram", "chat_id": "6077244180", "chat_type": "dm", "user_id": "6077244180", "message_id": "88706", "numeric_chat": 6077244180}"#;
    let red = engine.redact_text(input, &mut session);
    assert!(!red.contains("6077244180") && red.contains("89410294"));
    let parsed: serde_json::Value = serde_json::from_str(&red).unwrap();
    assert_eq!(parsed["chat_id"].as_str().unwrap().len(), 10);
    assert_eq!(
        parsed["chat_id"].as_str().unwrap(),
        parsed["user_id"].as_str().unwrap()
    );
    assert!(parsed["numeric_chat"].as_u64().unwrap() >= 1_000_000_000);
    assert_eq!(session.restore_text(&red), input);
}
