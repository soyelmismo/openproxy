//! Request/response wire, model-uid, quota and adapter-surface tests.

use super::super::models::*;
use super::super::protoc;
use super::super::quota::*;
use super::super::turn::*;
use super::super::wire::*;
use super::super::{DEVIN_CHAT_PATH, DEVIN_CLIENT_VERSION, DevinAdapter};
use super::{Fv, as_str, collect, get, get_all, msg, req_with, unwrap_envelope};
use crate::adapters::ProviderAdapter;
use bytes::Bytes;
use openproxy_types::OpenAIRequest;
use serde_json::json;

// ---------------------------------------------------------------------------
// build_devin_request golden decode
// ---------------------------------------------------------------------------

fn decode_request(bytes: &Bytes) -> Vec<(u32, Fv)> {
    let (flag, payload) = unwrap_envelope(bytes);
    assert_eq!(flag, 0);
    collect(&payload)
}

#[test]
fn build_devin_request_golden_fields() {
    let mut tool_msg = msg("tool", json!("done"));
    tool_msg.tool_call_id = Some("c1".into());
    let mut assistant = msg("assistant", json!("working"));
    assistant.tool_calls = Some(vec![json!({
        "id": "c1", "function": {"name": "run", "arguments": "{\"a\":true}"}
    })]);
    let mut req = req_with(vec![
        msg("system", json!("sys")),
        msg("user", json!("go")),
        assistant,
        tool_msg,
    ]);
    req.temperature = Some(0.5);
    req.max_tokens = Some(1234);
    req.extra.insert("session_id".into(), json!("sess-1"));
    req.tools = Some(vec![
        json!({"function": {"name": "run", "description": "d", "parameters": {"type": "object"}}}),
    ]);
    req.model = "swe-2".into();

    // Session turn counters are process-global: reset so repeated test runs
    // and other tests' sessions cannot shift this request's turn_index.
    let session_uuid = normalize_devin_uuid("sess-1");
    reset_devin_session_turn_index(&session_uuid);
    let body = build_devin_request(&req, "tok123", "swe-2", "").unwrap();
    let fields = decode_request(&body);

    // field 1 ClientMetadata
    let meta = collect(match get(&fields, 1).unwrap() {
        Fv::Bytes(b) => b,
        _ => panic!(),
    });
    assert_eq!(as_str(get(&meta, 1).unwrap()), "chisel");
    assert_eq!(as_str(get(&meta, 2).unwrap()), DEVIN_CLIENT_VERSION);
    assert_eq!(as_str(get(&meta, 3).unwrap()), "tok123");
    assert_eq!(as_str(get(&meta, 4).unwrap()), "en");
    assert_eq!(as_str(get(&meta, 5).unwrap()), "linux");
    assert_eq!(as_str(get(&meta, 12).unwrap()), "chisel");
    let fingerprint = as_str(get(&meta, 31).unwrap());
    assert_eq!(fingerprint.len(), 732);
    assert!(fingerprint.bytes().all(|b| b.is_ascii_hexdigit()));

    // field 2 system prompt
    assert_eq!(as_str(get(&fields, 2).unwrap()), "sys");

    // field 3 prompts: user, assistant(tool call), tool result
    let prompts = get_all(&fields, 3);
    assert_eq!(prompts.len(), 3);
    let sources: Vec<u64> = prompts
        .iter()
        .map(|p| {
            let sub = collect(match p {
                Fv::Bytes(b) => b,
                _ => panic!(),
            });
            match get(&sub, 2).unwrap() {
                Fv::Var(v) => *v,
                _ => panic!(),
            }
        })
        .collect();
    assert_eq!(
        sources,
        vec![
            DEVIN_SOURCE_USER,
            DEVIN_SOURCE_ASSISTANT,
            DEVIN_SOURCE_TOOL_RESULT
        ]
    );

    // prompt[2] tool_call_id
    let p2 = collect(match prompts[2] {
        Fv::Bytes(b) => b,
        _ => panic!(),
    });
    assert_eq!(as_str(get(&p2, 7).unwrap()), "c1");

    // prompt[1] tool call fields
    let p1 = collect(match prompts[1] {
        Fv::Bytes(b) => b,
        _ => panic!(),
    });
    let tc = collect(match get(&p1, 6).unwrap() {
        Fv::Bytes(b) => b,
        _ => panic!(),
    });
    assert_eq!(as_str(get(&tc, 1).unwrap()), "c1");
    assert_eq!(as_str(get(&tc, 2).unwrap()), "run");
    assert_eq!(as_str(get(&tc, 3).unwrap()), "{\"a\":true}");

    // field 7 varint 5
    assert_eq!(get(&fields, 7), Some(&Fv::Var(5)));

    // field 8 generation config
    let gen_cfg = collect(match get(&fields, 8).unwrap() {
        Fv::Bytes(b) => b,
        _ => panic!(),
    });
    assert_eq!(get(&gen_cfg, 1), Some(&Fv::Var(1)));
    assert_eq!(get(&gen_cfg, 2), Some(&Fv::Var(1234)));
    assert_eq!(get(&gen_cfg, 3), Some(&Fv::Var(400)));
    assert_eq!(get(&gen_cfg, 5), Some(&Fv::F64(0.5f64.to_bits())));
    assert_eq!(get(&gen_cfg, 7), Some(&Fv::Var(40)));
    assert_eq!(
        get(&gen_cfg, 8),
        Some(&Fv::F64(f64::from(0.95f32).to_bits()))
    );

    // field 10 tool
    let tool = collect(match get(&fields, 10).unwrap() {
        Fv::Bytes(b) => b,
        _ => panic!(),
    });
    assert_eq!(as_str(get(&tool, 1).unwrap()), "run");
    assert_eq!(
        String::from_utf8_lossy(match get(&tool, 3).unwrap() {
            Fv::Bytes(b) => b,
            _ => panic!(),
        }),
        "{\"type\":\"object\"}"
    );

    // field 15 session meta: last prompt is source 4 → no field 4
    let meta15 = collect(match get(&fields, 15).unwrap() {
        Fv::Bytes(b) => b,
        _ => panic!(),
    });
    // "sess-1" is not a valid UUID → normalized deterministically; first
    // request of the session emits turn_index 0, which is omitted on the wire.
    assert_eq!(as_str(get(&meta15, 1).unwrap()), session_uuid);
    assert!(get(&meta15, 2).is_none());
    assert_eq!(get(&meta15, 3), Some(&Fv::Var(4)));
    assert!(get(&meta15, 4).is_none());

    // field 16 cascade, 20 varint 1, 21 uid
    assert_eq!(as_str(get(&fields, 16).unwrap()), session_uuid);
    assert_eq!(get(&fields, 20), Some(&Fv::Var(1)));
    assert_eq!(as_str(get(&fields, 21).unwrap()), "swe-2-high");
}

#[test]
fn build_devin_request_clamps_nonpositive_temperature() {
    // Upstream sampler divides by temperature: a verbatim 0 (or negative)
    // makes every Devin model answer `invalid_argument`. The wire must clamp
    // it to the smallest positive value, which downstream reads as greedy.
    for (input, want_bits) in [
        (Some(0.0f32), f64::from(DEVIN_MIN_TEMPERATURE).to_bits()),
        (Some(-0.5f32), f64::from(DEVIN_MIN_TEMPERATURE).to_bits()),
        (Some(0.0001f32), f64::from(0.0001f32).to_bits()),
        (Some(0.5f32), f64::from(0.5f32).to_bits()),
        (Some(1.0f32), f64::from(1.0f32).to_bits()),
        (None, f64::from(1.0f32).to_bits()),
    ] {
        let mut req = req_with(vec![msg("user", json!("hi"))]);
        req.temperature = input;
        let body = build_devin_request(&req, "t", "swe-2", "").unwrap();
        let fields = decode_request(&body);
        let gen_cfg = collect(match get(&fields, 8).unwrap() {
            Fv::Bytes(b) => b,
            _ => panic!(),
        });
        assert_eq!(
            get(&gen_cfg, 5),
            Some(&Fv::F64(want_bits)),
            "temperature {input:?} produced wrong wire bits"
        );
    }
}

#[test]
fn build_devin_request_fresh_user_turn_sets_field_15_4() {
    let mut req = req_with(vec![msg("user", json!("hello"))]);
    req.extra.insert("sessionId".into(), json!("s2"));
    reset_devin_session_turn_index(&normalize_devin_uuid("s2"));
    let body = build_devin_request(&req, "t", "", "").unwrap();
    let fields = decode_request(&body);
    let meta15 = collect(match get(&fields, 15).unwrap() {
        Fv::Bytes(b) => b,
        _ => panic!(),
    });
    assert_eq!(get(&meta15, 4), Some(&Fv::Var(14)));
    // empty model → swe-2-high
    assert_eq!(as_str(get(&fields, 21).unwrap()), "swe-2-high");
    // no session → upstream still emits 15/16 with a fresh random UUID
    let req2 = req_with(vec![msg("user", json!("hi"))]);
    let body2 = build_devin_request(&req2, "t", "swe-2", "").unwrap();
    let fields2 = decode_request(&body2);
    let meta2 = collect(match get(&fields2, 15).unwrap() {
        Fv::Bytes(b) => b,
        _ => panic!(),
    });
    let generated = as_str(get(&meta2, 1).unwrap());
    assert!(uuid::Uuid::parse_str(&generated).is_ok());
    assert_eq!(as_str(get(&fields2, 16).unwrap()), generated);
}

#[test]
fn build_user_status_request_decodes_metadata() {
    let body = build_user_status_request("tok9", "");
    let fields = collect(&body);
    let meta = collect(match get(&fields, 1).unwrap() {
        Fv::Bytes(b) => b,
        _ => panic!(),
    });
    assert_eq!(as_str(get(&meta, 3).unwrap()), "tok9");
    assert_eq!(as_str(get(&meta, 1).unwrap()), "chisel");
}

// ---------------------------------------------------------------------------
// identity helpers
// ---------------------------------------------------------------------------

#[test]
fn identity_headers_shapes() {
    assert_eq!(basic_auth("abc"), "Basic abc-abc");
    let st = sentry_trace_header();
    let parts: Vec<&str> = st.split('-').collect();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0].len(), 32);
    assert_eq!(parts[1].len(), 16);
    assert_eq!(parts[2], "1");
    assert!(st.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    let fp = device_fingerprint("");
    assert_eq!(fp.len(), 732);
    assert!(fp.bytes().all(|b| b.is_ascii_hexdigit()));
    // seeded fingerprints are deterministic and still 732 lowercase hex chars
    assert_eq!(device_fingerprint("acct-1"), device_fingerprint("acct-1"));
    assert_ne!(device_fingerprint("acct-1"), device_fingerprint("acct-2"));
    let seeded = device_fingerprint("acct-1");
    assert_eq!(seeded.len(), 732);
    assert!(seeded.bytes().all(|b| b.is_ascii_hexdigit()));
}

// ---------------------------------------------------------------------------
// response frame parsing
// ---------------------------------------------------------------------------

#[test]
fn parse_devin_frame_full() {
    // tool call delta
    let mut tc = Vec::new();
    protoc::append_string(&mut tc, 1, "id1");
    protoc::append_string(&mut tc, 2, "run");
    protoc::append_string(&mut tc, 3, "{}");
    protoc::append_string(&mut tc, 4, "{");
    protoc::append_string(&mut tc, 5, "bad json");
    protoc::append_varint_field(&mut tc, 6, 1);

    // usage with headers + model name
    let mut hdr = Vec::new();
    protoc::append_string(&mut hdr, 1, "x-request-id");
    protoc::append_string(&mut hdr, 2, "req-9");
    let mut usage = Vec::new();
    protoc::append_varint_field(&mut usage, 2, 100);
    protoc::append_varint_field(&mut usage, 3, 40);
    protoc::append_varint_field(&mut usage, 4, 7);
    protoc::append_varint_field(&mut usage, 5, 12);
    protoc::append_varint_field(&mut usage, 6, 200);
    protoc::append_message(&mut usage, 8, &hdr);
    protoc::append_string(&mut usage, 9, "swe-2-high");

    // dim group: metric{5:key,4:{2:fixed32}}
    let mut metric_val = Vec::new();
    metric_val.push(0x15); // field2 wire5 tag
    metric_val.extend_from_slice(&1234.0f32.to_bits().to_le_bytes());
    let mut metric = Vec::new();
    protoc::append_string(&mut metric, 5, "input_tokens");
    protoc::append_message(&mut metric, 4, &metric_val);
    let mut dim = Vec::new();
    protoc::append_string(&mut dim, 1, "Token Usage");
    protoc::append_message(&mut dim, 2, &metric);

    let mut frame = Vec::new();
    protoc::append_string(&mut frame, 1, "out-1");
    protoc::append_varint_field(&mut frame, 2, 1700);
    protoc::append_string(&mut frame, 3, "hello ");
    protoc::append_string(&mut frame, 3, "world");
    protoc::append_varint_field(&mut frame, 4, 5);
    protoc::append_varint_field(&mut frame, 5, 2);
    protoc::append_message(&mut frame, 6, &tc);
    protoc::append_message(&mut frame, 7, &usage);
    protoc::append_string(&mut frame, 9, "think");
    protoc::append_bytes(&mut frame, 10, &[9, 9]);
    protoc::append_fixed64(&mut frame, 12, 12.5f64.to_bits());
    protoc::append_string(&mut frame, 17, "msg-7");
    protoc::append_string(&mut frame, 21, "thought_signature");
    protoc::append_message(&mut frame, 28, &dim);

    let f = parse_devin_frame(&frame).unwrap();
    assert_eq!(f.output_id, "out-1");
    assert_eq!(f.timestamp_secs, 1700);
    assert_eq!(f.text, "hello world");
    assert_eq!(f.delta_tokens, 5);
    assert_eq!(f.stop_reason, 2);
    assert_eq!(f.thinking_text, "think");
    assert_eq!(&f.signature[..], &[9, 9]);
    assert_eq!(f.signature_type, "thought_signature");
    assert_eq!(f.latency_ms, 12.5);
    assert_eq!(f.message_id, "msg-7");
    assert_eq!(f.tool_call_deltas.len(), 1);
    let d = &f.tool_call_deltas[0];
    assert_eq!(d.id, "id1");
    assert_eq!(d.invalid_json_str, "{");
    assert_eq!(d.invalid_json_err, "bad json");
    assert!(d.is_custom);
    let u = f.usage.as_ref().unwrap();
    assert_eq!(u.prompt_tokens, 100);
    assert_eq!(u.completion_tokens, 40);
    assert_eq!(u.cache_write_tokens, 7);
    assert_eq!(u.cached_tokens, 12);
    assert_eq!(u.status_code, 200);
    assert_eq!(u.request_id, "req-9");
    assert_eq!(u.model_name, "swe-2-high");
    assert!(f.dim_found);
    assert_eq!(f.dim_prompt, 1234);
}

#[test]
fn parse_devin_frame_usage_accumulates() {
    let mut usage = Vec::new();
    protoc::append_varint_field(&mut usage, 2, 10);
    protoc::append_varint_field(&mut usage, 4, 5);
    let mut frame = Vec::new();
    protoc::append_message(&mut frame, 7, &usage);
    protoc::append_message(&mut frame, 7, &usage);
    let f = parse_devin_frame(&frame).unwrap();
    let u = f.usage.unwrap();
    assert_eq!(u.prompt_tokens, 20);
    assert_eq!(u.cache_write_tokens, 10);
}

#[test]
fn parse_devin_frame_ts_submessage() {
    let mut ts = Vec::new();
    protoc::append_varint_field(&mut ts, 1, 42);
    let mut frame = Vec::new();
    protoc::append_message(&mut frame, 2, &ts);
    let f = parse_devin_frame(&frame).unwrap();
    assert_eq!(f.timestamp_secs, 42);
}

#[test]
fn parse_devin_frame_truncated_err() {
    // field 3 declares len 10, buffer ends early
    let bad = vec![0x1a, 0x0a, 0x01];
    assert!(parse_devin_frame(&bad).is_err());
}

// ---------------------------------------------------------------------------
// chat_model_uid resolution
// ---------------------------------------------------------------------------

#[test]
fn uid_empty_and_passthrough() {
    assert_eq!(resolve_devin_chat_model_uid("", "", 0), "swe-2-high");
    assert_eq!(
        resolve_devin_chat_model_uid("swe-2-high", "", 0),
        "swe-2-high"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("devin/SWE-2", "", 0),
        "swe-2-high"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("swe-2-low-lightning", "", 0),
        "swe-2-low-lightning"
    );
}

#[test]
fn uid_catalog_and_effort() {
    assert_eq!(resolve_devin_chat_model_uid("swe-2", "", 0), "swe-2-high");
    assert_eq!(
        resolve_devin_chat_model_uid("swe-2:max", "", 0),
        "swe-2-max"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("swe-2", "xhigh", 0),
        "swe-2-max"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("gpt-6-sol", "", 0),
        "gpt-6-sol-medium"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("gpt-6-sol:max", "", 0),
        "gpt-6-sol-max"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("gpt-6-sol:none", "", 0),
        "gpt-6-sol-none"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("gemini-4-argon", "", 0),
        "gemini-4-argon-high"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("kimi-k3", "", 0),
        "kimi-k3-high"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("unknown-thing", "", 0),
        "unknown-thing"
    );
    // budget fallback
    assert_eq!(
        resolve_devin_chat_model_uid("gpt-6-astra", "", 8000),
        "gpt-6-astra-medium"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("gpt-6-astra", "", 100),
        "gpt-6-astra-low"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("gpt-6-astra", "auto", 0),
        "gpt-6-astra-high"
    );
}

#[test]
fn uid_special_bases() {
    assert_eq!(resolve_devin_chat_model_uid("glm-5-2", "", 0), "glm-5-2");
    assert_eq!(
        resolve_devin_chat_model_uid("glm-5-2:max", "", 0),
        "glm-5-2-max"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("glm-5-2", "none", 0),
        "glm-5-2-none"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("glm-5-2-1m", "none", 0),
        "glm-5-2-none-1m"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("glm-5-2-1m", "max", 0),
        "glm-5-2-max-1m"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("swe-1-7", "medium", 0),
        "swe-1-7-medium"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("swe-1-7", "high", 0),
        "swe-1-7"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("swe-1-6", "fast", 0),
        "swe-1-6-fast"
    );
    assert_eq!(resolve_devin_chat_model_uid("swe-1-6", "", 0), "swe-1-6");
    assert_eq!(
        resolve_devin_chat_model_uid("claude-opus-4-6", "high", 0),
        "claude-opus-4-6-thinking"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("claude-opus-4-6", "", 0),
        "claude-opus-4-6"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("claude-sonnet-4-6-1m", "high", 0),
        "claude-sonnet-4-6-thinking-1m"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("swe-2-lightning", "", 0),
        "swe-2-high-lightning"
    );
    // "low" clamps to the nearest allowed level on the stem (swe-2 allows
    // medium|high|max) → medium.
    assert_eq!(
        resolve_devin_chat_model_uid("swe-2-lightning", "low", 0),
        "swe-2-medium-lightning"
    );
}

#[test]
fn uid_enum_aliases() {
    assert_eq!(
        resolve_devin_chat_model_uid("claude-haiku-4-5", "", 0),
        "MODEL_PRIVATE_11"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("gpt-4-1", "", 0),
        "MODEL_CHAT_GPT_4_1_2025_04_14"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("claude-sonnet-4-5", "high", 0),
        "MODEL_PRIVATE_3"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("claude-sonnet-4-5", "", 0),
        "MODEL_PRIVATE_2"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("model_gpt_5_2", "", 0),
        "MODEL_GPT_5_2_LOW"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("model_gpt_5_2", "high", 0),
        "MODEL_GPT_5_2_HIGH"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("model_gpt_5_2", "max", 0),
        "MODEL_GPT_5_2_XHIGH"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("model_google_gemini_3_0_flash", "", 0),
        "MODEL_GOOGLE_GEMINI_3_0_FLASH_HIGH"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("model_claude_4_5_opus", "high", 0),
        "MODEL_CLAUDE_4_5_OPUS_THINKING"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("model_claude_4_5_opus", "", 0),
        "MODEL_CLAUDE_4_5_OPUS"
    );
    assert_eq!(
        resolve_devin_chat_model_uid("gemini-3-flash", "", 0),
        "gemini-3-8-flash-high"
    );
}

// ---------------------------------------------------------------------------
// user status → quota
// ---------------------------------------------------------------------------

fn golden_user_status() -> Vec<u8> {
    let mut plan = Vec::new();
    protoc::append_string(&mut plan, 2, "Devin Teams");
    let mut org = Vec::new();
    protoc::append_string(&mut org, 4, "org-1");
    protoc::append_string(&mut org, 8, "Acme");
    protoc::append_message(&mut plan, 33, &org);

    let mut ts_start = Vec::new();
    protoc::append_varint_field(&mut ts_start, 1, 1_700_000_000);
    let mut ts_end = Vec::new();
    protoc::append_varint_field(&mut ts_end, 1, 1_800_000_000);

    let mut plan_status = Vec::new();
    protoc::append_message(&mut plan_status, 1, &plan);
    protoc::append_message(&mut plan_status, 2, &ts_start);
    protoc::append_message(&mut plan_status, 3, &ts_end);
    protoc::append_varint_field(&mut plan_status, 14, 73);
    protoc::append_varint_field(&mut plan_status, 15, 40);
    protoc::append_varint_field(&mut plan_status, 17, 1_700_086_400);
    protoc::append_varint_field(&mut plan_status, 18, 1_700_604_800);

    let mut us = Vec::new();
    protoc::append_string(&mut us, 3, "devin-user");
    protoc::append_string(&mut us, 5, "team-1");
    protoc::append_string(&mut us, 7, "u@x.io");
    protoc::append_message(&mut us, 13, &plan_status);
    protoc::append_string(&mut us, 36, "uid-1");

    let mut body = Vec::new();
    protoc::append_message(&mut body, 1, &us);
    body
}

#[test]
fn user_status_parse_to_quota() {
    let body = golden_user_status();
    let us = parse_user_status(&body).unwrap();
    assert_eq!(us.user_name, "devin-user");
    assert_eq!(us.team_id, "team-1");
    assert_eq!(us.email, "u@x.io");
    assert_eq!(us.user_id, "uid-1");
    assert_eq!(us.plan_name, "Devin Teams");
    assert_eq!(us.org_id, "org-1");
    assert_eq!(us.org_name, "Acme");
    assert_eq!(us.plan_start_secs, Some(1_700_000_000));
    assert_eq!(us.plan_end_secs, Some(1_800_000_000));
    assert_eq!(us.daily_remaining_pct, Some(73));
    assert_eq!(us.weekly_remaining_pct, Some(40));
    assert_eq!(us.daily_reset_unix, Some(1_700_086_400));
    assert_eq!(us.weekly_reset_unix, Some(1_700_604_800));

    let q = to_account_quota(&us);
    assert_eq!(q.session_limit, Some(100));
    assert_eq!(q.session_used, Some(27));
    assert_eq!(q.weekly_limit, Some(100));
    assert_eq!(q.weekly_used, Some(60));
    assert_eq!(q.plan_name.as_deref(), Some("Devin Teams"));
    assert!(q.session_reset_at.is_some());
    assert!(q.weekly_reset_at.is_some());
    let details = q.model_details.unwrap();
    assert_eq!(details.len(), 2);
    assert!((details[0].remaining_fraction - 0.73).abs() < 1e-9);
    assert!((details[1].remaining_fraction - 0.40).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// adapter surface
// ---------------------------------------------------------------------------

#[test]
fn adapter_headers_and_url() {
    let adapter = DevinAdapter::new();
    assert_eq!(adapter.config().id.as_str(), "devin");
    let url = adapter.build_chat_url(
        openproxy_types::TargetFormat::Devin,
        &openproxy_types::ModelId::new("swe-2"),
    );
    assert_eq!(url, format!("https://server.codeium.com{DEVIN_CHAT_PATH}"));

    let headers = adapter.build_headers(
        "tok1",
        openproxy_types::TargetFormat::Devin,
        &openproxy_types::ModelId::new("swe-2"),
    );
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    assert_eq!(get("authorization").as_deref(), Some("Basic tok1-tok1"));
    assert_eq!(
        get("content-type").as_deref(),
        Some("application/connect+proto")
    );
    assert_eq!(get("connect-protocol-version").as_deref(), Some("1"));
    assert_eq!(get("accept").as_deref(), Some("*/*"));
    assert!(get("sentry-trace").is_some());
    // Upstream ships an explicit empty User-Agent; empty also suppresses
    // openproxy's default UA injection downstream.
    assert_eq!(get("user-agent").as_deref(), Some(""));
}

#[test]
fn adapter_two_stage_body_build() {
    let adapter = DevinAdapter::new();
    let req = req_with(vec![msg("user", json!("hi"))]);
    let model = openproxy_types::ModelId::new("swe-2");
    let carrier = adapter
        .format_request(
            openproxy_types::TargetFormat::Devin,
            &req,
            &model,
            &req.messages,
            true,
        )
        .unwrap();
    // carrier is JSON
    assert!(serde_json::from_slice::<OpenAIRequest>(&carrier).is_ok());

    let resolved = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::ComboTarget::default(),
        model: openproxy_types::models::Model::default(),
        api_key: "secret-tok".into(),
        api_key_label: None,
        custom_meta: None,
    };
    let body = adapter
        .wrap_request_body(
            carrier,
            openproxy_types::TargetFormat::Devin,
            &model,
            &resolved,
        )
        .unwrap();
    let fields = decode_request(&body);
    let meta = collect(match get(&fields, 1).unwrap() {
        Fv::Bytes(b) => b,
        _ => panic!(),
    });
    assert_eq!(as_str(get(&meta, 3).unwrap()), "secret-tok");
    assert_eq!(as_str(get(&fields, 21).unwrap()), "swe-2-high");

    // an already-encoded envelope passes through untouched
    let env = Bytes::from(wrap_connect_envelope(b"\x08\x01"));
    let passthrough = adapter
        .wrap_request_body(
            env.clone(),
            openproxy_types::TargetFormat::Devin,
            &model,
            &resolved,
        )
        .unwrap();
    assert_eq!(passthrough, env);
}

#[test]
fn oauth_account_embeds_access_token_in_metadata() {
    // OAuth accounts carry an empty api_key; the session token lives in
    // custom_meta.access_token. ClientMetadata field 3 must embed that same
    // credential or Devin denies the call (permission_denied) even though
    // the Authorization header was right.
    let adapter = DevinAdapter::new();
    let req = req_with(vec![msg("user", json!("hi"))]);
    let model = openproxy_types::ModelId::new("swe-2");
    let carrier = adapter
        .format_request(
            openproxy_types::TargetFormat::Devin,
            &req,
            &model,
            &req.messages,
            true,
        )
        .unwrap();

    let meta = openproxy_types::context::CustomProviderMeta {
        access_token: "devin-session-token$oauth".into(),
        maybe_refresh: None,
        kiro_region: None,
        kiro_profile_arn: None,
        antigravity_project: None,
        antigravity_metadata: None,
        codex_workspace_id: None,
        claude_account_uuid: None,
        claude_metadata: None,
        devin_device_seed: None,
    };
    let resolved = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::ComboTarget::default(),
        model: openproxy_types::models::Model::default(),
        api_key: String::new(),
        api_key_label: None,
        custom_meta: Some(meta),
    };
    let body = adapter
        .wrap_request_body(
            carrier,
            openproxy_types::TargetFormat::Devin,
            &model,
            &resolved,
        )
        .unwrap();
    let fields = decode_request(&body);
    let client_meta = collect(match get(&fields, 1).unwrap() {
        Fv::Bytes(b) => b,
        _ => panic!(),
    });
    assert_eq!(
        as_str(get(&client_meta, 3).unwrap()),
        "devin-session-token$oauth"
    );
}

#[test]
fn devin_builtin_models_catalog() {
    let models = devin_builtin_models();
    assert_eq!(models.len(), 46);
    // swe-2 lists as swe-2:free (free cost tier on the wire).
    let swe2 = models
        .iter()
        .find(|m| m.model_id.as_str() == "swe-2:free")
        .unwrap();
    assert_eq!(swe2.display_name.as_deref(), Some("SWE-2"));
    assert_eq!(swe2.target_format, openproxy_types::TargetFormat::Devin);
    assert_eq!(swe2.context_length, Some(262_000));
    assert_eq!(swe2.max_output_tokens, Some(64_000));
    assert_eq!(swe2.family.as_deref(), Some("cognition"));
    assert_eq!(swe2.model_type.as_deref(), Some("chat"));
}

#[test]
fn free_models_emit_full_fraction_quota_detail() {
    use super::super::catalog::parse_client_model_configs;
    // A user_status whose catalog marks swe-2 free (cost tier 4 + promo) and
    // leaves a paid sibling (glm-5.3) untouched.
    let mut swe2_cfg = Vec::new();
    protoc::append_fixed32(&mut swe2_cfg, 3, 9.0f32.to_bits());
    protoc::append_string(&mut swe2_cfg, 22, "swe-2-high");
    let mut swe2_info = Vec::new();
    protoc::append_string(&mut swe2_info, 23, "swe-2");
    protoc::append_message(&mut swe2_cfg, 23, &swe2_info);
    protoc::append_varint_field(&mut swe2_cfg, 24, 4);
    let mut promo = Vec::new();
    protoc::append_varint_field(&mut promo, 1, 1);
    protoc::append_message(&mut swe2_cfg, 19, &promo);

    let mut paid_cfg = Vec::new();
    protoc::append_fixed32(&mut paid_cfg, 3, 1.0f32.to_bits());
    protoc::append_string(&mut paid_cfg, 22, "glm-5-3-low");
    let mut paid_info = Vec::new();
    protoc::append_string(&mut paid_info, 23, "glm-5.3");
    protoc::append_message(&mut paid_cfg, 23, &paid_info);
    protoc::append_varint_field(&mut paid_cfg, 24, 1);

    let mut catalog_blob = Vec::new();
    protoc::append_message(&mut catalog_blob, 1, &swe2_cfg);
    protoc::append_message(&mut catalog_blob, 1, &paid_cfg);

    let mut us_msg = Vec::new();
    protoc::append_message(&mut us_msg, 33, &catalog_blob);
    let mut body = Vec::new();
    protoc::append_message(&mut body, 1, &us_msg);

    let us = parse_user_status(&body).unwrap();
    assert_eq!(us.model_configs.len(), 2);

    let q = to_account_quota(&us);
    let details = q.model_details.unwrap();
    let free = details
        .iter()
        .find(|d| d.model_id == "swe-2:free")
        .expect("swe-2:free detail");
    assert_eq!(free.remaining_fraction, 1.0);
    assert_eq!(free.session_limit, 9_999);
    assert!(
        !details.iter().any(|d| d.model_id == "glm-5-3:free"),
        "paid models get no free detail"
    );
    // Catalog parsing cross-check: the fixture really did mark swe-2 free.
    assert!(parse_client_model_configs(&catalog_blob)[0].is_free());
}
