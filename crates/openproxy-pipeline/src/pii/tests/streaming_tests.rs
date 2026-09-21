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
fn test_raw_non_json_sse_stream_restoration() {
    let mut session = PiiSession::new(true);
    let fake_email = session.get_or_create_placeholder(PiiEntity::Email, "alice@example.com");
    let mut sse_replacer = PiiRestorationStage::new(&session);
    assert_eq!(
        sse_replacer.process_frame(&format!("data: User is {fake_email}\n\n")),
        "data: User is alice@example.com\n\n"
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

    // 1. First name only in streaming: "¡Hola " + "Alex" + "! ¿Cómo estás?"
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

    // 2. Full name across chunks: "Alex" + " Vance"
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
