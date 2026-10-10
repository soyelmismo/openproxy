use super::super::*;
use openproxy_types::config::{PiiConfig, PiiEntity};
use openproxy_types::message::{OpenAIChoice, OpenAIMessage, OpenAIResponse};

/// Exercise the real formatting -> HTTP upstream -> dispatcher -> client path.
/// The upstream is a local echo fixture, so no real PII leaves this machine.
#[tokio::test]
async fn pii_http_roundtrip_unary_and_streaming() {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    for streaming in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let (header_end, length) = loop {
                let mut chunk = [0u8; 4096];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0, "request ended before headers");
                bytes.extend_from_slice(&chunk[..n]);
                assert!(bytes.len() < 1024 * 1024);
                if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..pos]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (key, val) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| val.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    break (pos + 4, length);
                }
            };
            while bytes.len() < header_end + length {
                let mut chunk = [0u8; 4096];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0, "request ended before body");
                bytes.extend_from_slice(&chunk[..n]);
            }
            let request: serde_json::Value =
                serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
            let text = request["messages"][0]["content"].as_str().unwrap();
            assert!(!text.contains("alice@example.com"));
            assert!(!text.contains("+34 600 123 456"));
            let output = text.replace("+1-202-555-0111", "+12025550111");
            let (content_type, body) = if streaming {
                let split = output.find("alex.turner").unwrap() + 5;
                let frames = [&output[..split], &output[split..]].map(|part| {
                    format!("data: {}\n\n", serde_json::json!({"id": "pii-echo", "object": "chat.completion.chunk", "created": 1, "model": "m",
                        "choices": [{"index": 0, "delta": {"content": part}}]}))
                });
                (
                    "text/event-stream",
                    format!(
                        "{}{}data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n",
                        frames[0], frames[1]
                    ),
                )
            } else {
                ("application/json", serde_json::json!({"id": "pii-echo", "object": "chat.completion", "created": 1, "model": "m",
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": output}, "finish_reason": "stop"}]}).to_string())
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let (pool, conn, _) = crate::test_utils::fresh_pool();
        let key = Arc::new(openproxy_db::MasterKey::generate().unwrap());
        let combo_id = {
            let conn = conn.lock();
            let combo_id =
                crate::test_utils::seed_solo_combo_at_url(&conn, "mock-openai", &url, &key).0;
            conn.execute(
                "UPDATE models SET capabilities_json = ?1 WHERE provider_id = 'mock-openai'",
                [serde_json::json!({"streaming": streaming}).to_string()],
            )
            .unwrap();
            combo_id
        };
        let mut config = crate::test_utils::test_config_with_mock(key, &url);
        config.retries.max_attempts = 1;
        config.pii_config = PiiConfig {
            pii_enabled: true,
            pii_reversible: true,
            pii_redact_logs: true,
            pii_entities: vec![PiiEntity::Email, PiiEntity::Phone],
        };
        let pipeline = crate::test_utils::test_pipeline_with_pool(pool, config);
        let (mut req, _disconnect) = crate::test_utils::make_request(combo_id);
        let original = "email alice@example.com phone +34 600 123 456.";
        let openai = Arc::make_mut(&mut req.openai_request);
        openai.stream = streaming;
        openai.messages[0].content = Some(serde_json::json!(original));
        let (sink, mut rx) = tokio::sync::mpsc::channel(32);
        if streaming {
            req.stream_sink = Some(crate::StreamSink::Direct(sink));
        }
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), pipeline.run(req))
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(
            result.status_code, 200,
            "streaming={streaming}: {:?}",
            result.error
        );
        if streaming {
            let mut content = String::new();
            let mut done = false;
            while let Ok(chunk) = rx.try_recv() {
                let chunk = std::str::from_utf8(&chunk).unwrap();
                for line in chunk.lines().filter_map(|line| line.strip_prefix("data: ")) {
                    if line == "[DONE]" {
                        done = true;
                    } else {
                        let value: serde_json::Value = serde_json::from_str(line).unwrap();
                        if let Some(part) = value["choices"][0]["delta"]["content"].as_str() {
                            assert!(!done, "content emitted after terminal frame");
                            content.push_str(part);
                        }
                    }
                }
            }
            assert!(done);
            assert_eq!(content, original);
        } else {
            assert_eq!(
                resp_content(result.final_response.as_ref().unwrap()),
                original
            );
        }
    }
}

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
    // 6000 full placeholders plus the Person first/last component aliases.
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
