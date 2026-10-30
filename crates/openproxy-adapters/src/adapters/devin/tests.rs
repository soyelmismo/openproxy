//! Unit tests for the Devin Connect-RPC adapter.

use super::protoc::{self, ProtoReader};
use super::quota::{DevinQuotaKind, classify_devin_quota_exhaustion};
use super::response::devin_error_body_message;
use super::tools::*;
use super::turn::*;
use super::wire::*;
use openproxy_types::{CoreError, OpenAIMessage, OpenAIRequest};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Fv {
    Var(u64),
    Bytes(Vec<u8>),
    F64(u64),
    F32(u32),
}

/// Collect every top-level field of a protobuf message.
fn collect(buf: &[u8]) -> Vec<(u32, Fv)> {
    let mut r = ProtoReader::new(buf);
    let mut out = Vec::new();
    while let Some((f, w)) = r.tag() {
        match w {
            protoc::WIRE_VARINT => out.push((f, Fv::Var(r.varint().unwrap()))),
            protoc::WIRE_LEN => out.push((f, Fv::Bytes(r.bytes_value().unwrap().to_vec()))),
            protoc::WIRE_64BIT => out.push((f, Fv::F64(r.fixed64_value().unwrap()))),
            protoc::WIRE_32BIT => out.push((f, Fv::F32(r.fixed32_value().unwrap()))),
            other => panic!("unexpected wire type {other}"),
        }
    }
    out
}

fn get(fields: &[(u32, Fv)], field: u32) -> Option<&Fv> {
    fields.iter().find(|(f, _)| *f == field).map(|(_, v)| v)
}

fn get_all(fields: &[(u32, Fv)], field: u32) -> Vec<&Fv> {
    fields
        .iter()
        .filter(|(f, _)| *f == field)
        .map(|(_, v)| v)
        .collect()
}

fn as_str(v: &Fv) -> String {
    match v {
        Fv::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        _ => panic!("expected bytes field"),
    }
}

fn unwrap_envelope(frame: &[u8]) -> (u8, Vec<u8>) {
    assert!(frame.len() >= 5);
    let flag = frame[0];
    let len = u32::from_be_bytes(frame[1..5].try_into().unwrap()) as usize;
    assert_eq!(len, frame.len() - 5);
    (flag, frame[5..].to_vec())
}

fn msg(role: &str, content: Value) -> OpenAIMessage {
    OpenAIMessage {
        role: role.into(),
        content: Some(content),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: serde_json::Map::default(),
    }
}

fn req_with(messages: Vec<OpenAIMessage>) -> OpenAIRequest {
    OpenAIRequest {
        messages,
        ..OpenAIRequest::default()
    }
}

fn gzip_container(payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3];
    out.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(payload, 6));
    out.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out
}

// ---------------------------------------------------------------------------
// protoc
// ---------------------------------------------------------------------------

#[test]
fn protoc_roundtrip_all_wire_types() {
    let mut buf = Vec::new();
    protoc::append_varint_field(&mut buf, 1, 300);
    protoc::append_fixed64(&mut buf, 2, 0xdead_beef_cafe_f00d);
    protoc::append_string(&mut buf, 3, "héllo");
    protoc::append_bytes(&mut buf, 4, &[1, 2, 3]);
    protoc::append_tag(&mut buf, 5, protoc::WIRE_32BIT);
    buf.extend_from_slice(&42u32.to_le_bytes());
    protoc::append_message(&mut buf, 6, &[0x08, 0x01]);

    let mut r = ProtoReader::new(&buf);
    assert_eq!(r.tag(), Some((1, protoc::WIRE_VARINT)));
    assert_eq!(r.varint(), Some(300));
    assert_eq!(r.tag(), Some((2, protoc::WIRE_64BIT)));
    assert_eq!(r.fixed64_value(), Some(0xdead_beef_cafe_f00d));
    assert_eq!(r.tag(), Some((3, protoc::WIRE_LEN)));
    assert_eq!(r.bytes_value(), Some("héllo".as_bytes()));
    assert_eq!(r.tag(), Some((4, protoc::WIRE_LEN)));
    assert_eq!(r.bytes_value(), Some(&[1, 2, 3][..]));
    assert_eq!(r.tag(), Some((5, protoc::WIRE_32BIT)));
    assert_eq!(r.fixed32_value(), Some(42));
    assert_eq!(r.tag(), Some((6, protoc::WIRE_LEN)));
    assert_eq!(r.bytes_value(), Some(&[0x08, 0x01][..]));
    assert!(r.is_done());
    assert_eq!(r.tag(), None);
}

#[test]
fn protoc_multi_byte_varint() {
    let mut buf = Vec::new();
    protoc::append_varint(&mut buf, u64::MAX);
    let mut r = ProtoReader::new(&buf);
    assert_eq!(r.varint(), Some(u64::MAX));
}

#[test]
fn protoc_malformed_and_truncated() {
    // truncated varint (continuation bit, no successor)
    assert_eq!(ProtoReader::new(&[0x80]).varint(), None);
    // >10 continuation bytes → malformed
    assert_eq!(ProtoReader::new(&[0xff; 11]).varint(), None);
    // tag with field 0 → None
    assert_eq!(ProtoReader::new(&[0x00]).tag(), None);
    // truncated len-delimited
    let mut r = ProtoReader::new(&[0x12, 0x05, 0x01]);
    assert_eq!(r.tag(), Some((2, protoc::WIRE_LEN)));
    assert_eq!(r.bytes_value(), None);
    // truncated fixed64
    let mut r = ProtoReader::new(&[0x09, 0x01, 0x02]);
    assert_eq!(r.tag(), Some((1, protoc::WIRE_64BIT)));
    assert_eq!(r.fixed64_value(), None);
    // group wire types rejected by skip
    let mut r = ProtoReader::new(&[0x0b]);
    assert_eq!(r.tag(), Some((1, 3)));
    assert_eq!(r.skip(3), None);
}

#[test]
fn protoc_skip_fields() {
    let mut buf = Vec::new();
    protoc::append_varint_field(&mut buf, 1, 7);
    protoc::append_string(&mut buf, 2, "skipme");
    protoc::append_varint_field(&mut buf, 3, 9);
    let mut r = ProtoReader::new(&buf);
    assert_eq!(r.tag(), Some((1, protoc::WIRE_VARINT)));
    r.skip(protoc::WIRE_VARINT).unwrap();
    assert_eq!(r.tag(), Some((2, protoc::WIRE_LEN)));
    r.skip(protoc::WIRE_LEN).unwrap();
    assert_eq!(r.tag(), Some((3, protoc::WIRE_VARINT)));
    assert_eq!(r.varint(), Some(9));
}

// ---------------------------------------------------------------------------
// Connect envelope / deframer / gzip
// ---------------------------------------------------------------------------

#[test]
fn connect_envelope_wrap() {
    let env = wrap_connect_envelope(b"abc");
    assert_eq!(&env[..5], &[0, 0, 0, 0, 3]);
    assert_eq!(&env[5..], b"abc");
    assert!(looks_like_connect_envelope(&env));
    assert!(!looks_like_connect_envelope(b"{}"));
}

#[test]
fn deframer_single_and_multi_frame() {
    let mut stream = wrap_connect_envelope(b"one");
    stream.extend_from_slice(&wrap_connect_envelope(b"two"));
    let mut d = DevinDeframer::new();
    d.push(&stream);
    let f1 = d.next_frame().unwrap().unwrap();
    assert_eq!(f1.flag, 0);
    assert_eq!(&f1.payload[..], b"one");
    let f2 = d.next_frame().unwrap().unwrap();
    assert_eq!(&f2.payload[..], b"two");
    assert!(d.next_frame().unwrap().is_none());
    assert!(d.is_empty());
    assert!(!d.has_partial());
}

#[test]
fn deframer_split_feed() {
    let stream = wrap_connect_envelope(b"fragmented-payload");
    let mut d = DevinDeframer::new();
    let mut frame = None;
    for chunk in stream.chunks(3) {
        d.push(chunk);
        if let Some(f) = d.next_frame().unwrap() {
            frame = Some(f);
        }
    }
    let frame = frame.unwrap();
    assert_eq!(&frame.payload[..], b"fragmented-payload");
    assert!(d.is_empty());
}

#[test]
fn deframer_partial_then_bad_flag_and_oversize() {
    let mut d = DevinDeframer::new();
    d.push(&[0x00, 0x00, 0x00]);
    assert!(d.next_frame().unwrap().is_none());
    assert!(d.has_partial());

    let mut d = DevinDeframer::new();
    d.push(&[0x07, 0, 0, 0, 0]);
    assert!(matches!(d.next_frame(), Err(DevinWireError::BadFlag(7))));

    let mut d = DevinDeframer::new();
    let mut huge = vec![0x00];
    huge.extend_from_slice(&(17u32 * 1024 * 1024).to_be_bytes());
    d.push(&huge);
    assert!(matches!(
        d.next_frame(),
        Err(DevinWireError::FrameTooLarge(_))
    ));
}

#[test]
fn gunzip_roundtrip_and_deframer_compressed_frame() {
    let payload = b"hello gzip world".repeat(50);
    let container = gzip_container(&payload);
    let out = gunzip(&container).unwrap();
    assert_eq!(out, payload);

    // compressed + end-stream flag preserved, payload decompressed
    let mut frame = vec![0x03];
    frame.extend_from_slice(&(container.len() as u32).to_be_bytes());
    frame.extend_from_slice(&container);
    let mut d = DevinDeframer::new();
    d.push(&frame);
    let ev = d.next_frame().unwrap().unwrap();
    assert_eq!(ev.flag, 0x03);
    assert!(ev.is_end_stream());
    assert_eq!(&ev.payload[..], &payload[..]);
}

#[test]
fn gunzip_with_optional_header_fields() {
    // FLG = FNAME|FCOMMENT|FHCRC|FEXTRA
    let mut container = vec![0x1f, 0x8b, 8, 0x1e, 0, 0, 0, 0, 0, 3];
    container.extend_from_slice(&2u16.to_le_bytes()); // XLEN
    container.extend_from_slice(b"ab"); // FEXTRA
    container.extend_from_slice(b"name\0"); // FNAME
    container.extend_from_slice(b"comment\0"); // FCOMMENT
    let header_crc = crc32fast::hash(&container) as u16;
    container.extend_from_slice(&header_crc.to_le_bytes()); // FHCRC
    container.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(b"data", 6));
    container.extend_from_slice(&crc32fast::hash(b"data").to_le_bytes());
    container.extend_from_slice(&4u32.to_le_bytes());
    assert_eq!(gunzip(&container).unwrap(), b"data");
}

#[test]
fn gunzip_rejects_bad_input() {
    assert!(gunzip(b"short").is_err());
    assert!(gunzip(&[0x1f, 0x8b, 7, 0, 0, 0, 0, 0, 0, 0]).is_err()); // method
    assert!(gunzip(&[0x1f, 0x8c, 8, 0, 0, 0, 0, 0, 0, 0]).is_err()); // magic
}

// ---------------------------------------------------------------------------
// turn mapping
// ---------------------------------------------------------------------------

#[test]
fn turn_system_user_assistant_tool_flow() {
    let mut tool_msg = msg("tool", json!("42"));
    tool_msg.tool_call_id = Some("call_1".into());

    let mut assistant = msg("assistant", json!("thinking..."));
    assistant.tool_calls = Some(vec![json!({
        "id": "call_1",
        "type": "function",
        "function": {"name": "calc", "arguments": "{\"x\":1}"}
    })]);

    let req = req_with(vec![
        msg("system", json!("be brief")),
        msg("user", json!("hi")),
        assistant,
        tool_msg,
    ]);
    let turn = map_openai_turn(&req.messages).unwrap();
    assert_eq!(turn.system_prompt, "be brief");
    assert_eq!(turn.prompts.len(), 3);
    assert_eq!(turn.prompts[0].source, DEVIN_SOURCE_USER);
    assert_eq!(turn.prompts[0].content, "hi");
    assert_eq!(turn.prompts[1].source, DEVIN_SOURCE_ASSISTANT);
    assert_eq!(turn.prompts[1].tool_calls.len(), 1);
    assert_eq!(turn.prompts[1].tool_calls[0].name, "calc");
    assert_eq!(turn.prompts[1].tool_calls[0].arguments, "{\"x\":1}");
    assert_eq!(turn.prompts[2].source, DEVIN_SOURCE_TOOL_RESULT);
    assert_eq!(turn.prompts[2].tool_call_id, "call_1");
    assert_eq!(turn.prompts[2].content, "42");
    assert!(!turn.prompts[2].is_orphaned);
}

#[test]
fn turn_orphan_tool_result_becomes_user() {
    let mut tool_msg = msg("tool", json!("stale"));
    tool_msg.tool_call_id = Some("call_zzz".into());
    let req = req_with(vec![msg("user", json!("hi")), tool_msg]);
    let turn = map_openai_turn(&req.messages).unwrap();
    assert_eq!(turn.prompts.len(), 2);
    assert_eq!(turn.prompts[1].source, DEVIN_SOURCE_USER);
    assert!(turn.prompts[1].is_orphaned);
    assert_eq!(turn.prompts[1].content, "stale");
}

#[test]
fn turn_consecutive_assistant_merge() {
    let req = req_with(vec![
        msg("user", json!("q")),
        msg("assistant", json!("part one")),
        msg("assistant", json!("part two")),
        msg("user", json!("next")),
    ]);
    let turn = map_openai_turn(&req.messages).unwrap();
    let assistants: Vec<_> = turn
        .prompts
        .iter()
        .filter(|p| p.source == DEVIN_SOURCE_ASSISTANT)
        .collect();
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0].content, "part one\npart two");
}

#[test]
fn turn_image_extraction_and_header() {
    let req = req_with(vec![msg(
        "user",
        json!([
            {"type": "text", "text": "what is this?"},
            {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,QUJD"}}
        ]),
    )]);
    let turn = map_openai_turn(&req.messages).unwrap();
    let p = &turn.prompts[0];
    assert_eq!(p.images.len(), 1);
    assert_eq!(p.images[0].mime, "image/jpeg");
    assert_eq!(p.images[0].data_base64, "QUJD");
    assert!(p.content.starts_with("[Image 1: pasted_image_1.jpg]"));
    assert!(p.content.contains("what is this?"));
}

#[test]
fn turn_dropped_media_errors_when_run_empties() {
    // A user turn with only unsendable media → validation error.
    let req = req_with(vec![msg(
        "user",
        json!([{"type": "video", "video": {"url": "x"}}]),
    )]);
    let err = map_openai_turn(&req.messages).unwrap_err();
    assert!(matches!(err, CoreError::Validation(m) if m == "unsupported content part: video"));

    // Same turn alongside a content-bearing tool result → prompt dropped, no error.
    let mut tool_msg = msg("tool", json!("result"));
    tool_msg.tool_call_id = Some("c1".into());
    let req = req_with(vec![
        msg("user", json!([{"type": "video", "video": {"url": "x"}}])),
        tool_msg,
    ]);
    // tool result is orphaned (no pending call) → source 1, run still has content.
    let turn = map_openai_turn(&req.messages).unwrap();
    assert_eq!(turn.prompts.len(), 1);
    assert!(turn.prompts[0].is_orphaned);
}

#[test]
fn turn_empty_tool_result_placeholder() {
    let mut tool_msg = msg("tool", json!("   "));
    tool_msg.tool_call_id = Some("c1".into());
    let mut assistant = msg("assistant", json!(""));
    assistant.tool_calls = Some(vec![
        json!({"id": "c1", "function": {"name": "t", "arguments": "{}"}}),
    ]);
    let req = req_with(vec![assistant, tool_msg]);
    let turn = map_openai_turn(&req.messages).unwrap();
    // upstream emits an empty JSON object for empty tool results
    assert_eq!(turn.prompts[1].content, "{}");
}

#[test]
fn turn_assistant_thinking_and_signature() {
    let mut assistant = msg(
        "assistant",
        json!([
            {"type": "thinking", "thinking": "step A"},
            {"type": "text", "text": "answer"}
        ]),
    );
    assistant
        .extra
        .insert("reasoning".into(), json!("extra thought"));
    assistant
        .extra
        .insert("signature".into(), json!("deadbeef"));
    let req = req_with(vec![assistant]);
    let turn = map_openai_turn(&req.messages).unwrap();
    let p = &turn.prompts[0];
    assert_eq!(p.content, "answer");
    assert_eq!(p.thinking, "step A\n\nextra thought");
    // upstream stores unclassified opaque strings raw under "sealed"
    assert_eq!(p.signature, b"deadbeef");
    assert_eq!(p.signature_type, "sealed");
}

#[test]
fn turn_tool_result_content_parts() {
    let mut tool_msg = msg(
        "tool",
        json!([{"type": "text", "text": "line1"}, {"type": "text", "text": "line2"}]),
    );
    tool_msg.tool_call_id = Some("c1".into());
    let mut assistant = msg("assistant", json!(""));
    assistant.tool_calls = Some(vec![
        json!({"id": "c1", "function": {"name": "t", "arguments": "{}"}}),
    ]);
    let req = req_with(vec![assistant, tool_msg]);
    let turn = map_openai_turn(&req.messages).unwrap();
    assert_eq!(turn.prompts[1].content, "line1\nline2");
}

// ---------------------------------------------------------------------------
// tool collection + sanitization
// ---------------------------------------------------------------------------

#[test]
fn collect_devin_tools_basic_and_skip_codex() {
    let tools = vec![
        json!({"type": "function", "function": {"name": "read_file", "description": "d", "parameters": {"type": "object"}}}),
        json!({"type": "function", "function": {"name": "mcp__codex_app__automation_update", "description": "x"}}),
        json!({"type": "function", "function": {"name": "", "description": "empty"}}),
    ];
    let out = collect_devin_tools(&tools);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].name, "read_file");
    assert_eq!(
        String::from_utf8_lossy(&out[0].parameters),
        "{\"type\":\"object\"}"
    );
}

#[test]
fn sanitize_exec_command_description() {
    let out = sanitize_devin_tool_description(
        "shell_exec_command",
        "Run a command returning output or a session ID for ongoing interaction.",
    );
    assert!(out.contains("returning output or an session ID for ongoing interaction"));
    // Case-insensitive needle.
    let upper = sanitize_devin_tool_description(
        "x__exec_command",
        "RETURNING OUTPUT OR A SESSION ID FOR ONGOING INTERACTION",
    );
    assert!(upper.contains("an session ID"));
    // Unrelated names untouched.
    let untouched = sanitize_devin_tool_description(
        "read_file",
        "returning output or a session ID for ongoing interaction",
    );
    assert!(untouched.contains(" a session ID"));
}

#[test]
fn sanitize_write_stdin_description() {
    let out = sanitize_devin_tool_description(
        "tty_write_stdin",
        "Writes characters to an existing unified exec session and returns recent output.",
    );
    assert!(out.ends_with("returns recent output."));
    assert!(out.contains("a existing unified exec session"));
    // Without trailing period.
    let no_period = sanitize_devin_tool_description(
        "__write_stdin",
        "Writes characters to an existing unified exec session and returns recent output",
    );
    assert!(no_period.contains("a existing unified exec session"));
    assert!(!no_period.ends_with('.'));
}

// ---------------------------------------------------------------------------
// trailer errors + quota exhaustion classification
// ---------------------------------------------------------------------------

#[test]
fn trailer_error_mapping() {
    assert_eq!(parse_devin_trailer_error(b""), None);
    assert_eq!(parse_devin_trailer_error(b"{}"), None);
    assert_eq!(parse_devin_trailer_error(b"not json"), None);
    assert_eq!(
        parse_devin_trailer_error(
            br#"{"error":{"code":"invalid_argument","message":"bad input"}}"#
        ),
        Some((
            400,
            "devin upstream error (invalid_argument): bad input".into()
        ))
    );
    assert_eq!(
        parse_devin_trailer_error(
            br#"{"error":{"code":"invalid_argument","message":"internal error in x"}}"#
        )
        .map(|(s, _)| s),
        Some(502)
    );
    for (code, msg, want) in [
        ("internal", "boom", 502),
        ("unauthenticated", "no auth", 401),
        ("permission_denied", "server under high demand", 429),
        ("permission_denied", "forbidden", 403),
        ("resource_exhausted", "quota", 429),
        ("unavailable", "down", 503),
        ("canceled", "ctx done", 499),
        ("deadline_exceeded", "slow", 504),
        ("failed_precondition", "daily quota exhausted", 429),
        ("failed_precondition", "acu limit reached", 429),
        ("failed_precondition", "plain failure", 400),
    ] {
        let payload = format!(r#"{{"error":{{"code":"{code}","message":"{msg}"}}}}"#);
        assert_eq!(
            parse_devin_trailer_error(payload.as_bytes()).map(|(s, _)| s),
            Some(want),
            "code={code} msg={msg}"
        );
    }
    // Unknown codes default to 502 like upstream's switch fallthrough.
    assert_eq!(
        parse_devin_trailer_error(br#"{"error":{"code":"unknown_code","message":"x"}}"#)
            .map(|(s, _)| s),
        Some(502)
    );
}

#[test]
fn classify_quota_exhaustion() {
    assert_eq!(
        classify_devin_quota_exhaustion("your Daily quota is exhausted"),
        Some(DevinQuotaKind::Daily)
    );
    assert_eq!(
        classify_devin_quota_exhaustion("Weekly quota exhausted; daily also"),
        Some(DevinQuotaKind::Weekly)
    );
    assert_eq!(classify_devin_quota_exhaustion("quota exhausted"), None);
    assert_eq!(classify_devin_quota_exhaustion("exhausted"), None);
    assert_eq!(classify_devin_quota_exhaustion("quota"), None);
    // Monthly ACU exhaustion is a different pool: never upgrades to the
    // daily/weekly cooldown (upstream keeps it deliberately narrow).
    assert_eq!(
        classify_devin_quota_exhaustion("monthly acu quota exhausted"),
        None
    );
    // Generic rate limit without the quota word.
    assert_eq!(
        classify_devin_quota_exhaustion("daily request exhausted"),
        None
    );
}

#[test]
fn error_body_message_strips_wrapper() {
    // The trailer wrapper adds `devin upstream error (code): ` around the
    // raw upstream message; classification must see only the raw text so
    // the code cannot supply the "exhausted" word itself.
    assert_eq!(
        devin_error_body_message("devin upstream error (resource_exhausted): weekly quota limit"),
        "weekly quota limit"
    );
    assert_eq!(
        devin_error_body_message(
            "devin upstream error (failed_precondition): daily quota exhausted"
        ),
        "daily quota exhausted"
    );
    // Raw HTTP error bodies pass through unchanged.
    assert_eq!(devin_error_body_message("plain body"), "plain body");
    // False-positive guard: code alone carrying "exhausted" must not match.
    assert_eq!(
        classify_devin_quota_exhaustion(devin_error_body_message(
            "devin upstream error (resource_exhausted): weekly quota limit"
        )),
        None
    );
}

mod request;
