use super::super::*;
use openproxy_types::config::PiiEntity;
use openproxy_types::message::OpenAIMessage;

fn check_roundtrip(entities: &[PiiEntity], input: &str) -> (String, PiiSession) {
    let engine = PiiEngine::new(entities);
    let mut session = PiiSession::new(true);
    let redacted = engine.redact_text(input, &mut session);
    assert_ne!(redacted, input, "Input should have been redacted: {input}");
    let restored = session.restore_text(&redacted);
    assert_eq!(restored, input, "Roundtrip restoration failed for: {input}");
    (redacted, session)
}

fn check_no_redact(entities: &[PiiEntity], input: &str) {
    let engine = PiiEngine::new(entities);
    let mut session = PiiSession::new(true);
    let redacted = engine.redact_text(input, &mut session);
    assert_eq!(
        redacted, input,
        "Input should NOT have been redacted: {input}"
    );
}

#[test]
fn test_unicode_email_omissions() {
    // 1. Fullwidth dot U+FF0E in domain separating subdomains
    let (red1, _) = check_roundtrip(
        &[PiiEntity::Email],
        "Contact user@mail\u{FF0E}example\u{FF0E}com for details.",
    );
    assert!(!red1.contains("user@"));
    assert!(!red1.contains("example"));

    // 2. Ideographic full stop U+3002 in domain (CJK input)
    let (red2, _) = check_roundtrip(
        &[PiiEntity::Email],
        "Email alice@example\u{3002}org for assistance.",
    );
    assert!(!red2.contains("alice@"));

    // 3. Small commercial at U+FE6B
    let (red3, _) = check_roundtrip(
        &[PiiEntity::Email],
        "Write to support\u{FE6B}company.com today.",
    );
    assert!(!red3.contains("support"));

    // 4. Fullwidth dot U+FF0E in local-part
    let (red4, _) = check_roundtrip(
        &[PiiEntity::Email],
        "Admin is first\u{FF0E}last@domain.com please write.",
    );
    assert!(!red4.contains("first"));

    // 5. Zero-width space U+200B inside local part
    let (red5, _) = check_roundtrip(
        &[PiiEntity::Email],
        "Obfuscated al\u{200B}ice@example.com was sent.",
    );
    assert!(!red5.contains("example.com"));

    // 6. Punycode TLD xn--p1ai
    let (red6, _) = check_roundtrip(
        &[PiiEntity::Email],
        "Russian domain contact@domain.xn--p1ai is valid.",
    );
    assert!(!red6.contains("contact@"));
}

#[test]
fn test_ipv4_after_colon_is_not_confused_with_ipv6() {
    for input in ["IP:192.0.2.42", "Server:203.0.113.7"] {
        let (redacted, _) = check_roundtrip(&[PiiEntity::Ip], input);
        assert!(!redacted.contains("192.0.2.42"));
        assert!(!redacted.contains("203.0.113.7"));
    }
    let (redacted, session) = check_roundtrip(&[PiiEntity::Ip], "::ffff:192.0.2.42");
    assert_eq!(session.forward.len(), 1);
    assert!(redacted.starts_with("fd00:"));
}

#[test]
fn test_bare_ipv6_unspecified_requires_literal_or_network_context() {
    for input in ["ns::Type", "namespace :: method", "x :: y", "x ::  y"] {
        check_no_redact(&[PiiEntity::Ip], input);
    }
    for input in [
        "::",
        "Configured host :: for wildcard listening.",
        "Bind address: ::",
        "a::b",
        "3::4",
    ] {
        check_roundtrip(&[PiiEntity::Ip], input);
    }
}

#[test]
fn test_ipv6_omissions_and_boundaries() {
    // 1. Unspecified address ::
    let (red1, _) = check_roundtrip(
        &[PiiEntity::Ip],
        "Configured host :: for wildcard listening.",
    );
    assert!(!red1.contains("host :: "));

    // 2. Subnet ending in ::
    let (red2, _) = check_roundtrip(&[PiiEntity::Ip], "Assigned prefix 2001:db8:: to network.");
    assert!(!red2.contains("2001:db8::"));

    // 3. Link-local ending in ::
    let (red3, _) = check_roundtrip(&[PiiEntity::Ip], "Link local address fe80:: is configured.");
    assert!(!red3.contains("fe80::"));

    // 4. IPv4-mapped IPv6 address ::ffff:192.0.2.1
    let (red4, _) = check_roundtrip(
        &[PiiEntity::Ip],
        "IPv4-mapped address ::ffff:192.0.2.1 detected.",
    );
    assert!(!red4.contains("192.0.2.1"));
    assert!(!red4.contains("::ffff:"));

    // 5. Boundary checks: invalid addresses with adjacent colons should not match
    check_no_redact(&[PiiEntity::Ip], "Invalid address :::1 should not match.");
    check_no_redact(
        &[PiiEntity::Ip],
        "Nine segments 1:2:3:4:5:6:7:8:9 should not match.",
    );
}

#[test]
fn test_phone_fullwidth_digits() {
    // Fullwidth decimal digits in international phone
    let input = "Call +\u{FF11}\u{FF12}\u{FF13}-\u{FF15}\u{FF15}\u{FF15}-\u{FF10}\u{FF11}\u{FF19}\u{FF19} immediately.";
    let (red, _) = check_roundtrip(&[PiiEntity::Phone], input);
    assert!(!red.contains("+\u{FF11}\u{FF12}\u{FF13}"));
}

#[test]
fn test_contextual_secrets_omissions() {
    // 1. Password labels: db_pass, db_pwd, passphrase
    let (red1, _) = check_roundtrip(&[PiiEntity::Secret], "db_pass = \"SuperSecretPass123!\"");
    assert!(!red1.contains("SuperSecretPass123!"));

    let (red2, _) = check_roundtrip(&[PiiEntity::Secret], "db_pwd = \"MySecretDbPassword!\"");
    assert!(!red2.contains("MySecretDbPassword!"));

    let (red3, _) = check_roundtrip(
        &[PiiEntity::Secret],
        "passphrase = \"correct-horse-battery-staple\"",
    );
    assert!(!red3.contains("correct-horse-battery-staple"));

    // 2. Key labels: master_key, signing_key, client_secret
    let (red4, _) = check_roundtrip(&[PiiEntity::Secret], "master_key = \"MasterCryptSecret99\"");
    assert!(!red4.contains("MasterCryptSecret99"));

    let (red5, _) = check_roundtrip(
        &[PiiEntity::Secret],
        "client_secret = \"ClientSecretString456\"",
    );
    assert!(!red5.contains("ClientSecretString456"));

    // 3. Hash label: api_hash
    let (red6, _) = check_roundtrip(
        &[PiiEntity::Secret],
        "api_hash: \"0123456789abcdef0123456789abcdef\"",
    );
    assert!(!red6.contains("0123456789abcdef0123456789abcdef"));

    // 4. English multiline console labels
    let cloud_input = "Access Key ID\nAKIAIOSFODNN7EXAMPLE\nSecret Access Key\nwJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n";
    let (red7, _) = check_roundtrip(&[PiiEntity::Secret], cloud_input);
    assert!(!red7.contains("AKIAIOSFODNN7EXAMPLE"));
    assert!(!red7.contains("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"));

    // 5. Common token formats: GitLab PAT, Perplexity, NPM
    // Generate an unmistakably synthetic fixture instead of committing a
    // credential-shaped literal that triggers GitHub push protection.
    let gitlab_token = format!("glpat-{}", "x".repeat(20));
    let (red8, _) = check_roundtrip(
        &[PiiEntity::Secret],
        &format!("GitLab token {gitlab_token} in CI pipeline."),
    );
    assert!(!red8.contains(&gitlab_token));

    let (red9, _) = check_roundtrip(
        &[PiiEntity::Secret],
        "Perplexity key pplx-1234567890abcdef1234567890abcdef12345678 configured.",
    );
    assert!(!red9.contains("pplx-1234567890abcdef1234567890abcdef12345678"));

    let (red10, _) = check_roundtrip(
        &[PiiEntity::Secret],
        "NPM token npm_1234567890abcdefghijklmnopqrstuv in .npmrc.",
    );
    assert!(!red10.contains("npm_1234567890abcdefghijklmnopqrstuv"));
}

#[test]
fn test_person_contextual_and_accents() {
    // 1. Contextual markers Name: and Nombre:
    let (red1, _) = check_roundtrip(&[PiiEntity::Person], "Name: Carlos went home.");
    assert!(!red1.contains("Carlos"));

    let (red2, _) = check_roundtrip(&[PiiEntity::Person], "Nombre: Miguel está aquí.");
    assert!(!red2.contains("Miguel"));

    // 2. Accented Spanish first names
    let (red3, _) = check_roundtrip(
        &[PiiEntity::Person],
        "Ayer hablé con José sobre el despliegue.",
    );
    assert!(!red3.contains("José"));

    let (red4, _) = check_roundtrip(
        &[PiiEntity::Person],
        "María aprobó la solicitud de cambios.",
    );
    assert!(!red4.contains("María"));

    let (red5, _) = check_roundtrip(
        &[PiiEntity::Person],
        "Contacta a Álvaro para soporte técnico.",
    );
    assert!(!red5.contains("\u{00C1}lvaro"));
}

#[test]
fn test_tool_arguments_payload_name_and_id_are_redacted() {
    let engine = PiiEngine::new(&[PiiEntity::Person, PiiEntity::Email, PiiEntity::Secret]);
    let mut session = PiiSession::new(true);

    let messages = vec![OpenAIMessage {
        role: "assistant".into(),
        content: None,
        name: None,
        tool_call_id: None,
        tool_calls: Some(vec![serde_json::json!({
            "id": "call_protocol_id_12345",
            "type": "function",
            "function": {
                "name": "create_user_account",
                "arguments": "{\"name\": \"Carlos Sanchez\", \"id\": \"carlos.sanchez@example.com\", \"secret\": \"SuperSecretToken12345\"}"
            }
        })]),
        extra: Default::default(),
    }];

    let redacted = engine.redact_messages(&messages, &mut session);

    let tc = &redacted[0].tool_calls.as_ref().unwrap()[0];
    // 1. Tool protocol identifiers must remain intact
    assert_eq!(tc["id"], "call_protocol_id_12345");
    assert_eq!(tc["function"]["name"], "create_user_account");

    // 2. Payload fields 'name' and 'id' INSIDE arguments must be redacted
    let args_str = tc["function"]["arguments"].as_str().unwrap();
    assert!(
        !args_str.contains("Carlos Sanchez"),
        "Payload 'name' leaked: {args_str}"
    );
    assert!(
        !args_str.contains("carlos.sanchez@example.com"),
        "Payload 'id' leaked: {args_str}"
    );
    assert!(
        !args_str.contains("SuperSecretToken12345"),
        "Payload 'secret' leaked: {args_str}"
    );

    // 3. Roundtrip restoration restores the exact original arguments payload
    let mut resp = openproxy_types::message::OpenAIResponse {
        id: "resp_test".into(),
        object: "chat.completion".into(),
        created: 0,
        model: "gpt-4".into(),
        usage: None,
        choices: vec![openproxy_types::message::OpenAIChoice {
            index: 0,
            finish_reason: Some("tool_calls".into()),
            message: redacted[0].clone(),
        }],
    };
    session.restore_openai_response(&mut resp);
    let restored_args_str =
        resp.choices[0].message.tool_calls.as_ref().unwrap()[0]["function"]["arguments"]
            .as_str()
            .unwrap();
    let restored_args: serde_json::Value = serde_json::from_str(restored_args_str).unwrap();
    assert_eq!(restored_args["name"], "Carlos Sanchez");
    assert_eq!(restored_args["id"], "carlos.sanchez@example.com");
    assert_eq!(restored_args["secret"], "SuperSecretToken12345");
}

#[test]
fn test_preseed_all_messages_prevents_placeholder_collision() {
    let engine = PiiEngine::new(&[PiiEntity::Email]);
    let mut session = PiiSession::new(true);

    let messages = vec![
        OpenAIMessage {
            role: "user".into(),
            content: Some(serde_json::json!("Contact alice@example.com for info.")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
        OpenAIMessage {
            role: "assistant".into(),
            content: Some(serde_json::json!("Already notified <EMAIL_2> earlier.")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
    ];

    let redacted = engine.redact_messages(&messages, &mut session);
    let msg1_text = redacted[0].content.as_ref().unwrap().as_str().unwrap();
    assert!(
        !msg1_text.contains("<EMAIL_2>"),
        "Collision with existing placeholder: {msg1_text}"
    );
}
