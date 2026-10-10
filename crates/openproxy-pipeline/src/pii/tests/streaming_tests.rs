use super::super::*;
use openproxy_types::config::PiiEntity;
use std::collections::HashMap;

fn mapping(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn test_streaming_attached_cli_secret_restores_at_every_split() {
    let engine = PiiEngine::new(&[PiiEntity::Secret]);
    let mut session = PiiSession::new(true);
    let original = "sshpass -p7 ssh user@example.invalid";
    let redacted = engine.redact_text(original, &mut session);
    assert_ne!(redacted, original);
    for split in 0..=redacted.len() {
        let mut replacer = StreamingWindowReplacer::from_session(&session);
        let mut restored = replacer.process(&redacted[..split]);
        restored.push_str(&replacer.process(&redacted[split..]));
        restored.push_str(&replacer.flush());
        assert_eq!(restored, original, "split={split}");
    }
    assert_eq!(session.restore_text(&redacted), original);
    let placeholder = session.forward.get("7").unwrap();
    let embedded = format!("prefix-p{placeholder}_suffix");
    assert_eq!(session.restore_text(&embedded), embedded);
}

#[test]
fn test_streaming_full_person_alias_has_priority_at_every_split() {
    let mut session = PiiSession::new(true);
    let placeholder = session.get_or_create_placeholder(PiiEntity::Person, "Miguel");
    let input = format!("Hello {placeholder}!");
    for split in 0..=input.len() {
        let mut replacer = StreamingWindowReplacer::from_session(&session);
        let mut restored = replacer.process(&input[..split]);
        restored.push_str(&replacer.process(&input[split..]));
        restored.push_str(&replacer.flush());
        assert_eq!(restored, "Hello Miguel!", "split={split}");
    }
}

#[test]
fn test_streaming_window_replacer_split_across_chunks() {
    let mut replacer = StreamingWindowReplacer::new(mapping(&[
        ("<EMAIL_1>", "alice@example.com"),
        ("<PHONE_1>", "+1-800-555-0199"),
    ]));

    assert_eq!(
        replacer.process("Send confirmation to <EMA"),
        "Send confirmation to "
    );
    assert_eq!(
        replacer.process("IL_1> right now."),
        "alice@example.com right now."
    );
    assert_eq!(replacer.flush(), "");

    assert_eq!(replacer.process("Call me at <"), "Call me at ");
    assert_eq!(replacer.process("PHONE_"), "");
    assert_eq!(replacer.process("1> today."), "+1-800-555-0199 today.");
    assert_eq!(replacer.flush(), "");

    assert_eq!(replacer.process("こんにちは 🚀 <E"), "こんにちは 🚀 ");
    assert_eq!(replacer.process("MA"), "");
    assert_eq!(replacer.process("IL_"), "");
    assert_eq!(replacer.process("1> 世界"), "alice@example.com 世界");
    assert_eq!(replacer.flush(), "");
}

#[test]
fn test_streaming_window_replacer_non_placeholders() {
    let mut replacer = StreamingWindowReplacer::new(mapping(&[("<EMAIL_1>", "alice@example.com")]));
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
    assert_eq!(
        replacer.process("if a < b then contact <EMAIL_1>."),
        "if a < b then contact alice@example.com."
    );
    assert_eq!(
        replacer.process("Email is <<EMAIL_1>>."),
        "Email is <alice@example.com>."
    );
}

#[test]
fn test_streaming_sse_framing_preservation() {
    let mut sse = PiiRestorationStage::from_mapping(mapping(&[("<EMAIL_1>", "alice@example.com")]));
    assert_eq!(
        sse.process_frame("data: {\"choices\":[{\"delta\":{\"content\":\"Contact <EMA\"}}]}\n\n"),
        "data: {\"choices\":[{\"delta\":{\"content\":\"Contact \"}}]}\n\n"
    );
    assert_eq!(
        sse.process_frame("data: {\"choices\":[{\"delta\":{\"content\":\"IL_1> please.\"}}]}\n\n"),
        "data: {\"choices\":[{\"delta\":{\"content\":\"alice@example.com please.\"}}]}\n\n"
    );
    assert_eq!(sse.process_frame("data: [DONE]\n\n"), "data: [DONE]\n\n");
}

#[test]
fn test_multi_frame_sse_chunk_reconstitution() {
    let mut replacer =
        PiiRestorationStage::from_mapping(mapping(&[("<EMAIL_1>", "carol@domain.org")]));
    let processed = replacer.process_frame("data: {\"choices\":[{\"delta\":{\"content\":\"Contact <EMAIL_1>\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"finish_reason\":\"stop\"}}]}\n\n");
    assert!(
        processed.contains("carol@domain.org") && processed.contains("\"finish_reason\":\"stop\"")
    );
}

#[test]
fn test_streaming_tool_call_arguments_restoration() {
    let mut replacer =
        PiiRestorationStage::from_mapping(mapping(&[("<EMAIL_1>", "support@domain.org")]));
    let frame = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"to\\\": \\\"<EMAIL_1>\\\"}\"}}]}}]}\n\n";
    assert!(replacer.process_frame(frame).contains("support@domain.org"));
}

#[test]
fn test_crlf_multi_event_sse_chunk_processing() {
    let mut replacer =
        PiiRestorationStage::from_mapping(mapping(&[("<EMAIL_1>", "crlf_user@example.com")]));
    let out = replacer.process_frame("data: {\"choices\":[{\"delta\":{\"content\":\"Notice <EMA\"}}]}\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"IL_1> received.\"}}]}\r\n\r\ndata: [DONE]\r\n\r\n");
    assert!(
        out.contains("Notice ")
            && out.contains("crlf_user@example.com received.")
            && out.contains("data: [DONE]")
    );
}

#[test]
fn test_sse_metadata_preservation_with_event_and_comments() {
    let mut replacer =
        PiiRestorationStage::from_mapping(mapping(&[("<EMAIL_1>", "admin@openproxy.io")]));
    let out = replacer.process_frame(": ping keepalive\nevent: completion\nid: evt-99\ndata: {\"choices\":[{\"delta\":{\"content\":\"User is <EMAIL_1>\"}}]}\n\n");
    assert!(
        out.contains(": ping keepalive")
            && out.contains("event: completion")
            && out.contains("id: evt-99")
            && out.contains("admin@openproxy.io")
    );
}

#[test]
fn test_residual_flushing_channel_isolation() {
    let mut replacer =
        PiiRestorationStage::from_mapping(mapping(&[("<KEY_1>", "sk-proj-secret-1234567890")]));
    let out1 = replacer.process_frame(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Thinking about <KE\"}}]}\n\n",
    );
    assert_eq!(
        out1,
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Thinking about \"}}]}\n\n"
    );
    let out_done = replacer.process_frame("data: [DONE]\n\n");
    assert!(
        out_done.contains("\"reasoning_content\":\"<KE\"")
            && !out_done.contains("\"content\":\"<KE\"")
    );
}

#[test]
fn test_raw_sse_split_placeholders_flush_as_raw_at_done_and_eof() {
    for fake in ["alex.turner1@fastmail.com", "10.240.0.1"] {
        let original = if fake.contains('@') {
            "alice@example.com"
        } else {
            "192.0.2.42"
        };
        for prefix in ["", "Contact "] {
            for done in [false, true] {
                for split in 0..=fake.len() {
                    let mut sse = PiiRestorationStage::from_mapping(mapping(&[(fake, original)]));
                    let mut out =
                        sse.process_frame(&format!("data: {prefix}{}\n\n", &fake[..split]));
                    out.push_str(&sse.process_frame(&format!("data: {}\n\n", &fake[split..])));
                    if done {
                        out.push_str(&sse.process_frame("data: [DONE]\n\n"));
                    } else if let Some(residual) = sse.flush_residual_frame() {
                        out.push_str(&residual);
                    }
                    let text: String = out
                        .lines()
                        .filter_map(|line| line.strip_prefix("data: "))
                        .filter(|payload| *payload != "[DONE]")
                        .collect();
                    assert_eq!(
                        text,
                        format!("{prefix}{original}"),
                        "split={split} done={done} out={out:?}"
                    );
                    assert!(!out.contains("choices"), "raw transport changed: {out}");
                    assert!(sse.flush_residual_frame().is_none());
                }
            }
        }
    }
    let mut sse = PiiRestorationStage::from_mapping(mapping(&[("<EMAIL_1>", "alice@example.com")]));
    let mut out = sse.process_frame("data: Contact <EMA\n\n");
    out.push_str(&sse.process_frame("data: [DONE]\n\n"));
    assert!(out.contains("data: <EMA\n\n"));
    assert!(!out.contains("alice@example.com"));
    assert!(!out.contains("choices"));
}

#[test]
fn test_finish_reason_restores_complete_contact_at_chunk_edge() {
    let mut sse = PiiRestorationStage::from_mapping(mapping(&[(
        "alex.turner1@fastmail.com",
        "alice@example.com",
    )]));
    let mut out = sse.process_frame("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Contact alex.turner1@fastmail.com\"}}]}\n\n");
    out.push_str(&sse.process_frame(
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    ));
    assert_eq!(joined_content(&out), "Contact alice@example.com");
    assert!(out.contains("\"finish_reason\":\"stop\""));
    assert!(sse.flush_residual_frame().is_none());
}

#[test]
fn test_contact_sentence_punctuation_and_larger_tokens_at_every_split() {
    let fake = "alex.turner1@fastmail.com";
    let ip = "10.240.0.1";
    let ipv6 = "fd00::1";
    let pairs = [
        (fake, "alice@example.com"),
        (ip, "192.0.2.42"),
        (ipv6, "2001:db8::42"),
    ];
    for (input, expected) in [
        (
            format!("Contact {fake}. Next"),
            "Contact alice@example.com. Next".to_string(),
        ),
        (format!("{fake}.\n"), "alice@example.com.\n".to_string()),
        (format!("{fake}-evil"), format!("{fake}-evil")),
        (format!("{ip}. Next"), "192.0.2.42. Next".to_string()),
        (format!("{ip}.9 ok"), format!("{ip}.9 ok")),
        (format!("{ipv6}:9"), format!("{ipv6}:9")),
        (format!("a:{ipv6}"), format!("a:{ipv6}")),
    ] {
        for split in 0..=input.len() {
            let mut r = StreamingWindowReplacer::new(mapping(&pairs));
            let mut restored = r.process(&input[..split]);
            restored.push_str(&r.process(&input[split..]));
            restored.push_str(&r.flush());
            assert_eq!(restored, expected, "input={input:?} split={split}");
        }
    }
    let mut r = StreamingWindowReplacer::new(mapping(&[("a.a@b.com", "first@example.com")]));
    assert_eq!(
        r.process("a.a@b.com a.a@b.com!"),
        "first@example.com first@example.com!"
    );
}

#[test]
fn test_raw_non_json_sse_stream_restoration() {
    let mut session = PiiSession::new(true);
    let fake_email = session.get_or_create_placeholder(PiiEntity::Email, "alice@example.com");
    let mut sse_replacer = PiiRestorationStage::new(&session);
    assert_eq!(
        sse_replacer.process_frame(&format!("data: User is {fake_email}!\n\n")),
        "data: User is alice@example.com!\n\n"
    );
}

#[test]
fn test_stream_eof_without_done_sentinel_preserves_buffer() {
    let mut session = PiiSession::new(true);
    let fake_email = session.get_or_create_placeholder(PiiEntity::Email, "alice@example.com");
    let mut sse = PiiRestorationStage::new(&session);
    let prefix = &fake_email[..fake_email.len() - 5];
    assert_eq!(
        sse.process_frame(&format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"Notice {prefix}\"}}}}]}}\n\n"
        )),
        "data: {\"choices\":[{\"delta\":{\"content\":\"Notice \"}}]}\n\n"
    );
    let flush = sse.flush_residual_frame();
    assert!(flush.is_some() && flush.unwrap().contains(prefix));
}

#[test]
fn test_streaming_person_name_restoration_first_and_full() {
    let mut session = PiiSession::new(true);
    let p = session.get_or_create_placeholder(PiiEntity::Person, "Miguel");
    assert_eq!(p, "Alex Vance");

    let mut sse = PiiRestorationStage::new(&session);

    assert_eq!(
        sse.process_frame("data: {\"choices\":[{\"delta\":{\"content\":\"¡Hola \"}}]}\n\n"),
        "data: {\"choices\":[{\"delta\":{\"content\":\"¡Hola \"}}]}\n\n"
    );
    assert_eq!(
        sse.process_frame("data: {\"choices\":[{\"delta\":{\"content\":\"Alex\"}}]}\n\n"),
        "data: {\"choices\":[{\"delta\":{\"content\":\"\"}}]}\n\n"
    );
    assert_eq!(
        sse.process_frame("data: {\"choices\":[{\"delta\":{\"content\":\"! ¿Cómo estás?\"}}]}\n\n"),
        "data: {\"choices\":[{\"delta\":{\"content\":\"Miguel! ¿Cómo estás?\"}}]}\n\n"
    );

    assert_eq!(
        sse.process_frame("data: {\"choices\":[{\"delta\":{\"content\":\"Bienvenido \"}}]}\n\n"),
        "data: {\"choices\":[{\"delta\":{\"content\":\"Bienvenido \"}}]}\n\n"
    );
    assert_eq!(
        sse.process_frame("data: {\"choices\":[{\"delta\":{\"content\":\"Alex\"}}]}\n\n"),
        "data: {\"choices\":[{\"delta\":{\"content\":\"\"}}]}\n\n"
    );
    assert_eq!(
        sse.process_frame("data: {\"choices\":[{\"delta\":{\"content\":\" Vance.\"}}]}\n\n"),
        "data: {\"choices\":[{\"delta\":{\"content\":\"Miguel.\"}}]}\n\n"
    );
}

#[test]
fn test_streaming_multi_choice_isolation() {
    let mut sse = PiiRestorationStage::from_mapping(mapping(&[
        ("<EMAIL_1>", "alice@example.com"),
        ("<EMAIL_2>", "bob@example.com"),
    ]));

    let frame1 = sse.process_frame(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"User 0: <EMA\"}}]}\n\n",
    );
    let val1: serde_json::Value =
        serde_json::from_str(frame1.strip_prefix("data: ").unwrap().trim()).unwrap();
    assert_eq!(val1["choices"][0]["index"], 0);
    assert_eq!(val1["choices"][0]["delta"]["content"], "User 0: ");

    let frame2 = sse.process_frame(
        "data: {\"choices\":[{\"index\":1,\"delta\":{\"content\":\"User 1: Hello world\"}}]}\n\n",
    );
    let val2: serde_json::Value =
        serde_json::from_str(frame2.strip_prefix("data: ").unwrap().trim()).unwrap();
    assert_eq!(val2["choices"][0]["index"], 1);
    assert_eq!(
        val2["choices"][0]["delta"]["content"],
        "User 1: Hello world"
    );

    let frame3 = sse.process_frame(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"IL_1> done.\"}}]}\n\n",
    );
    let val3: serde_json::Value =
        serde_json::from_str(frame3.strip_prefix("data: ").unwrap().trim()).unwrap();
    assert_eq!(val3["choices"][0]["index"], 0);
    assert_eq!(
        val3["choices"][0]["delta"]["content"],
        "alice@example.com done."
    );
}

#[test]
fn test_streaming_multi_tool_calls_isolation() {
    let mut sse =
        PiiRestorationStage::from_mapping(mapping(&[("<EMAIL_1>", "support@example.com")]));

    let frame1 = sse.process_frame("data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"to\\\": \\\"<EMA\"}}]}}]}\n\n");
    let val1: serde_json::Value =
        serde_json::from_str(frame1.strip_prefix("data: ").unwrap().trim()).unwrap();
    assert_eq!(val1["choices"][0]["index"], 0);
    assert_eq!(val1["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
    assert_eq!(
        val1["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
        "{\"to\": \""
    );

    let frame2 = sse.process_frame("data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"{\\\"action\\\": \\\"ping\\\"}\"}}]}}]}\n\n");
    let val2: serde_json::Value =
        serde_json::from_str(frame2.strip_prefix("data: ").unwrap().trim()).unwrap();
    assert_eq!(val2["choices"][0]["index"], 0);
    assert_eq!(val2["choices"][0]["delta"]["tool_calls"][0]["index"], 1);
    assert_eq!(
        val2["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
        "{\"action\": \"ping\"}"
    );

    let frame3 = sse.process_frame("data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"IL_1>\\\"}\"}}]}}]}\n\n");
    let val3: serde_json::Value =
        serde_json::from_str(frame3.strip_prefix("data: ").unwrap().trim()).unwrap();
    assert_eq!(val3["choices"][0]["index"], 0);
    assert_eq!(val3["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
    assert_eq!(
        val3["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
        "support@example.com\"}"
    );
}

#[test]
fn test_streaming_tool_call_arguments_json_escaping_quotes_and_newlines() {
    let original_secret = "secret\"with\"quotes\nand\\backslashes";
    let mut sse = PiiRestorationStage::from_mapping(mapping(&[("<KEY_1>", original_secret)]));

    let frame1 = sse.process_frame("data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"secret\\\": \\\"<KE\"}}]}}]}\n\n");
    let frame2 = sse.process_frame("data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"Y_1>\\\"}\"}}]}}]}\n\n");

    let val1: serde_json::Value =
        serde_json::from_str(frame1.strip_prefix("data: ").unwrap().trim()).unwrap();
    let val2: serde_json::Value =
        serde_json::from_str(frame2.strip_prefix("data: ").unwrap().trim()).unwrap();

    let args1 = val1["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .unwrap();
    let args2 = val2["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .unwrap();

    let combined_args = format!("{args1}{args2}");
    let parsed: serde_json::Value = serde_json::from_str(&combined_args).expect(
        "tool arguments must remain valid parseable JSON despite quotes and newlines in original",
    );
    assert_eq!(parsed["secret"], original_secret);
}

#[test]
fn test_streaming_finish_reason_flushes_pending_tokens_before_stop() {
    let mut sse = PiiRestorationStage::from_mapping(mapping(&[("<EMAIL_1>", "alice@example.com")]));

    let _ = sse.process_frame(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Contact <EMA\"}}]}\n\n",
    );
    let finish_frame = sse.process_frame(
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    );

    assert!(
        finish_frame.contains("<EMA"),
        "finish_reason frame must contain pending buffer: {finish_frame}"
    );
    assert!(finish_frame.contains("\"finish_reason\":\"stop\""));
}

#[test]
fn test_streaming_token_boundary_prevents_substring_false_positives() {
    let mut replacer = StreamingWindowReplacer::new(mapping(&[
        ("Alex Vance", "Miguel"),
        ("sec_123", "secret-token"),
    ]));

    let text = "Hello Alex Vancey, my_Alex Vance, and Alex Vance. Also sec_12345 vs sec_123.";
    let processed = replacer.process(text);
    assert_eq!(
        processed,
        "Hello Alex Vancey, my_Alex Vance, and Miguel. Also sec_12345 vs secret-token."
    );
}

#[test]
fn test_streaming_defer_right_boundary_at_chunk_edge() {
    let mut replacer = StreamingWindowReplacer::new(mapping(&[("Alex Vance", "Miguel")]));

    let out1 = replacer.process("Hello Alex Vance");
    assert_eq!(out1, "Hello ");

    let out2 = replacer.process("y from sales.");
    assert_eq!(out2, "Alex Vancey from sales.");
}

#[test]
fn test_streaming_left_context_preserved_across_chunks() {
    let mut replacer = StreamingWindowReplacer::new(mapping(&[("Alex Vance", "Miguel")]));

    let out1 = replacer.process("prefix_Alex");
    assert_eq!(out1, "prefix_");

    let out2 = replacer.process(" Vance is here.");
    assert_eq!(out2, "Alex Vance is here.");
}

#[test]
fn test_streaming_phone_restoration_from_session_with_aliases() {
    let mut session = PiiSession::new(true);
    let placeholder = session.get_or_create_placeholder(PiiEntity::Phone, "+1-800-555-0199");
    assert_eq!(placeholder, "+1-202-555-0111");

    let mut sse = PiiRestorationStage::new(&session);
    let out = sse.process_frame(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Call +12025550111 now.\"}}]}\n\n",
    );
    assert_eq!(
        out,
        "data: {\"choices\":[{\"delta\":{\"content\":\"Call +1-800-555-0199 now.\"}}]}\n\n"
    );
}

/// The unary boundary rule (`structure_tests`) must hold for every chunk split
/// in the streaming path too: a placeholder that is a substring of a larger
/// contact token is never restored, while a real placeholder with sentence
/// punctuation is.
#[test]
fn test_streaming_contact_substring_boundaries_at_every_split() {
    let mut session = PiiSession::new(true);
    let email = session.get_or_create_placeholder(PiiEntity::Email, "alice@example.com");
    let ip = session.get_or_create_placeholder(PiiEntity::Ip, "192.0.2.42");

    for input in [
        format!("prefix.{email}"),
        format!("prefix+{email}"),
        format!("{email}.evil"),
        format!("Contact {email}."),
        format!("{ip}.9"),
        format!("See {ip} now."),
    ] {
        let expected = session.restore_text(&input);
        for split in 0..=input.len() {
            let mut replacer = StreamingWindowReplacer::from_session(&session);
            let mut restored = replacer.process(&input[..split]);
            restored.push_str(&replacer.process(&input[split..]));
            restored.push_str(&replacer.flush());
            assert_eq!(restored, expected, "input={input:?} split={split}");
        }
    }
}

/// Same invariant through the SSE stage: the deferred-token decision must not
/// depend on where the frame boundary falls.
#[test]
fn test_sse_contact_substring_boundaries_at_every_split() {
    let mut session = PiiSession::new(true);
    let email = session.get_or_create_placeholder(PiiEntity::Email, "alice@example.com");
    let ip = session.get_or_create_placeholder(PiiEntity::Ip, "192.0.2.42");

    for (text, must_contain_original, must_contain_placeholder) in [
        (format!("Contact {email}."), true, false),
        (format!("{email}.evil"), false, true),
        (format!("{ip}.9"), false, true),
    ] {
        for split in 0..=text.len() {
            let mut sse = PiiRestorationStage::new(&session);
            let frame = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}}}}]}}\n\n",
                json_escape(&text[..split])
            );
            let frame2 = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}}}}]}}\n\n",
                json_escape(&text[split..])
            );
            let mut out = sse.process_frame(&frame);
            out.push_str(&sse.process_frame(&frame2));
            // Terminate the stream so any pending buffer is flushed, exactly as
            // the real transport does at `[DONE]`.
            out.push_str(&sse.process_frame("data: [DONE]\n\n"));
            // The restored text can be split across frames (the placeholder
            // itself straddles the boundary), so join the decoded `content` of
            // every emitted frame before asserting.
            let restored = joined_content(&out);
            if must_contain_original {
                assert!(
                    restored.contains("alice@example.com") || restored.contains("192.0.2.42"),
                    "expected original in {restored:?} (split={split})"
                );
            }
            if must_contain_placeholder {
                let kept = restored.contains(&email) || restored.contains(&ip);
                assert!(
                    kept,
                    "expected placeholder kept in {restored:?} (split={split})"
                );
            }
            assert!(
                !restored.contains("alice@example.com.evil"),
                "split={split}"
            );
        }
    }
}

/// Concatenates the decoded `delta.content` of every `data:` frame in a chunk,
/// so a placeholder split across two frames is compared as one string.
fn joined_content(chunk: &str) -> String {
    let mut joined = String::new();
    for part in chunk.replace("\r\n", "\n").split("\n\n") {
        for line in part.lines() {
            let Some(payload) = line.trim_end().strip_prefix("data:").map(str::trim_start) else {
                continue;
            };
            let Ok(val) = serde_json::from_str::<serde_json::Value>(payload) else {
                continue;
            };
            if let Some(text) = val
                .get("choices")
                .and_then(|c| c.as_array())
                .and_then(|a| a.first())
                .and_then(|c| c.get("delta"))
                .and_then(|d| d.get("content"))
                .and_then(|t| t.as_str())
            {
                joined.push_str(text);
            }
        }
    }
    joined
}

/// JSON-string escaping helper for the SSE frames above (kept local so the
/// test file does not need a new dependency).
fn json_escape(s: &str) -> String {
    serde_json::to_string(s)
        .ok()
        .and_then(|q| {
            (q.starts_with('"') && q.ends_with('"') && q.len() >= 2)
                .then(|| q[1..q.len() - 1].to_string())
        })
        .unwrap_or_else(|| s.to_string())
}
