use super::*;

fn check(got: (String, String), c: &str, r: &str) {
    assert_eq!(got.0, c);
    assert_eq!(got.1, r);
}

#[test]
fn test_extract_think_from_content_table() {
    let cases = [
        ("Hello, world!", "Hello, world!", "", false),
        (
            "<think>\nLet me think...\n</think>\nThe answer is 42.",
            "The answer is 42.",
            "Let me think...",
            true,
        ),
        ("<think>A</think>B<think>C</think>D", "BD", "A\nC", true),
        (
            "<THINK>reasoning</THINK>answer",
            "answer",
            "reasoning",
            true,
        ),
        (
            "<thinking>my thoughts</thinking>response",
            "response",
            "my thoughts",
            true,
        ),
        (
            "<reasoning>logic</reasoning>output",
            "output",
            "logic",
            true,
        ),
        (
            "<think>incomplete reasoning",
            "",
            "incomplete reasoning",
            true,
        ),
        ("<think></think>answer", "answer", "", false),
        (
            "<think>all reasoning, no answer</think>",
            "",
            "all reasoning, no answer",
            true,
        ),
        (
            "<think>\nNow I have a comprehensive understanding.\n</think>\n\n</think>",
            "",
            "Now I have a comprehensive understanding.",
            true,
        ),
        (
            "<think>reasoning</think>\n\n</think>\nThe answer.",
            "The answer.",
            "reasoning",
            true,
        ),
    ];
    for (input, exp_c, exp_r, has_r) in cases {
        let r = extract_think_from_content(input);
        assert_eq!(r.content, exp_c, "Failed content for {input}");
        assert_eq!(r.reasoning, exp_r, "Failed reasoning for {input}");
        assert_eq!(r.has_reasoning(), has_r);
    }
}

#[test]
fn stream_simple() {
    let mut ext = ThinkStreamExtractor::new();
    check(ext.process("<think>"), "", "");
    check(ext.process("reasoning here"), "", "reasoning here");
    check(ext.process("</think>"), "", "");
    check(ext.process("final answer"), "final answer", "");
    check(ext.flush(), "", "");
}

#[test]
fn stream_tag_split_across_chunks() {
    let mut ext = ThinkStreamExtractor::new();
    check(ext.process("Hello <thi"), "Hello ", "");
    check(
        ext.process("nk>reasoning</think> world"),
        " world",
        "reasoning",
    );
}

#[test]
fn stream_close_tag_split() {
    let mut ext = ThinkStreamExtractor::new();
    ext.process("<think>");
    check(
        ext.process("some reasoning here</thin"),
        "",
        "some reasoning here",
    );
    check(ext.process("k>answer"), "answer", "");
}

#[test]
fn stream_no_tags() {
    let mut ext = ThinkStreamExtractor::new();
    check(ext.process("just a normal "), "just a normal ", "");
    check(ext.process("response"), "response", "");
}

#[test]
fn stream_interleaved() {
    let mut ext = ThinkStreamExtractor::new();
    check(
        ext.process("<think>A</think>B<think>C</think>D"),
        "BD",
        "AC",
    );
}

#[test]
fn stream_flush_unterminated() {
    let mut ext = ThinkStreamExtractor::new();
    ext.process("<think>");
    check(ext.process("incomplete"), "", "incomplete");
    check(ext.flush(), "", "");
}

#[test]
fn stream_flush_partial_tag() {
    let mut ext = ThinkStreamExtractor::new();
    ext.process("hello <thi");
    check(ext.flush(), "<thi", "");
}

#[test]
fn non_streaming_no_duplicate_when_native_reasoning_present() {
    use crate::translation::{OpenAIChoice, OpenAIResponse};
    use openproxy_types::OpenAIMessage;
    let mut extra = serde_json::Map::new();
    extra.insert(
        "reasoning_content".into(),
        serde_json::Value::String("Let me think about this.".into()),
    );
    let resp = extract_think_from_response(OpenAIResponse {
        id: "test".into(),
        object: "chat.completion".into(),
        created: 0,
        model: "MiniMax-M3".into(),
        choices: vec![OpenAIChoice {
            index: 0,
            message: OpenAIMessage {
                role: "assistant".into(),
                content: Some(serde_json::Value::String(
                    "<think>Let me think about this.</think>The answer is 42.".into(),
                )),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra,
            },
            finish_reason: Some("stop".into()),
        }],
        usage: None,
    });
    assert_eq!(
        resp.choices[0]
            .message
            .content
            .as_ref()
            .unwrap()
            .as_str()
            .unwrap(),
        "The answer is 42."
    );
    assert_eq!(
        resp.choices[0]
            .message
            .extra
            .get("reasoning_content")
            .unwrap()
            .as_str()
            .unwrap(),
        "Let me think about this."
    );
}

#[test]
fn streaming_no_duplicate_when_native_reasoning_present() {
    let mut ext = ThinkStreamExtractor::new();
    check(ext.process("<think>\nLet me think"), "", "\nLet me think");
    check(ext.process(" about this."), "", " about this.");
    check(
        ext.process("</think>The answer is 42."),
        "The answer is 42.",
        "",
    );
}

#[test]
fn stream_multibyte_and_emoji_tests() {
    let mut ext = ThinkStreamExtractor::new();
    assert!(!ext.process("35 × 63 = 2205").0.is_empty());
    let mut ext2 = ThinkStreamExtractor::new();
    ext2.process("<think>");
    assert!(ext2.process("calculating 35 × 63").1.contains("×"));
    let mut ext3 = ThinkStreamExtractor::new();
    assert!(ext3.process("The answer is 42 🎉").0.contains("🎉"));
    assert_eq!(find_safe_split_point("35 × 63"), 8);
    assert_eq!(find_safe_split_point_close("35 × 63", "</think>"), 8);
}
