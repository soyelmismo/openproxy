use super::super::*;
use openproxy_types::config::{PiiConfig, PiiEntity};
use openproxy_types::message::{OpenAIChoice, OpenAIMessage, OpenAIResponse};

fn make_resp(content: &str) -> OpenAIResponse {
    OpenAIResponse {
        id: "resp-1".into(),
        object: "chat.completion".into(),
        created: 1_700_000_000,
        model: "test-model".into(),
        choices: vec![OpenAIChoice {
            index: 0,
            message: OpenAIMessage {
                role: "assistant".into(),
                content: Some(serde_json::Value::String(content.into())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            },
            finish_reason: Some("stop".into()),
        }],
        usage: None,
    }
}

fn resp_content(resp: &OpenAIResponse) -> &str {
    resp.choices[0]
        .message
        .content
        .as_ref()
        .unwrap()
        .as_str()
        .unwrap()
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

    let mut resp = make_resp(
        "Verified card 4532 0151 1283 1018. Confirmation sent to alex.turner1@fastmail.com.",
    );
    session.restore_openai_response(&mut resp);
    assert_eq!(
        resp_content(&resp),
        "Verified card 4532-0151-1283-0366. Confirmation sent to test@example.com."
    );
}

#[test]
fn test_latency_overhead_on_50kb_payload() {
    let engine = PiiEngine::new(&PiiEntity::ALL);
    let paragraph = "In software engineering, testing email addresses like alice@example.com and phone +1-800-555-0199\n\
requires care. You can visit https://service.com/dashboard and consult Dr. Gregory House for assistance.\n\
Also make sure server at 192.168.1.50 is operational and API key sk-proj-1234567890abcdefghijklmn is safe.\n\
```rust\nfn main() {\n    let email = \"code_email@not_redacted.org\";\n    println!(\"Hello {}\", email);\n}\n```\n";
    let big_payload = paragraph.repeat(50 * 1024 / paragraph.len() + 1);
    assert!(big_payload.len() >= 50 * 1024);

    let mut s = PiiSession::new(true);
    let t0 = std::time::Instant::now();
    let red = engine.redact_text(&big_payload, &mut s);
    let redact_duration = t0.elapsed();
    let res = s.restore_text(&red);
    assert_eq!(res, big_payload);
    assert!(
        redact_duration.as_millis() < 600,
        "Redaction too slow: {redact_duration:?}"
    );
}

#[test]
fn test_pipeline_roundtrip_unary_and_streaming() {
    let cfg = PiiConfig {
        pii_enabled: true,
        pii_reversible: true,
        pii_redact_logs: true,
        pii_entities: PiiEntity::ALL.to_vec(),
    };
    let engine = PiiEngine::from_config(&cfg);
    let mut session = PiiSession::new(cfg.pii_reversible);

    let original_msg =
        "Please verify my email alice@example.com with key sk-proj-1234567890abcdefghijklmn.";
    let msgs = vec![OpenAIMessage {
        role: "user".into(),
        content: Some(serde_json::Value::String(original_msg.into())),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: Default::default(),
    }];

    let redacted = engine.redact_messages(&msgs, &mut session);
    let red_str = match &redacted[0].content {
        Some(serde_json::Value::String(s)) => s.as_str(),
        _ => panic!(),
    };
    assert!(
        !red_str.contains("alice@example.com")
            && !red_str.contains("sk-proj-1234567890abcdefghijklmn")
    );
    let fake_email = session.forward.get("alice@example.com").cloned().unwrap();
    let fake_key = session
        .forward
        .get("sk-proj-1234567890abcdefghijklmn")
        .cloned()
        .unwrap();
    assert!(red_str.contains(&fake_email) && red_str.contains(&fake_key));

    let mut upstream_unary = make_resp(&format!(
        "Confirmed receipt for {fake_email} using key {fake_key}."
    ));
    session.restore_openai_response(&mut upstream_unary);
    assert_eq!(
        resp_content(&upstream_unary),
        "Confirmed receipt for alice@example.com using key sk-proj-1234567890abcdefghijklmn."
    );

    let mut sse_replacer = PiiRestorationStage::new(&session);
    let (e1, e2) = fake_email.split_at(5);
    let (k1, k2) = fake_key.split_at(8);
    let c1 = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"Confirmed receipt for {e1}\"}}}}]}}\n\n"
    );
    let c2 = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{e2} using key {k1}\"}}}}]}}\n\n"
    );
    let c3 =
        format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{k2} right now.\"}}}}]}}\n\n");
    assert_eq!(
        sse_replacer.process_frame(&c1),
        "data: {\"choices\":[{\"delta\":{\"content\":\"Confirmed receipt for \"}}]}\n\n"
    );
    assert_eq!(
        sse_replacer.process_frame(&c2),
        "data: {\"choices\":[{\"delta\":{\"content\":\"alice@example.com using key \"}}]}\n\n"
    );
    assert_eq!(
        sse_replacer.process_frame(&c3),
        "data: {\"choices\":[{\"delta\":{\"content\":\"sk-proj-1234567890abcdefghijklmn right now.\"}}]}\n\n"
    );
    assert_eq!(
        sse_replacer.process_frame("data: [DONE]\n\n"),
        "data: [DONE]\n\n"
    );
}

#[test]
fn test_pii_non_reversible_behavior() {
    let cfg = PiiConfig {
        pii_enabled: true,
        pii_reversible: false,
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

    let mut resp = make_resp("Noted alex.turner1@fastmail.com");
    session.restore_openai_response(&mut resp);
    assert_eq!(resp_content(&resp), "Noted alex.turner1@fastmail.com");
}

#[test]
fn test_pre_existing_synthetic_placeholders_seed_counter() {
    let engine = PiiEngine::new(&PiiEntity::ALL);
    let mut session = PiiSession::new(true);
    let user_prompt = "Tell me what <EMAIL_1> and <KEY_2> mean. My email is user@domain.com.";
    let red = engine.redact_text(user_prompt, &mut session);
    assert!(red.contains("<EMAIL_1>") && red.contains("<KEY_2>"));
    let fake_email = session.forward.get("user@domain.com").unwrap().clone();
    assert!(red.contains(&fake_email));
    let resp = format!("Regarding <EMAIL_1> and <KEY_2>, we confirmed {fake_email}.");
    assert_eq!(
        session.restore_text(&resp),
        "Regarding <EMAIL_1> and <KEY_2>, we confirmed user@domain.com."
    );
}

#[test]
fn test_high_volume_pii_scalability_and_zero_collision() {
    let mut session = PiiSession::new(true);
    let (mut em, mut ph, mut ip, mut cd, mut ps, mut sc) = (
        std::collections::HashSet::new(),
        std::collections::HashSet::new(),
        std::collections::HashSet::new(),
        std::collections::HashSet::new(),
        std::collections::HashSet::new(),
        std::collections::HashSet::new(),
    );

    for i in 1..=1000 {
        assert!(em.insert(session.get_or_create_placeholder(
            PiiEntity::Email,
            &format!("test_user_{i}@enterprise.corp")
        )));
        assert!(ph.insert(
            session.get_or_create_placeholder(PiiEntity::Phone, &format!("+34-600-{i:06}"))
        ));
        assert!(ip.insert(session.get_or_create_placeholder(
            PiiEntity::Ip,
            &format!("172.16.{}.{}", (i / 254) % 254, (i % 254) + 1)
        )));
        let fake_card = session
            .get_or_create_placeholder(PiiEntity::CreditCard, &format!("card_dummy_token_{i}"));
        assert!(cd.insert(fake_card.clone()));
        let digits: String = fake_card.chars().filter(|c| c.is_ascii_digit()).collect();
        assert!(luhn_check(&digits));
        assert!(ps.insert(
            session.get_or_create_placeholder(PiiEntity::Person, &format!("Person Identity {i}"))
        ));
        assert!(sc.insert(
            session.get_or_create_placeholder(
                PiiEntity::Secret,
                &format!("sk-proj-orig-secret-{i:08}")
            )
        ));
    }
    assert_eq!(session.forward.len(), 6000);
    // reverse map contains the 6000 full placeholders plus first/last component aliases for Person entities
    assert!(session.reverse.len() >= 6000 && session.reverse.len() <= 6080);

    use std::fmt::Write;
    let (mut sample, mut expected) = (String::new(), String::new());
    for i in 1..=50 {
        let fe = session
            .forward
            .get(&format!("test_user_{i}@enterprise.corp"))
            .unwrap();
        let fp = session.forward.get(&format!("+34-600-{i:06}")).unwrap();
        let _ = write!(sample, "user={fe} phone={fp} ");
        let _ = write!(
            expected,
            "user=test_user_{i}@enterprise.corp phone=+34-600-{i:06} "
        );
    }
    assert_eq!(session.restore_text(&sample), expected);
}
