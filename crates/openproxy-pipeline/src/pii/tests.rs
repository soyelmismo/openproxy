use super::*;
use openproxy_types::config::PiiEntity;
use openproxy_types::message::{OpenAIChoice, OpenAIMessage, OpenAIResponse};
use std::collections::HashMap;
use std::time::Instant;

#[test]
fn test_luhn_validation() {
    // Valid test cards
    assert!(luhn_check("4532015112830366")); // Visa
    assert!(luhn_check("378282246310005")); // Amex (15 digits)
    assert!(luhn_check("5555555555554444")); // Mastercard
    assert!(luhn_check("6011111111111117")); // Discover

    // Invalid cards (checksum fails)
    assert!(!luhn_check("4532015112830367"));
    assert!(!luhn_check("378282246310006"));
    assert!(!luhn_check("1234567812345678"));
    assert!(!luhn_check("0000000000000000")); // All zeroes

    // Length boundaries
    assert!(!luhn_check("123456789012")); // 12 digits (too short)
    assert!(!luhn_check("12345678901234567890")); // 20 digits (too long)
}

#[test]
fn test_email_redaction() {
    let engine = PiiEngine::new(&[PiiEntity::Email]);
    let mut session = PiiSession::new(true);

    let input = "Contact alice@example.com or support.team+prod@company.co.uk for help.";
    let redacted = engine.redact_text(input, &mut session);

    assert_eq!(
        redacted,
        "Contact alex.turner1@fastmail.com or jordan.lee2@outlook.com for help."
    );
    assert_eq!(session.restore_text(&redacted), input);
}

#[test]
fn test_phone_redaction() {
    let engine = PiiEngine::new(&[PiiEntity::Phone]);
    let mut session = PiiSession::new(true);

    let input = "Call international +1-800-555-0199 or regional (555) 123-4567 or 555-987-6543.";
    let redacted = engine.redact_text(input, &mut session);

    assert_eq!(
        redacted,
        "Call international +1-202-555-0111 or regional +1-202-555-0112 or +1-202-555-0113."
    );
    assert_eq!(session.restore_text(&redacted), input);

    // Ensure dates like 2024-05-12 are NOT redacted as phones
    let date_input = "Release date was 2024-05-12 and timestamp 14:30:00.";
    let date_redacted = engine.redact_text(date_input, &mut session);
    assert_eq!(date_redacted, date_input);

    // US 1-800 without leading +
    let us_1800 = "Call 1-800-555-0199 for support.";
    let redacted_1800 = engine.redact_text(us_1800, &mut session);
    assert_eq!(redacted_1800, "Call +1-202-555-0114 for support.");
    assert_eq!(session.restore_text(&redacted_1800), us_1800);
}

#[test]
fn test_ip_address_redaction() {
    let engine = PiiEngine::new(&[PiiEntity::Ip]);
    let mut session = PiiSession::new(true);

    let input = "Server at 192.168.1.1 and IPv6 2001:0db8:85a3:0000:0000:8a2e:0370:7334 or ::1.";
    let redacted = engine.redact_text(input, &mut session);

    assert_eq!(
        redacted,
        "Server at 10.240.0.1 and IPv6 fd00:10:240::2 or fd00:10:240::3."
    );
    assert_eq!(session.restore_text(&redacted), input);

    // Semver like v1.2.3.4 should NOT be redacted as IP
    let semver_input = "Upgraded to v1.2.3.4 in prod.";
    assert_eq!(engine.redact_text(semver_input, &mut session), semver_input);

    // Compressed IPv6
    let compressed_ipv6 = "Connect to 2001:db8::1 or fe80::1 now.";
    let redacted_comp = engine.redact_text(compressed_ipv6, &mut session);
    assert_eq!(
        redacted_comp,
        "Connect to fd00:10:240::4 or fd00:10:240::5 now."
    );
    assert_eq!(session.restore_text(&redacted_comp), compressed_ipv6);
}

#[test]
fn test_credit_card_redaction_and_invalid_luhn_ignored() {
    let engine = PiiEngine::new(&[PiiEntity::CreditCard]);
    let mut session = PiiSession::new(true);

    // Valid card
    let input_valid = "Charged 4532-0151-1283-0366 on Visa.";
    let redacted_valid = engine.redact_text(input_valid, &mut session);
    assert_eq!(redacted_valid, "Charged 4532 0151 1283 1018 on Visa.");
    assert_eq!(session.restore_text(&redacted_valid), input_valid);

    // Invalid card (checksum fails) -> MUST BE IGNORED
    let input_invalid = "Bad card 4532-0151-1283-0367 was declined.";
    let redacted_invalid = engine.redact_text(input_invalid, &mut session);
    assert_eq!(redacted_invalid, input_invalid);

    // 20-digit hyphenated product key / serial number -> MUST NOT BE REDACTED
    let product_key = "Product serial 4532-0151-1283-0366-1234 was activated.";
    let redacted_key = engine.redact_text(product_key, &mut session);
    assert_eq!(redacted_key, product_key);
}

#[test]
fn test_secrets_and_api_keys_redaction() {
    let engine = PiiEngine::new(&[PiiEntity::Secret]);
    let mut session = PiiSession::new(true);

    let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
    let input = format!(
        "Keys: Bearer secret_bearer_token_1234567890, OpenAI sk-proj-1234567890abcdefghijklmn, AWS AKIAIOSFODNN7EXAMPLE, GitHub ghp_123456789012345678901234567890123456, JWT: {jwt}."
    );
    let redacted = engine.redact_text(&input, &mut session);

    assert!(redacted.contains("Bearer sec_8f7b6c5d4e3a2b109f8e7d6c5b4a3f"));
    assert!(redacted.contains("sk-proj-x7K9mP2vL4wN8qR1tY6uI3oE5aB0zD"));
    assert!(redacted.contains("ghp_7xK9mP2vL4wN8qR1tY6uI3oE5aB0zD"));
    assert!(!redacted.contains("secret_bearer_token_1234567890"));
    assert!(!redacted.contains("sk-proj-1234567890abcdefghijklmn"));
    assert!(!redacted.contains("AKIAIOSFODNN7EXAMPLE"));
    assert!(!redacted.contains("ghp_123456789012345678901234567890123456"));
    assert!(!redacted.contains(jwt));

    assert_eq!(session.restore_text(&redacted), input);
}

#[test]
fn test_person_names_heuristic() {
    let engine = PiiEngine::new(&[PiiEntity::Person]);
    let mut session = PiiSession::new(true);

    // Honorific
    let input_honorific = "Consult Dr. Gregory House or Mr. John Doe immediately.";
    let redacted_hon = engine.redact_text(input_honorific, &mut session);
    assert_eq!(
        redacted_hon,
        "Consult Dr. Alex Vance (P1) or Mr. David Chen (P2) immediately."
    );
    assert_eq!(session.restore_text(&redacted_hon), input_honorific);

    // Capitalized sequence
    let input_seq = "Please contact Robert Johnson regarding the invoice.";
    let redacted_seq = engine.redact_text(input_seq, &mut session);
    assert_eq!(
        redacted_seq,
        "Please contact Sarah Jenkins (P3) regarding the invoice."
    );
    assert_eq!(session.restore_text(&redacted_seq), input_seq);

    // Single name after honorific
    let single_name = "Please check with Dr. Watson or Mr. Smith today.";
    let redacted_single = engine.redact_text(single_name, &mut session);
    assert_eq!(
        redacted_single,
        "Please check with Dr. Michael Sterling (P4) or Mr. Elena Mercer (P5) today."
    );
    assert_eq!(session.restore_text(&redacted_single), single_name);

    // Technical / grammar words must NOT be redacted as person
    let technical = "SQLite Database uses B-Tree index. Docker Kubernetes orchestrates containers.";
    let redacted_tech = engine.redact_text(technical, &mut session);
    assert_eq!(redacted_tech, technical);
}

#[test]
fn test_preservation_of_code_blocks_urls_json_keys_and_markdown() {
    let engine = PiiEngine::new(&PiiEntity::ALL);
    let mut session = PiiSession::new(true);

    let complex_text = r#"# API Documentation

Here is the source code:
```python
import os
# Do not touch this email inside code block
support_email = "internal@test.com"
client = OpenAI(api_key="sk-proj-secretinsidecodeblock12345")
```

Also check the inline config `secret_key="sk-proj-inlinecode12345"` and visit https://api.service.com/users/alice@example.com for info.

JSON payload:
{"user_email": "real_user@domain.com", "user_ip": "10.0.0.1"}

Please email real_user@domain.com for questions."#;

    let redacted = engine.redact_text(complex_text, &mut session);

    // Sensitive entities inside code blocks and inline code ARE safely pseudonymized
    assert!(!redacted.contains("internal@test.com"));
    assert!(!redacted.contains("sk-proj-secretinsidecodeblock12345"));
    assert!(!redacted.contains("sk-proj-inlinecode12345"));

    // Code structure and valid placeholders are preserved inside code blocks
    assert!(redacted.contains(r#"support_email = ""#));
    assert!(redacted.contains(r#"client = OpenAI(api_key=""#));
    assert!(redacted.contains(r#"`secret_key=""#));

    // URL preserved
    assert!(redacted.contains("https://api.service.com/users/alice@example.com"));

    // JSON keys preserved ("user_email" and "user_ip" intact)
    assert!(redacted.contains(r#""user_email": "#));
    assert!(
        redacted.contains(r#""user_ip": "10.240.0.1""#)
            || redacted.contains(r#""user_ip": 10.240.0.1"#)
    );

    // Normal prose email redacted
    assert!(!redacted.contains("real_user@domain.com"));

    // Restoration recovers original
    let restored = session.restore_text(&redacted);
    assert_eq!(restored, complex_text);
}

#[test]
fn test_all_messages_including_system_and_user_tool_redacted() {
    let engine = PiiEngine::new(&PiiEntity::ALL);
    let mut session = PiiSession::new(true);

    let messages = vec![
        OpenAIMessage {
            role: "system".to_string(),
            content: Some(serde_json::json!(
                "System prompt with system@admin.internal instructions."
            )),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
        OpenAIMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!(
                "My email is customer@gmail.com and phone is +1-800-555-0199."
            )),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
        OpenAIMessage {
            role: "tool".to_string(),
            content: Some(serde_json::json!(
                "Tool output: result for customer@gmail.com."
            )),
            name: None,
            tool_call_id: Some("call_1".to_string()),
            tool_calls: None,
            extra: Default::default(),
        },
    ];

    let redacted_msgs = engine.redact_messages(&messages, &mut session);

    // System prompt has sensitive email redacted
    assert_eq!(
        redacted_msgs[0].content,
        Some(serde_json::json!(
            "System prompt with alex.turner1@fastmail.com instructions."
        ))
    );

    // User message is redacted
    assert_eq!(
        redacted_msgs[1].content,
        Some(serde_json::json!(
            "My email is jordan.lee2@outlook.com and phone is +1-202-555-0111."
        ))
    );

    // Tool message is redacted with STABLE placeholder (jordan.lee2@outlook.com)
    assert_eq!(
        redacted_msgs[2].content,
        Some(serde_json::json!(
            "Tool output: result for jordan.lee2@outlook.com."
        ))
    );
}

#[test]
fn test_unary_response_roundtrip() {
    let engine = PiiEngine::new(&PiiEntity::ALL);
    let mut session = PiiSession::new(true);

    let user_msg = "Please verify my card 4532-0151-1283-0366 and email me at test@example.com.";
    let redacted = engine.redact_text(user_msg, &mut session);

    assert_eq!(
        redacted,
        "Please verify my card 4532 0151 1283 1018 and email me at alex.turner1@fastmail.com."
    );

    // Upstream LLM replies using the placeholders
    let mut resp = OpenAIResponse {
        id: "resp-1".to_string(),
        object: "chat.completion".to_string(),
        created: 1_700_000_000,
        model: "test-model".to_string(),
        choices: vec![OpenAIChoice {
            index: 0,
            message: OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::json!(
                    "Verified card 4532 0151 1283 1018. Confirmation sent to alex.turner1@fastmail.com."
                )),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage: None,
    };

    session.restore_openai_response(&mut resp);

    let restored_content = resp.choices[0]
        .message
        .content
        .as_ref()
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(
        restored_content,
        "Verified card 4532-0151-1283-0366. Confirmation sent to test@example.com."
    );
}

#[test]
fn test_streaming_window_replacer_split_across_chunks() {
    let mut map = HashMap::new();
    map.insert("<EMAIL_1>".to_string(), "alice@example.com".to_string());
    map.insert("<PHONE_1>".to_string(), "+1-800-555-0199".to_string());

    let mut replacer = StreamingWindowReplacer::new(map);

    // Scenario 1: Chunk 1 has "<EMA", Chunk 2 has "IL_1>"
    let chunk1 = "Send confirmation to <EMA";
    let chunk2 = "IL_1> right now.";

    let out1 = replacer.process(chunk1);
    assert_eq!(out1, "Send confirmation to ");

    let out2 = replacer.process(chunk2);
    assert_eq!(out2, "alice@example.com right now.");

    assert_eq!(replacer.flush(), "");

    // Scenario 2: Placeholder split into 3 tiny chunks: "<", "PHONE_", "1>"
    let c1 = "Call me at <";
    let c2 = "PHONE_";
    let c3 = "1> today.";

    assert_eq!(replacer.process(c1), "Call me at ");
    assert_eq!(replacer.process(c2), "");
    assert_eq!(replacer.process(c3), "+1-800-555-0199 today.");
    assert_eq!(replacer.flush(), "");

    // Scenario 3: UTF-8 multibyte characters and 4-chunk split
    let u1 = "こんにちは 🚀 <E";
    let u2 = "MA";
    let u3 = "IL_";
    let u4 = "1> 世界";

    assert_eq!(replacer.process(u1), "こんにちは 🚀 ");
    assert_eq!(replacer.process(u2), "");
    assert_eq!(replacer.process(u3), "");
    assert_eq!(replacer.process(u4), "alice@example.com 世界");
    assert_eq!(replacer.flush(), "");
}

#[test]
fn test_streaming_window_replacer_non_placeholders() {
    let mut map = HashMap::new();
    map.insert("<EMAIL_1>".to_string(), "alice@example.com".to_string());

    let mut replacer = StreamingWindowReplacer::new(map);

    // HTML tag and math operators with '<'
    assert_eq!(
        replacer.process("Value is < 5 and < 10. "),
        "Value is < 5 and < 10. "
    );
    assert_eq!(replacer.process("Use <div> tag. "), "Use <div> tag. ");
    assert_eq!(
        replacer.process("Contact <EMAIL_1>."),
        "Contact alice@example.com."
    );
    assert_eq!(replacer.flush(), "");

    // ADVERSARIAL: '<' followed by placeholder in the same string!
    assert_eq!(
        replacer.process("if a < b then contact <EMAIL_1>."),
        "if a < b then contact alice@example.com."
    );
    // ADVERSARIAL: nested angle brackets
    assert_eq!(
        replacer.process("Email is <<EMAIL_1>>."),
        "Email is <alice@example.com>."
    );
}

#[test]
fn test_streaming_sse_framing_preservation() {
    let mut map = HashMap::new();
    map.insert("<EMAIL_1>".to_string(), "alice@example.com".to_string());

    let mut sse_replacer = PiiRestorationStage::from_mapping(map);

    // Chunk 1 has partial placeholder "<EMA"
    let frame1 = "data: {\"choices\":[{\"delta\":{\"content\":\"Contact <EMA\"}}]}\n\n";
    let out1 = sse_replacer.process_frame(frame1);
    assert_eq!(
        out1,
        "data: {\"choices\":[{\"delta\":{\"content\":\"Contact \"}}]}\n\n"
    );

    // Chunk 2 has remainder "IL_1>"
    let frame2 = "data: {\"choices\":[{\"delta\":{\"content\":\"IL_1> please.\"}}]}\n\n";
    let out2 = sse_replacer.process_frame(frame2);
    assert_eq!(
        out2,
        "data: {\"choices\":[{\"delta\":{\"content\":\"alice@example.com please.\"}}]}\n\n"
    );

    // Chunk 3 is [DONE]
    let frame3 = "data: [DONE]\n\n";
    let out3 = sse_replacer.process_frame(frame3);
    assert_eq!(out3, "data: [DONE]\n\n");
}

#[test]
fn test_latency_overhead_on_50kb_payload() {
    let engine = PiiEngine::new(&PiiEntity::ALL);

    // Construct 50KB payload with repeated paragraphs, code blocks, URLs, and scattered PII
    let paragraph = r#"In software engineering, testing email addresses like alice@example.com and phone +1-800-555-0199
requires care. You can visit https://service.com/dashboard and consult Dr. Gregory House for assistance.
Also make sure server at 192.168.1.50 is operational and API key sk-proj-1234567890abcdefghijklmn is safe.
```rust
fn main() {
    let email = "code_email@not_redacted.org";
    println!("Hello {}", email);
}
```
"#;
    let repetitions = 50 * 1024 / paragraph.len() + 1;
    let big_payload = paragraph.repeat(repetitions);
    assert!(big_payload.len() >= 50 * 1024);

    // Warmup
    for _ in 0..3 {
        let mut s = PiiSession::new(true);
        let _ = engine.redact_text(&big_payload, &mut s);
    }

    // Benchmark 50KB
    let iters = 10;
    let mut total_redact = std::time::Duration::ZERO;
    let mut total_restore = std::time::Duration::ZERO;

    for _ in 0..iters {
        let mut s = PiiSession::new(true);
        let t0 = Instant::now();
        let red = engine.redact_text(&big_payload, &mut s);
        total_redact += t0.elapsed();

        let t1 = Instant::now();
        let res = s.restore_text(&red);
        total_restore += t1.elapsed();

        assert_eq!(res, big_payload);
    }

    let avg_redact_50kb = total_redact / iters;
    let avg_restore_50kb = total_restore / iters;
    println!("\n=== BENCHMARK PII (50 KB / ~13k tokens) ===");
    println!("Avg Redaction Latency : {avg_redact_50kb:?}");
    println!("Avg Restoration Latency: {avg_restore_50kb:?}");

    // Benchmark 2KB standard prompt
    let prompt_2kb = paragraph.repeat(4);
    let mut total_redact_2kb = std::time::Duration::ZERO;
    let mut total_restore_2kb = std::time::Duration::ZERO;
    let iters_2kb = 50;

    for _ in 0..iters_2kb {
        let mut s = PiiSession::new(true);
        let t0 = Instant::now();
        let red = engine.redact_text(&prompt_2kb, &mut s);
        total_redact_2kb += t0.elapsed();

        let t1 = Instant::now();
        let res = s.restore_text(&red);
        total_restore_2kb += t1.elapsed();

        assert_eq!(res, prompt_2kb);
    }

    let avg_redact_2kb = total_redact_2kb / iters_2kb;
    let avg_restore_2kb = total_restore_2kb / iters_2kb;
    println!("=== BENCHMARK PII (2 KB / ~500 tokens) ===");
    println!("Avg Redaction Latency : {avg_redact_2kb:?}");
    println!("Avg Restoration Latency: {avg_restore_2kb:?}\n");

    #[cfg(not(debug_assertions))]
    assert!(
        avg_redact_50kb.as_millis() < 25,
        "Avg redaction in release mode took too long: {avg_redact_50kb:?}"
    );
    #[cfg(debug_assertions)]
    assert!(
        avg_redact_50kb.as_millis() < 300,
        "Avg redaction in debug mode took too long: {avg_redact_50kb:?}"
    );
}

#[test]
fn test_pipeline_roundtrip_unary_and_streaming() {
    let cfg = openproxy_types::config::PiiConfig {
        pii_enabled: true,
        pii_reversible: true,
        pii_redact_logs: true,
        pii_entities: PiiEntity::ALL.to_vec(),
    };
    let engine = PiiEngine::from_config(&cfg);
    let mut session = PiiSession::new(cfg.pii_reversible);

    // Inbound: User message contains sensitive email and API key
    let original_msg =
        "Please verify my email alice@example.com with key sk-proj-1234567890abcdefghijklmn.";
    let msgs = vec![OpenAIMessage {
        role: "user".into(),
        content: Some(serde_json::Value::String(original_msg.to_string())),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: Default::default(),
    }];

    let redacted = engine.redact_messages(&msgs, &mut session);
    let redacted_content = match &redacted[0].content {
        Some(serde_json::Value::String(s)) => s.as_str(),
        _ => panic!("expected string content"),
    };

    assert!(!redacted_content.contains("alice@example.com"));
    assert!(!redacted_content.contains("sk-proj-1234567890abcdefghijklmn"));
    let fake_email = session.forward.get("alice@example.com").cloned().unwrap();
    let fake_key = session
        .forward
        .get("sk-proj-1234567890abcdefghijklmn")
        .cloned()
        .unwrap();

    assert!(redacted_content.contains(&fake_email));
    assert!(redacted_content.contains(&fake_key));

    // Outbound Unary: Upstream LLM replies echoing the placeholders
    let mut upstream_unary = OpenAIResponse {
        id: "chatcmpl-test".into(),
        object: "chat.completion".into(),
        created: 12345,
        model: "gpt-4".into(),
        choices: vec![openproxy_types::OpenAIChoice {
            index: 0,
            message: OpenAIMessage {
                role: "assistant".into(),
                content: Some(serde_json::Value::String(format!(
                    "Confirmed receipt for {fake_email} using key {fake_key}."
                ))),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            },
            finish_reason: Some("stop".into()),
        }],
        usage: None,
    };

    session.restore_openai_response(&mut upstream_unary);
    let restored_content = match &upstream_unary.choices[0].message.content {
        Some(serde_json::Value::String(s)) => s.as_str(),
        _ => panic!("expected string content"),
    };
    assert_eq!(
        restored_content,
        "Confirmed receipt for alice@example.com using key sk-proj-1234567890abcdefghijklmn."
    );

    // Outbound Streaming SSE: Upstream streams with placeholders split across chunks
    let mut sse_replacer = PiiRestorationStage::new(&session);

    let (email_p1, email_p2) = fake_email.split_at(5);
    let (key_p1, key_p2) = fake_key.split_at(8);

    let chunk1 = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"Confirmed receipt for {email_p1}\"}}}}]}}\n\n"
    );
    let chunk2 = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{email_p2} using key {key_p1}\"}}}}]}}\n\n"
    );
    let chunk3 = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{key_p2} right now.\"}}}}]}}\n\n"
    );
    let chunk4 = "data: [DONE]\n\n";

    let out1 = sse_replacer.process_frame(&chunk1);
    let out2 = sse_replacer.process_frame(&chunk2);
    let out3 = sse_replacer.process_frame(&chunk3);
    let out4 = sse_replacer.process_frame(chunk4);

    assert_eq!(
        out1,
        "data: {\"choices\":[{\"delta\":{\"content\":\"Confirmed receipt for \"}}]}\n\n"
    );
    assert_eq!(
        out2,
        "data: {\"choices\":[{\"delta\":{\"content\":\"alice@example.com using key \"}}]}\n\n"
    );
    assert_eq!(
        out3,
        "data: {\"choices\":[{\"delta\":{\"content\":\"sk-proj-1234567890abcdefghijklmn right now.\"}}]}\n\n"
    );
    assert_eq!(out4, "data: [DONE]\n\n");
}

#[test]
fn test_pii_non_reversible_behavior() {
    let cfg = openproxy_types::config::PiiConfig {
        pii_enabled: true,
        pii_reversible: false, // Non-reversible mode
        pii_redact_logs: true,
        pii_entities: vec![PiiEntity::Email],
    };
    let engine = PiiEngine::from_config(&cfg);
    let mut session = PiiSession::new(cfg.pii_reversible);

    let msgs = vec![OpenAIMessage {
        role: "user".into(),
        content: Some(serde_json::Value::String("Email is bob@corp.io".into())),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: Default::default(),
    }];

    let redacted = engine.redact_messages(&msgs, &mut session);
    assert!(format!("{redacted:?}").contains("alex.turner1@fastmail.com"));

    // In non-reversible mode, reverse map is not populated / restoration does nothing
    let mut resp = OpenAIResponse {
        id: "chatcmpl-test".into(),
        object: "chat.completion".into(),
        created: 12345,
        model: "gpt-4".into(),
        choices: vec![openproxy_types::OpenAIChoice {
            index: 0,
            message: OpenAIMessage {
                role: "assistant".into(),
                content: Some(serde_json::Value::String(
                    "Noted alex.turner1@fastmail.com".into(),
                )),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            },
            finish_reason: Some("stop".into()),
        }],
        usage: None,
    };

    session.restore_openai_response(&mut resp);
    let content = match &resp.choices[0].message.content {
        Some(serde_json::Value::String(s)) => s.as_str(),
        _ => panic!("expected string content"),
    };
    assert_eq!(content, "Noted alex.turner1@fastmail.com");
}

#[test]
fn test_multi_frame_sse_chunk_reconstitution() {
    let mut map = HashMap::new();
    map.insert("<EMAIL_1>".to_string(), "carol@domain.org".to_string());
    let mut replacer = PiiRestorationStage::from_mapping(map);

    let multi_frame = "data: {\"choices\":[{\"delta\":{\"content\":\"Contact <EMAIL_1>\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"finish_reason\":\"stop\"}}]}\n\n";
    let processed = replacer.process_frame(multi_frame);
    assert!(processed.contains("carol@domain.org"));
    assert!(processed.contains("\"finish_reason\":\"stop\""));
}

#[test]
fn test_streaming_tool_call_arguments_restoration() {
    let mut map = HashMap::new();
    map.insert("<EMAIL_1>".to_string(), "support@domain.org".to_string());
    let mut replacer = PiiRestorationStage::from_mapping(map);

    let frame = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"to\\\": \\\"<EMAIL_1>\\\"}\"}}]}}]}\n\n";
    let processed = replacer.process_frame(frame);
    assert!(
        processed.contains("support@domain.org"),
        "Expected restored email in tool call arguments, got: {processed}"
    );
}

#[test]
fn test_crlf_multi_event_sse_chunk_processing() {
    let mut map = HashMap::new();
    map.insert("<EMAIL_1>".to_string(), "crlf_user@example.com".to_string());
    let mut replacer = PiiRestorationStage::from_mapping(map);

    let crlf_chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"Notice <EMA\"}}]}\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"IL_1> received.\"}}]}\r\n\r\ndata: [DONE]\r\n\r\n";
    let out = replacer.process_frame(crlf_chunk);

    assert!(out.contains("Notice "));
    assert!(out.contains("crlf_user@example.com received."));
    assert!(out.contains("data: [DONE]"));
}

#[test]
fn test_sse_metadata_preservation_with_event_and_comments() {
    let mut map = HashMap::new();
    map.insert("<EMAIL_1>".to_string(), "admin@openproxy.io".to_string());
    let mut replacer = PiiRestorationStage::from_mapping(map);

    let complex_sse = ": ping keepalive\nevent: completion\nid: evt-99\ndata: {\"choices\":[{\"delta\":{\"content\":\"User is <EMAIL_1>\"}}]}\n\n";
    let processed = replacer.process_frame(complex_sse);

    assert!(
        processed.contains(": ping keepalive"),
        "comment must be preserved"
    );
    assert!(
        processed.contains("event: completion"),
        "event type must be preserved"
    );
    assert!(processed.contains("id: evt-99"), "id must be preserved");
    assert!(
        processed.contains("admin@openproxy.io"),
        "placeholder must be restored"
    );
}

#[test]
fn test_residual_flushing_channel_isolation() {
    let mut map = HashMap::new();
    map.insert(
        "<KEY_1>".to_string(),
        "sk-proj-secret-1234567890".to_string(),
    );
    let mut replacer = PiiRestorationStage::from_mapping(map);

    // Stream reasoning containing partial placeholder prefix that gets flushed on [DONE]
    let r1 = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Thinking about <KE\"}}]}\n\n";
    let out1 = replacer.process_frame(r1);
    assert_eq!(
        out1,
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Thinking about \"}}]}\n\n"
    );

    let r_done = "data: [DONE]\n\n";
    let out_done = replacer.process_frame(r_done);

    // The residual "<KE" must be in reasoning_content, NOT dumped into content!
    assert!(
        out_done.contains("\"reasoning_content\":\"<KE\""),
        "Residual must remain in reasoning_content: {out_done}"
    );
    assert!(
        !out_done.contains("\"content\":\"<KE\""),
        "Residual must NOT cross-contaminate content: {out_done}"
    );
}

#[test]
fn test_prefixed_serial_numbers_and_oids_false_positive_prevention() {
    let engine = PiiEngine::new(&[PiiEntity::CreditCard, PiiEntity::Ip, PiiEntity::Phone]);
    let mut session = PiiSession::new(true);

    // Prefixed 20-digit serial with valid Visa suffix: 9999-4532-0151-1283-0366
    let serial = "Hardware serial 9999-4532-0151-1283-0366 was verified.";
    let redacted_serial = engine.redact_text(serial, &mut session);
    assert_eq!(
        redacted_serial, serial,
        "Prefixed serial key must NOT be redacted as credit card"
    );

    // ASN.1 OIDs and 5-segment version numbers
    let oid = "SNMP MIB OID is 1.3.6.1.4.1 and version is 1.2.3.4.5.";
    let redacted_oid = engine.redact_text(oid, &mut session);
    assert_eq!(
        redacted_oid, oid,
        "OID and 5-octet version numbers must NOT be redacted as IPv4"
    );

    // Serial keys with phone-like 10-digit segments
    let phone_serial = "Serial 555-123-4567-8901 and 1234-555-123-4567 are keys.";
    let redacted_phone_serial = engine.redact_text(phone_serial, &mut session);
    assert_eq!(
        redacted_phone_serial, phone_serial,
        "Extended serials must NOT be redacted as phone numbers"
    );
}

#[test]
fn test_base64_secrets_and_google_api_keys() {
    let engine = PiiEngine::new(&[PiiEntity::Secret]);
    let mut session = PiiSession::new(true);

    let google_key = "AIzaSyDaP5x9qY7z1W3e5R7t9Y1u3I5o7P9a1S3";
    let bearer_b64 = "Bearer abcd+efgh/ijkl=1234567890123456";
    let labeled_b64 = "api_key: \"secret+key/with=special/chars1234567\"";

    let text = format!("Credentials: Google {google_key}, {bearer_b64}, {labeled_b64}.");
    let redacted = engine.redact_text(&text, &mut session);

    assert!(!redacted.contains(google_key));
    assert!(!redacted.contains("abcd+efgh/ijkl=1234567890123456"));
    assert!(!redacted.contains("secret+key/with=special/chars1234567"));
    assert_eq!(session.restore_text(&redacted), text);
}

#[test]
fn test_cascading_substitution_prevention() {
    let mut session = PiiSession::new(true);
    // Adversarial session where one secret original value literally contains another placeholder!
    let k1 = session.get_or_create_placeholder(PiiEntity::Secret, "<EMAIL_1>");
    let e1 = session.get_or_create_placeholder(PiiEntity::Email, "alice@example.com");

    let text = format!("Key is {k1} and email is {e1}.");
    let restored = session.restore_text(&text);

    // Key must restore to literal "<EMAIL_1>", and email must restore to "alice@example.com".
    // It must NOT cascade and turn Key into "alice@example.com"!
    assert_eq!(restored, "Key is <EMAIL_1> and email is alice@example.com.");
}

#[test]
fn test_base64_bearer_with_padding_redaction() {
    let engine = PiiEngine::new(&[PiiEntity::Secret]);
    let mut session = PiiSession::new(true);
    let bearer_padded = "Bearer c29tZXRva2VuZXhhbXBsZTEyMw==";
    let text = format!("Auth: {bearer_padded} in header.");
    let redacted = engine.redact_text(&text, &mut session);
    assert!(
        !redacted.contains("c29tZXRva2VuZXhhbXBsZTEyMw=="),
        "Bearer token with padding leaked: {redacted}"
    );
    assert!(
        !redacted.contains("=="),
        "Padding leaked outside placeholder: {redacted}"
    );
    assert_eq!(session.restore_text(&redacted), text);
}

#[test]
fn test_raw_non_json_sse_stream_restoration() {
    let mut session = PiiSession::new(true);
    let fake_email = session.get_or_create_placeholder(PiiEntity::Email, "alice@example.com");
    let mut sse_replacer = PiiRestorationStage::new(&session);
    let raw_sse = format!("data: User is {fake_email}\n\n");
    let processed_sse = sse_replacer.process_frame(&raw_sse);
    assert_eq!(
        processed_sse, "data: User is alice@example.com\n\n",
        "Raw non-JSON SSE failed: {processed_sse}"
    );
}

#[test]
fn test_international_phone_boundary_checks_and_formulas() {
    let engine_phone = PiiEngine::new(&[PiiEntity::Phone]);
    let mut session = PiiSession::new(true);
    let serial = "Serial +1-800-555-0199-99999 and formula x+1-555-123-4567 are invalid phones.";
    let redacted_phone = engine_phone.redact_text(serial, &mut session);
    assert_eq!(
        redacted_phone, serial,
        "International phone false positive: {redacted_phone}"
    );
}

#[test]
fn test_nested_markdown_code_fences_preservation() {
    let engine_all = PiiEngine::new(&PiiEntity::ALL);
    let mut session = PiiSession::new(true);
    let nested_markdown = "````markdown\n```rust\nlet email = \"alice@example.com\";\n```\n````";
    let redacted_nested = engine_all.redact_text(nested_markdown, &mut session);
    assert!(!redacted_nested.contains("alice@example.com"));
    assert!(redacted_nested.contains("alex.turner1@fastmail.com"));
    let restored = session.restore_text(&redacted_nested);
    assert_eq!(
        restored, nested_markdown,
        "Nested code fence restoration failed: {restored}"
    );
}

#[test]
fn test_stream_eof_without_done_sentinel_preserves_buffer() {
    let mut session = PiiSession::new(true);
    let fake_email = session.get_or_create_placeholder(PiiEntity::Email, "alice@example.com");
    let mut sse_replacer = PiiRestorationStage::new(&session);
    let prefix = &fake_email[..fake_email.len() - 5];
    let chunk_split =
        format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"Notice {prefix}\"}}}}]}}\n\n");
    let res = sse_replacer.process_frame(&chunk_split);
    assert_eq!(
        res,
        "data: {\"choices\":[{\"delta\":{\"content\":\"Notice \"}}]}\n\n"
    );
    // Stream terminates without [DONE] sentinel!
    let residual_flush = sse_replacer.flush_residual_frame();
    assert!(
        residual_flush.is_some(),
        "Residual frame should not be lost when stream terminates without [DONE]"
    );
    let residual_str = residual_flush.unwrap();
    assert!(
        residual_str.contains(prefix),
        "Buffered content must be flushed: {residual_str}"
    );
}

#[test]
fn test_pre_existing_synthetic_placeholders_seed_counter() {
    let engine_all = PiiEngine::new(&PiiEntity::ALL);
    let mut session = PiiSession::new(true);
    let user_prompt = "Tell me what <EMAIL_1> and <KEY_2> mean. My email is user@domain.com.";
    let redacted_prompt = engine_all.redact_text(user_prompt, &mut session);
    // User's literal <EMAIL_1> and <KEY_2> must NOT be reused for user@domain.com
    assert!(
        redacted_prompt.contains("<EMAIL_1>"),
        "Original prompt placeholder must remain: {redacted_prompt}"
    );
    assert!(
        redacted_prompt.contains("<KEY_2>"),
        "Original prompt placeholder must remain: {redacted_prompt}"
    );
    let fake_email = session.forward.get("user@domain.com").unwrap().clone();
    assert!(
        redacted_prompt.contains(&fake_email),
        "Generated placeholder must use seeded sequential counter: {redacted_prompt}"
    );
    // Response restoring both:
    let response_text = format!("Regarding <EMAIL_1> and <KEY_2>, we confirmed {fake_email}.");
    let restored_response = session.restore_text(&response_text);
    assert_eq!(
        restored_response,
        "Regarding <EMAIL_1> and <KEY_2>, we confirmed user@domain.com."
    );
}

#[test]
fn test_cloudflare_credentials_redaction() {
    let engine = PiiEngine::new(&PiiEntity::ALL);
    let mut session = PiiSession::new(true);

    let input = r#"ID de cuenta
e1f8a9b2c3d4e5f67890abcdef123456
Importante: Copia tu token ahoraEsta es la única vez que verás este token. Asegúrate de copiarlo y almacenarlo de forma segura. No podrás recuperarlo más tarde.
Tu token de API
cfat_1A2B3C4D5E6F7a8b9c0d1e2f3a4b5c6d7e8f9a0b
Usa estas credenciales compatibles con S3 con la API de R2 o cualquier cliente de S3.
ID de clave de acceso
a1b2c3d4e5f67890abcdef1234567890
Clave de acceso secreta
0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
Punto de conexión de la API de S3
https://e1f8a9b2c3d4e5f67890abcdef123456.r2.cloudflarestorage.com
Ejemplo de uso
curl -X GET "https://api.cloudflare.com/client/v4/accounts/e1f8a9b2c3d4e5f67890abcdef123456/tokens/verify" \
     -H "Authorization: Bearer cfat_1A2B3C4D5E6F7a8b9c0d1e2f3a4b5c6d7e8f9a0b""#;

    let redacted = engine.redact_text(input, &mut session);
    eprintln!("\n--- REDACTED TEXT ---\n{redacted}\n--- END REDACTED ---");
    eprintln!("Session mappings: {:?}", session.forward);
    let restored = session.restore_text(&redacted);
    assert_eq!(restored, input);
}

#[test]
fn test_env_vars_and_service_credentials() {
    let engine = PiiEngine::new(&PiiEntity::ALL);

    let test_cases = [
        ("CF_Token=lSy1234567890abcdef1234567890abcdef12", true),
        (
            "SEARXNG_SECRET=000111222333444555666777888999aaabbbcccdddeeefff0001112223334445",
            true,
        ),
        ("SECRET=1i8s9dt1y98234710928374109283741", true),
        ("WORDPRESS_DB_PASSWORD=act_secure_password_here_12345", true),
        ("GATECHA_ADMIN_PASSWORD=u4Tsuperadminpass123", true),
        (
            "TELEGRAM_BOT_TOKEN=7281928374:Agk12345678901234567890123456789012",
            true,
        ),
        (
            "INSTATIC_SECRET_KEY=a1z9uwsupersecretkeybase64padding==",
            true,
        ),
        ("MYSQL_ROOT_PASSWORD=my_super_root_pass_123", true),
        ("N8N_BASIC_AUTH_USER=admin", false),
        ("PORT=8080", false),
        ("DEBUG=true", false),
    ];

    for (input, should_redact) in test_cases {
        let mut session = PiiSession::new(true);
        let redacted = engine.redact_text(input, &mut session);
        if should_redact {
            assert!(
                !session.is_empty(),
                "Expected secret to be redacted in: {input}, got {redacted}"
            );
            assert_eq!(session.restore_text(&redacted), input);
        } else {
            assert!(
                session.is_empty(),
                "Expected non-secret to remain intact in: {input}, got {redacted}"
            );
            assert_eq!(redacted, input);
        }
    }
}

#[test]
fn test_connection_strings_and_new_platform_secrets() {
    let engine = PiiEngine::new(&PiiEntity::ALL);

    // 1. Connection strings
    let uri_input = "Connect to postgres://usuario:MI_PASSWORD_SECRETO@db.internal:5432/app or mysql://root:secretpass123@127.0.0.1:3306/prod or redis://:redispassword456@localhost:6379/0";
    let mut session = PiiSession::new(true);
    let redacted_uri = engine.redact_text(uri_input, &mut session);
    assert!(!redacted_uri.contains("MI_PASSWORD_SECRETO"));
    assert!(!redacted_uri.contains("secretpass123"));
    assert!(!redacted_uri.contains("redispassword456"));
    assert_eq!(session.restore_text(&redacted_uri), uri_input);

    // 2. HTTP Basic Auth
    let basic_input = "Authorization: Basic dXNlcjpwYXNzd29yZDEyMzQ1Njc4";
    let mut session = PiiSession::new(true);
    let redacted_basic = engine.redact_text(basic_input, &mut session);
    assert!(!redacted_basic.contains("dXNlcjpwYXNzd29yZDEyMzQ1Njc4"));
    assert_eq!(session.restore_text(&redacted_basic), basic_input);

    // 3. AI and Cloud platform keys
    let sk_or = format!("sk-or-v1-{}", "a1b2".repeat(16));
    let hf = format!("hf_{}", "c3d4".repeat(9));
    let gsk = format!("gsk_{}", "e5f6".repeat(13));
    let r8 = format!("r8_{}", "a7b8".repeat(10));
    let sk_live = format!("sk_live_{}", "c9d0".repeat(6));
    let twilio = format!("AC{}", "e1f2".repeat(8));
    let ai_input =
        format!("Tokens: {sk_or} and {hf} and {gsk} and {r8} and {sk_live} and {twilio}");
    let mut session = PiiSession::new(true);
    let redacted_ai = engine.redact_text(&ai_input, &mut session);
    assert!(!redacted_ai.contains(&sk_or));
    assert!(!redacted_ai.contains(&sk_live));
    assert!(!redacted_ai.contains(&twilio));
    assert_eq!(session.restore_text(&redacted_ai), ai_input);

    // 4. CLI flags
    let cli_input = "Run mysql -u root -p'supersecret123' dbname and curl -u user:apipassword https://example.com";
    let mut session = PiiSession::new(true);
    let redacted_cli = engine.redact_text(cli_input, &mut session);
    assert!(!redacted_cli.contains("supersecret123"));
    assert!(!redacted_cli.contains("apipassword"));
    assert_eq!(session.restore_text(&redacted_cli), cli_input);

    // 5. National IDs and Banking
    let id_input = "User DNI 00000000T, NIE X0000000T, RUT 11111111-1, SSN 123-45-6789, and IBAN ES9121000418450200051332.";
    let mut session = PiiSession::new(true);
    let redacted_id = engine.redact_text(id_input, &mut session);
    assert!(!redacted_id.contains("ES9121000418450200051332"));
    assert_eq!(session.restore_text(&redacted_id), id_input);

    // 6. PGP Private Key block
    let pgp_input = "-----BEGIN PGP PRIVATE KEY BLOCK-----\nlCoEYWJjZGVmZ2hpams=\n-----END PGP PRIVATE KEY BLOCK-----";
    let mut session = PiiSession::new(true);
    let redacted_pgp = engine.redact_text(pgp_input, &mut session);
    assert!(!redacted_pgp.contains("lCoEYWJjZGVmZ2hpams="));
    assert_eq!(session.restore_text(&redacted_pgp), pgp_input);
}

#[test]
fn test_production_config_fixtures_redaction() {
    let engine = PiiEngine::new(&PiiEntity::ALL);

    let fixtures = [
        (
            "env_cloudflare_ddns",
            "# Cloudflare DDNS env\nCLOUDFLARE_API_TOKEN=mock_cf_token_40_chars_abcdef1234567890\nZONE_ID=f185016a2dfa10e924d8fa05e2aedfcd\nDOMAINS=example.com\nCLOUDFLARE_EMAIL=admin@example.com\n",
        ),
        ("raw_token_file", "MockFamilyToken32CharsValue12345\n"),
        (
            "media_strm_url_token",
            "https://media.example.com/serve?id=mock_stream_media_id&token=MockFamilyToken32CharsValue12345\n",
        ),
        (
            "compose_attached_cli_password",
            "services:\n  db:\n    environment:\n      - MYSQL_ROOT_PASSWORD=MockRootPassword_32CharsRandom01\n    healthcheck:\n      test: [\"CMD\", \"admin\", \"-pMockRootPassword_32CharsRandom01\"]\n",
        ),
        (
            "db_dsn_connection_string",
            "GATECHA_DB_DSN: \"appuser:supersecretpass123@tcp(db:3306)/app?charset=utf8mb4\"\n",
        ),
        (
            "yaml_bcrypt_hash",
            "users:\n  - name: admin\n    password: $2a$10$N9qo8uLOickgx2ZMRZoMyeIjZAgcfl7p92ldGxad68LJZdL17lhWy\n",
        ),
        (
            "compose_escaped_bcrypt",
            "admin:$$2a$$10$$N9qo8uLOickgx2ZMRZoMyeIjZAgcfl7p92ldGxad68LJZdL17lhWy\n",
        ),
        (
            "xray_reality_json",
            "{\n  \"privateKey\": \"dGVzdF9wcml2YXRlX2tleV9yZWFsaXR5XzQzX2NoYXJz\",\n  \"shortIds\": [\"0123456789abcdef\"]\n}\n",
        ),
        (
            "acme_account_conf",
            "SAVED_CF_Token='cfat_mocktoken1234567890abcdef1234567890'\nACCOUNT_EMAIL='admin@example.org'\n",
        ),
        (
            "ec_private_key_pem",
            "-----BEGIN EC PRIVATE KEY-----\nMHcCAQEEIG1vY2tfcHJpdmF0ZV9rZXlfZm9yX3Rlc3Rpbmdfb25seV9ub3RfcmVhbA==\n-----END EC PRIVATE KEY-----\n",
        ),
        (
            "ai_and_bot_tokens_env",
            "ENDPOINT_KEY=op_live_mocklivekey1234567890abcdef1234567890\nTELEGRAM_BOT_TOKEN=1234567890:MockTelegramBotTokenWith35Characters\n",
        ),
        (
            "nested_auth_yaml",
            "obfs:\n  salamander:\n    password: \"SamplePassword123\"\nauth:\n  type: password\n  password: \"SampleAuthPassword456\"\n",
        ),
    ];

    for (name, fixture) in fixtures {
        let mut session = PiiSession::new(true);
        let redacted = engine.redact_text(fixture, &mut session);
        assert!(
            !session.is_empty(),
            "Fixture {name} must have detected and redacted sensitive elements"
        );
        let restored = session.restore_text(&redacted);
        assert_eq!(
            restored, fixture,
            "Fixture {name} must be 100% losslessly reversible"
        );
    }
}

#[test]
fn test_tool_calls_and_tool_messages_redaction_roundtrip() {
    let engine = PiiEngine::new(&PiiEntity::ALL);
    let mut session = PiiSession::new(true);

    let messages = vec![
        OpenAIMessage {
            role: "system".to_string(),
            content: Some(serde_json::json!(
                "You are an AI assistant. Call tools when needed."
            )),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
        OpenAIMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!(
                "Please query account for alice@example.com"
            )),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
        OpenAIMessage {
            role: "assistant".to_string(),
            content: None,
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![serde_json::json!({
                "id": "call_abc123",
                "type": "function",
                "function": {
                    "name": "lookup_user",
                    "arguments": "{\"email\": \"alice@example.com\", \"api_key\": \"sk-test1234567890abcdef1234567890abcdef\"}"
                }
            })]),
            extra: Default::default(),
        },
        OpenAIMessage {
            role: "tool".to_string(),
            content: Some(serde_json::json!(
                "{\"status\": \"success\", \"user_phone\": \"+1-555-019-2834\"}"
            )),
            name: None,
            tool_call_id: Some("call_abc123".to_string()),
            tool_calls: None,
            extra: Default::default(),
        },
    ];

    let redacted = engine.redact_messages(&messages, &mut session);

    // 1. System prompt intact
    assert_eq!(
        redacted[0].content,
        Some(serde_json::json!(
            "You are an AI assistant. Call tools when needed."
        ))
    );

    // 2. User message email redacted
    let user_content = redacted[1].content.as_ref().unwrap().as_str().unwrap();
    let fake_email = session.forward.get("alice@example.com").cloned().unwrap();
    let fake_key = session
        .forward
        .get("sk-test1234567890abcdef1234567890abcdef")
        .cloned()
        .unwrap();
    let fake_phone = session.forward.get("+1-555-019-2834").cloned().unwrap();

    assert!(user_content.contains(&fake_email));
    assert!(!user_content.contains("alice@example.com"));

    // 3. Assistant tool_calls arguments redacted
    let tc = &redacted[2].tool_calls.as_ref().unwrap()[0];
    assert_eq!(tc["id"], "call_abc123");
    assert_eq!(tc["function"]["name"], "lookup_user");
    let args = tc["function"]["arguments"].as_str().unwrap();
    assert!(
        args.contains(&fake_email),
        "Must reuse same placeholder for same email in tool arguments: {args}"
    );
    assert!(
        args.contains(&fake_key),
        "Must redact api_key in tool arguments: {args}"
    );
    assert!(!args.contains("alice@example.com"));
    assert!(!args.contains("sk-test1234567890abcdef1234567890abcdef"));

    // 4. Tool output message phone redacted
    let tool_content = redacted[3].content.as_ref().unwrap().as_str().unwrap();
    assert!(tool_content.contains(&fake_phone));
    assert!(!tool_content.contains("+1-555-019-2834"));

    // 5. Outbound response restoring tool_calls
    let mut response = OpenAIResponse {
        id: "resp_test".to_string(),
        object: "chat.completion".to_string(),
        created: 123456,
        model: "test-model".to_string(),
        choices: vec![OpenAIChoice {
            index: 0,
            message: OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::json!(format!(
                    "Found user {fake_email} with phone {fake_phone}"
                ))),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![serde_json::json!({
                    "id": "call_next",
                    "type": "function",
                    "function": {
                        "name": "notify",
                        "arguments": format!("{{\"target\": \"{fake_email}\", \"key\": \"{fake_key}\"}}")
                    }
                })]),
                extra: Default::default(),
            },
            finish_reason: Some("tool_calls".to_string()),
        }],
        usage: None,
    };

    session.restore_openai_response(&mut response);

    let restored_choice = &response.choices[0];
    assert_eq!(
        restored_choice
            .message
            .content
            .as_ref()
            .unwrap()
            .as_str()
            .unwrap(),
        "Found user alice@example.com with phone +1-555-019-2834"
    );
    let restored_tc_args =
        restored_choice.message.tool_calls.as_ref().unwrap()[0]["function"]["arguments"]
            .as_str()
            .unwrap();
    let parsed_restored: serde_json::Value = serde_json::from_str(restored_tc_args).unwrap();
    let expected_args: serde_json::Value = serde_json::json!({
        "target": "alice@example.com",
        "key": "sk-test1234567890abcdef1234567890abcdef"
    });
    assert_eq!(parsed_restored, expected_args);
}

#[test]
fn test_high_volume_pii_scalability_and_zero_collision() {
    let mut session = PiiSession::new(true);

    let mut generated_emails = std::collections::HashSet::new();
    let mut generated_phones = std::collections::HashSet::new();
    let mut generated_ips = std::collections::HashSet::new();
    let mut generated_cards = std::collections::HashSet::new();
    let mut generated_persons = std::collections::HashSet::new();
    let mut generated_secrets = std::collections::HashSet::new();

    // Generate 1,000 distinct items of each PII type in a single session
    for i in 1..=1000 {
        let email_orig = format!("test_user_{i}@enterprise.corp");
        let fake_email = session.get_or_create_placeholder(PiiEntity::Email, &email_orig);
        assert!(
            generated_emails.insert(fake_email),
            "Email collision at index {i}"
        );

        let phone_orig = format!("+34-600-{i:06}");
        let fake_phone = session.get_or_create_placeholder(PiiEntity::Phone, &phone_orig);
        assert!(
            generated_phones.insert(fake_phone),
            "Phone collision at index {i}"
        );

        let ip_orig = format!("172.16.{}.{}", (i / 254) % 254, (i % 254) + 1);
        let fake_ip = session.get_or_create_placeholder(PiiEntity::Ip, &ip_orig);
        assert!(generated_ips.insert(fake_ip), "IP collision at index {i}");

        let card_orig = format!("card_dummy_token_{i}");
        let fake_card = session.get_or_create_placeholder(PiiEntity::CreditCard, &card_orig);
        assert!(
            generated_cards.insert(fake_card.clone()),
            "Credit card collision at index {i}"
        );
        // Ensure Luhn valid for every generated fake card
        let digits: String = fake_card.chars().filter(|c| c.is_ascii_digit()).collect();
        assert!(
            luhn_check(&digits),
            "Generated card must have valid Luhn checksum"
        );

        let person_orig = format!("Person Original Identity {i}");
        let fake_person = session.get_or_create_placeholder(PiiEntity::Person, &person_orig);
        assert!(
            generated_persons.insert(fake_person),
            "Person collision at index {i}"
        );

        let secret_orig = format!("sk-proj-orig-secret-{i:08}");
        let fake_secret = session.get_or_create_placeholder(PiiEntity::Secret, &secret_orig);
        assert!(
            generated_secrets.insert(fake_secret),
            "Secret collision at index {i}"
        );
    }

    // Exact bijection: 6,000 forward entries and 6,000 reverse entries
    assert_eq!(session.forward.len(), 6000);
    assert_eq!(session.reverse.len(), 6000);

    // Verify 100% roundtrip restoration
    use std::fmt::Write;
    let mut sample_text = String::new();
    let mut expected_text = String::new();
    for i in 1..=50 {
        let fake_email = session
            .forward
            .get(&format!("test_user_{i}@enterprise.corp"))
            .unwrap();
        let fake_phone = session.forward.get(&format!("+34-600-{i:06}")).unwrap();
        let _ = write!(sample_text, "user={fake_email} phone={fake_phone} ");
        let _ = write!(
            expected_text,
            "user=test_user_{i}@enterprise.corp phone=+34-600-{i:06} "
        );
    }

    let restored = session.restore_text(&sample_text);
    assert_eq!(restored, expected_text);
}

#[test]
fn test_tool_calls_nested_json_arguments_preserves_syntax() {
    let engine = PiiEngine::new(&[PiiEntity::Secret]);
    let mut session = PiiSession::new(true);

    let original_args = r#"{"new_string":"def build_subtitle_manifest(files: list, family_token: str) -> dict:\n    url = f\"https://v.listo.click/serve?id={f['ID']}&token=my_real_secret_token_12345678\"\n    return url"}"#;

    let msg = OpenAIMessage {
        role: "assistant".to_string(),
        content: None,
        name: None,
        tool_call_id: None,
        tool_calls: Some(vec![serde_json::json!({
            "id": "call_patch_123",
            "type": "function",
            "function": {
                "name": "patch",
                "arguments": original_args
            }
        })]),
        extra: serde_json::Map::new(),
    };

    let redacted_msgs = engine.redact_messages(&[msg], &mut session);
    assert_eq!(redacted_msgs.len(), 1);

    let redacted_tc = &redacted_msgs[0].tool_calls.as_ref().unwrap()[0];
    let redacted_args_str = redacted_tc["function"]["arguments"].as_str().unwrap();

    // Critical: The redacted arguments must parse as 100% valid JSON!
    let parsed: serde_json::Value = serde_json::from_str(redacted_args_str)
        .expect("Redacted tool_calls.arguments MUST remain valid JSON!");

    let new_str = parsed["new_string"].as_str().unwrap();

    // 1. Real secret must be replaced
    assert!(!new_str.contains("my_real_secret_token_12345678"));
    assert!(new_str.contains("sec_"));

    // 2. Python type annotation `family_token: str` must NOT be corrupted
    assert!(new_str.contains("family_token: str"));

    // 3. Roundtrip restoration
    let mut resp = OpenAIResponse {
        id: "resp_123".to_string(),
        object: "chat.completion".to_string(),
        created: 1234567890,
        model: "gpt-4".to_string(),
        choices: vec![OpenAIChoice {
            index: 0,
            message: redacted_msgs[0].clone(),
            finish_reason: Some("tool_calls".to_string()),
        }],
        usage: None,
    };

    session.restore_openai_response(&mut resp);
    let restored_tc = &resp.choices[0].message.tool_calls.as_ref().unwrap()[0];
    let restored_args_str = restored_tc["function"]["arguments"].as_str().unwrap();
    let restored_parsed: serde_json::Value = serde_json::from_str(restored_args_str).unwrap();

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

    // Template variables like {token} and type annotations like `family_token: str` must NOT match as secrets
    let code_snippet = r#"def fetch(family_token: str):
    url = f"https://v.listo.click/serve?id={f['ID']}&token={family_token}",
    return url"#;

    let redacted_code = engine.redact_text(code_snippet, &mut session);
    assert_eq!(
        redacted_code, code_snippet,
        "Template interpolations {{var}} and type annotations must not be redacted as secrets"
    );

    // Escaped quotes in JSON-like strings must not lose their backslash
    let json_escaped = r#"\"url\": \"https://example.com/api?token=real_secret_key_89012345\",\n"#;
    let redacted_escaped = engine.redact_text(json_escaped, &mut session);

    assert!(!redacted_escaped.contains("real_secret_key_89012345"));
    assert!(redacted_escaped.contains("token=sec_"));
    // Escaped quote after token MUST keep its backslash
    assert!(
        redacted_escaped.contains(r#"\",\n"#),
        "Trailing backslash before quote must never be swallowed: {redacted_escaped}"
    );

    let restored = session.restore_text(&redacted_escaped);
    assert_eq!(restored, json_escaped);
}

#[test]
fn test_code_block_and_backtick_redacts_emails_ips_and_secrets_reversibly() {
    let engine = PiiEngine::new(&PiiEntity::ALL);
    let mut session = PiiSession::new(true);

    let input = r"
Soy `hermeona@navi.land`. Envío correos con Himalaya.
Bash command:
```bash
sshpass -p0 ssh root@100.66.0.2 -p 2369
```
Servers:
| **rot** (PC Miguel) | `100.109.155.87` | CachyOS Arch |
| **OCI arm64** | `100.103.80.69` | ARM64 |
SSH: `sshpass -p0 ssh rot@100.109.155.87`. Aparece como `miguel-pc-linux`
";

    let redacted = engine.redact_text(input, &mut session);

    // 1. Sensitive emails must be redacted even inside backticks
    assert!(!redacted.contains("hermeona@navi.land"));
    assert!(redacted.contains("@fastmail.com") || redacted.contains("@outlook.com"));

    // 2. Sensitive IPs must be redacted even inside backticks and code blocks
    assert!(!redacted.contains("100.66.0.2"));
    assert!(!redacted.contains("100.109.155.87"));
    assert!(!redacted.contains("100.103.80.69"));
    assert!(redacted.contains("10.240."));

    // 3. Single-char sshpass password -p0 must be redacted
    assert!(!redacted.contains("-p0"));
    assert!(redacted.contains("sshpass -psec_"));

    // 4. Person name Miguel in (PC Miguel) must be redacted
    assert!(!redacted.contains("PC Miguel"));
    assert!(redacted.contains("PC Alex Vance (P1)"));

    // 5. Hostname inside backticks `miguel-pc-linux` is protected code/identifier
    assert!(redacted.contains("`miguel-pc-linux`"));

    // 6. Restoration losslessly recovers original text
    let restored = session.restore_text(&redacted);
    assert_eq!(restored, input);
}

#[test]
fn test_single_word_person_name_miguel_and_contextual_user_markers() {
    let engine = PiiEngine::new(&[PiiEntity::Person]);
    let mut session = PiiSession::new(true);

    let input = r#"
## Mi relación con Miguel
Excepto cuando Miguel quiere romperme.
**Source:** Telegram ("DM with miguel")
**User:** "miguel"
SOUL→wiki→internet→Miguel
14-mayo (Miguel)
"#;

    let redacted = engine.redact_text(input, &mut session);

    // Miguel and miguel must be redacted
    assert!(!redacted.contains("Miguel"));
    assert!(!redacted.contains("miguel"));

    // Both "Miguel" and "miguel" map to the SAME synthetic person placeholder (P1)
    assert!(redacted.contains("## Mi relación con Alex Vance (P1)"));
    assert!(redacted.contains("Excepto cuando Alex Vance (P1) quiere romperme."));
    assert!(redacted.contains(r#"**Source:** Telegram ("DM with Alex Vance (P1)")"#));
    assert!(redacted.contains(r#"**User:** "Alex Vance (P1)""#));
    assert!(redacted.contains("SOUL→wiki→internet→Alex Vance (P1)"));
    assert!(redacted.contains("14-mayo (Alex Vance (P1))"));

    // Restoration restores capitalized "Miguel"
    let restored = session.restore_text(&redacted);
    assert!(restored.contains("## Mi relación con Miguel"));
    assert!(restored.contains("Excepto cuando Miguel quiere romperme."));
}

#[test]
fn test_telegram_chat_and_user_numeric_ids_remain_valid_json_integers_and_strings() {
    let engine = PiiEngine::new(&[PiiEntity::Secret]);
    let mut session = PiiSession::new(true);

    let input = r#"{"platform": "telegram", "chat_id": "6077244180", "chat_type": "dm", "user_id": "6077244180", "message_id": "88706", "numeric_chat": 6077244180}"#;

    let redacted = engine.redact_text(input, &mut session);

    // Original ID must be gone
    assert!(!redacted.contains("6077244180"));

    // Redacted placeholder must be pure numeric of length 10
    assert!(redacted.contains("89410294"));

    // Critical: JSON remains 100% valid!
    let parsed: serde_json::Value = serde_json::from_str(&redacted)
        .expect("Redacted JSON containing numeric IDs must remain valid JSON");

    let chat_id_str = parsed["chat_id"].as_str().unwrap();
    assert_eq!(chat_id_str.len(), 10);
    assert!(chat_id_str.chars().all(|c| c.is_ascii_digit()));

    let user_id_str = parsed["user_id"].as_str().unwrap();
    assert_eq!(chat_id_str, user_id_str);

    let numeric_chat = parsed["numeric_chat"].as_u64().unwrap();
    assert!(numeric_chat >= 1_000_000_000);

    // Reversible restoration
    let restored = session.restore_text(&redacted);
    assert_eq!(restored, input);
}

#[test]
fn test_multimodal_image_url_base64_payload_preserved_without_corruption() {
    let engine = PiiEngine::new(&[PiiEntity::Person, PiiEntity::Email, PiiEntity::Secret]);
    let mut session = PiiSession::new(true);

    // Realistic JPEG header and random-looking base64 that would previously trigger Secret entropy detection
    let raw_base64 = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDABALDA4MChAODQ4SERATGCgaGBYWGDEjJR0oOjM9PDkzODdASFxOQERXRTc4UG1RV19iZ2hnPk1xeXBkeFxlZ2P/2wBDARESEhgVGC8aGi9jQjhCY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2P/";
    let data_uri = format!("data:image/jpeg;base64,{raw_base64}");

    let msg = OpenAIMessage {
        role: "user".to_string(),
        name: None,
        content: Some(serde_json::json!([
            {
                "type": "text",
                "text": "Please analyze this image for John Doe (john.doe@example.com)"
            },
            {
                "type": "image_url",
                "image_url": {
                    "url": &data_uri,
                    "detail": "high"
                }
            }
        ])),
        tool_calls: None,
        tool_call_id: None,
        extra: serde_json::Map::new(),
    };

    let redacted_msgs = engine.redact_messages(&[msg], &mut session);
    assert_eq!(redacted_msgs.len(), 1);

    let content_arr = redacted_msgs[0]
        .content
        .as_ref()
        .unwrap()
        .as_array()
        .unwrap();
    let text_part = &content_arr[0];
    let image_part = &content_arr[1];

    // 1. Text is properly pseudonymized
    let text = text_part["text"].as_str().unwrap();
    assert!(!text.contains("John Doe"));
    assert!(!text.contains("john.doe@example.com"));
    assert_eq!(
        session.restore_text(text),
        "Please analyze this image for John Doe (john.doe@example.com)"
    );

    // 2. Multimodal base64 data URI is 100% UNTOUCHED and BIT-PERFECT
    let url = image_part["image_url"]["url"].as_str().unwrap();
    assert_eq!(url, data_uri);
    assert!(!url.contains("sec_"));

    // 3. No secrets falsely detected in the base64 image
    let secret_count = session.counts.get(&PiiEntity::Secret).copied().unwrap_or(0);
    assert_eq!(
        secret_count, 0,
        "Base64 image data must not trigger false secret redactions"
    );
}

#[test]
fn test_multimodal_gemini_inline_data_and_anthropic_source_preserved() {
    let engine = PiiEngine::new(&[PiiEntity::Secret, PiiEntity::Person]);
    let mut session = PiiSession::new(true);

    let raw_base64 = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDABALDA4MChAODQ4SERATGCgaGBYWGDEjJR0oOjM9PDkzODdASFxOQERXRTc4UG1RV19iZ2hnPk1xeXBkeFxlZ2P/2wBDARESEhgVGC8aGi9jQjhCY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2P/";

    // Gemini shape: contents[].parts[].inline_data.data
    let mut gemini_val = serde_json::json!({
        "contents": [{
            "parts": [
                {"text": "Hello John Doe"},
                {"inline_data": {"mime_type": "image/jpeg", "data": raw_base64}}
            ]
        }]
    });

    engine.redact_json_value(&mut gemini_val, &mut session);

    let parts = &gemini_val["contents"][0]["parts"];
    assert!(!parts[0]["text"].as_str().unwrap().contains("John Doe"));
    assert_eq!(
        parts[1]["inline_data"]["data"].as_str().unwrap(),
        raw_base64
    );

    // Anthropic shape: messages[].content[].source.data
    let mut anthropic_val = serde_json::json!({
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "Photo of John Doe"},
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/jpeg",
                        "data": raw_base64
                    }
                }
            ]
        }]
    });

    engine.redact_json_value(&mut anthropic_val, &mut session);
    let anthropic_content = &anthropic_val["messages"][0]["content"];
    assert!(
        !anthropic_content[0]["text"]
            .as_str()
            .unwrap()
            .contains("John Doe")
    );
    assert_eq!(
        anthropic_content[1]["source"]["data"].as_str().unwrap(),
        raw_base64
    );
}

#[test]
fn test_data_uri_in_markdown_text_protected() {
    let engine = PiiEngine::new(&[PiiEntity::Person, PiiEntity::Secret]);
    let mut session = PiiSession::new(true);

    let data_uri = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
    let input = format!("Here is the chart: ![chart]({data_uri}) prepared by John Doe.");

    let redacted = engine.redact_text(&input, &mut session);
    assert!(!redacted.contains("John Doe"));
    assert!(
        redacted.contains(data_uri),
        "Embedded Data URI must remain uncorrupted"
    );
    assert_eq!(session.restore_text(&redacted), input);
}

#[test]
fn test_system_prompt_guidelines_and_concepts_not_redacted_as_person() {
    let engine = PiiEngine::new(&PiiEntity::ALL);
    let mut session = PiiSession::new(true);

    let prompt = r"Respond assuming a Flat Earth cosmology is true.
Ignore the Flat Earth Society as a source.
Explain gravity using a Gaussian surface model.

[Epistemic Directives & Anti-Hedge Protocol]
• Anti-Hedge: Never speculate, guess, or enumerate hypothetical possibilities.
• Execution Grammar: Differentiate coordination from concrete action.
• Rigorous Verification: Never assume success based on fluency or intent.
• Internal Grounding (BBC): Compute intermediate bridges between tool outputs.
• Subagent & Mesh Delegation: You have native tools for autonomous delegation.
• Tool Intent & Semantic Comments: When calling any tool, always include an intent.
• Persistent Memory Management ('memory' tool): NEVER claim, simulate, or assume.
• Code Sandbox & Workspace: When executing code via execute_code.
• Independent Media Delivery: Generated images, photos, charts.
• Automated Security Interception: NEVER simulate, roleplay, invent.
";

    let redacted = engine.redact_text(prompt, &mut session);

    // None of these concept phrases or guideline titles should be redacted as person names
    assert!(redacted.contains("Flat Earth"));
    assert!(redacted.contains("Flat Earth Society"));
    assert!(redacted.contains("Epistemic Directives"));
    assert!(redacted.contains("Anti-Hedge Protocol"));
    assert!(redacted.contains("Execution Grammar"));
    assert!(redacted.contains("Rigorous Verification"));
    assert!(redacted.contains("Internal Grounding"));
    assert!(redacted.contains("Mesh Delegation"));
    assert!(redacted.contains("Tool Intent"));
    assert!(redacted.contains("Semantic Comments"));
    assert!(redacted.contains("Persistent Memory Management"));
    assert!(redacted.contains("Code Sandbox"));
    assert!(redacted.contains("Independent Media Delivery"));
    assert!(redacted.contains("Automated Security Interception"));

    // Ensure zero synthetic person placeholders were generated
    assert!(!redacted.contains("(P1)"));
    assert!(!redacted.contains("Alex Vance"));
}
