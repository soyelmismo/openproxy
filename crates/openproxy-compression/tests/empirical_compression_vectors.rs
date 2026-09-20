//! Empirical verification of compression filters (Lite filter and RTK line filter).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use openproxy_compression::lite::collapse_whitespace;
use openproxy_compression::rtk::apply_rtk;
use openproxy_types::OpenAIMessage;
use serde_json::Value;

fn make_msg(role: &str, content: &str) -> OpenAIMessage {
    OpenAIMessage {
        role: role.to_string(),
        content: Some(Value::String(content.to_string())),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: serde_json::Map::default(),
    }
}

fn get_text(msg: &OpenAIMessage) -> &str {
    msg.content.as_ref().and_then(|v| v.as_str()).unwrap_or("")
}

#[test]
fn test_lite_collapse_whitespace_table_vectors() {
    let test_cases = [
        ("line1\n\n\n\n\nline2", "line1\n\nline2", true),
        ("para1\n\npara2", "para1\n\npara2", false),
        ("line1   \nline2\t\nline3", "line1\nline2\nline3", true),
        ("line1\nline2   ", "line1\nline2", true),
        ("line1\nline2\n\npara2", "line1\nline2\n\npara2", false),
        ("hello 世界   \nnext line", "hello 世界\nnext line", true),
        ("😀😀😀\n\n\n😀😀", "😀😀😀\n\n😀😀", true),
    ];

    for (input, expected, should_change) in test_cases {
        let mut msgs = vec![make_msg("user", input)];
        let applied = collapse_whitespace(&mut msgs);
        let actual = get_text(&msgs[0]);

        assert_eq!(actual, expected, "mismatch for input: {input:?}");
        if should_change {
            assert!(
                !applied.is_empty(),
                "expected techniques applied for {input:?}"
            );
        } else {
            assert!(
                applied.is_empty(),
                "expected NO techniques applied for {input:?}"
            );
        }
    }
}

#[test]
fn test_rtk_ansi_stripping_and_git_filter_vectors() {
    let cases = [
        (
            "\u{1b}[32mOn branch main\u{1b}[0m\n  (use \"git add\" to update)\n\tmodified: foo.rs\nnothing added to commit\n",
            true,
        ),
        (
            "\x1b[31mred error\x1b[0m: command failed with code 1\n",
            true,
        ),
        (
            "Just a simple conversation text that is not a CLI command output.",
            false,
        ),
    ];

    for (input, should_compress) in cases {
        let mut msgs = vec![make_msg("user", input)];
        let applied = apply_rtk(&mut msgs);
        let result = get_text(&msgs[0]);

        if should_compress {
            assert!(!applied.is_empty(), "expected RTK rules applied for input");
            assert!(
                !result.contains("\u{1b}["),
                "ANSI escapes should be stripped"
            );
        } else {
            assert_eq!(result, input);
        }
    }
}

#[test]
fn test_compression_multibyte_and_emoji_stress() {
    let input = "🌟 Star 🌟   \n\n\n\n\n🚀 Rocket 🚀\t\t\n\n\n   🦀 Rust 🦀   ";
    let mut msgs = vec![make_msg("user", input)];
    let _ = collapse_whitespace(&mut msgs);
    let out = get_text(&msgs[0]);

    assert!(out.contains("🌟 Star 🌟"));
    assert!(out.contains("🚀 Rocket 🚀"));
    assert!(out.contains("🦀 Rust 🦀"));
    // Verify no 3+ consecutive newlines exist
    assert!(!out.contains("\n\n\n"));
}
