use super::*;

fn msg(role: &str, content: &str) -> OpenAIMessage {
    OpenAIMessage {
        role: role.to_string(),
        content: Some(Value::String(content.to_string())),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: serde_json::Map::default(),
    }
}

#[test]
fn test_compress_pytest_output() {
    let mut lines: Vec<String> = Vec::new();
    lines.push(
        "========================= test session starts =========================".to_string(),
    );
    for i in 0..95 {
        lines.push(format!("test_module.py::test_pass_{i} PASSED [ 50%]"));
    }
    for i in 0..3 {
        lines.push(format!("test_module.py::test_fail_{i} FAILED [ 50%]"));
    }
    lines.push(
        "========================= 3 failed, 95 passed in 5.0s ========================="
            .to_string(),
    );
    assert_eq!(lines.len(), 100);
    let content = lines.join("\n");
    let mut msgs = vec![msg("tool", &content)];
    let applied = compress_logs(&mut msgs);
    assert!(
        applied.contains(&TECHNIQUE),
        "should compress pytest output, got: {applied:?}"
    );
    let output = msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap();
    assert!(
        output.contains("FAILED"),
        "should keep FAILED lines: {output}"
    );
    assert!(
        output.contains("passed"),
        "should keep summary line: {output}"
    );
    assert!(output.contains("[#log_compressed:"));
    assert!(output.len() < content.len());
}

#[test]
fn test_compress_cargo_test_output() {
    let mut lines: Vec<String> = Vec::new();
    lines.push("running 80 tests".to_string());
    for i in 0..78 {
        lines.push(format!("test test_{i} ... ok"));
    }
    lines.push(
        "test result: ok. 78 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s".to_string(),
    );
    assert_eq!(lines.len(), 80);
    let content = lines.join("\n");
    let mut msgs = vec![msg("tool", &content)];
    let applied = compress_logs(&mut msgs);
    assert!(
        applied.contains(&TECHNIQUE),
        "should compress cargo output, got: {applied:?}"
    );
    let output = msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap();
    assert!(
        output.contains("test result:"),
        "should keep test result line: {output}"
    );
    assert!(output.contains("[#log_compressed:"));
    assert!(output.len() < content.len());
}

#[test]
fn test_compress_skips_short_logs() {
    let lines: Vec<String> = (0..10).map(|i| format!("test {i} PASSED")).collect();
    let content = lines.join("\n");
    let mut msgs = vec![msg("tool", &content)];
    let applied = compress_logs(&mut msgs);
    assert!(applied.is_empty(), "should skip short logs");
    assert_eq!(
        msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap(),
        content,
        "content should be unchanged"
    );
}

#[test]
fn test_compress_skips_non_logs() {
    let lines: Vec<String> = (0..30)
        .map(|i| {
            format!("This is line {i} of the prose. The quick brown fox jumps over the lazy dog.")
        })
        .collect();
    let content = lines.join("\n");
    let mut msgs = vec![msg("tool", &content)];
    let applied = compress_logs(&mut msgs);
    assert!(applied.is_empty(), "should skip non-log content");
    assert_eq!(
        msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap(),
        content,
        "content should be unchanged"
    );
}

#[test]
fn test_compress_dedups_warnings() {
    let mut lines: Vec<String> = Vec::new();
    for _ in 0..20 {
        lines.push("warning: unused variable: x".to_string());
    }
    for i in 0..10 {
        lines.push(format!("test test_{i} ... ok"));
    }
    assert_eq!(lines.len(), 30);
    let content = lines.join("\n");
    let mut msgs = vec![msg("tool", &content)];
    let applied = compress_logs(&mut msgs);
    assert!(
        applied.contains(&TECHNIQUE),
        "should compress, got: {applied:?}"
    );
    let output = msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap();
    let count = output.matches("warning: unused variable: x").count();
    assert_eq!(
        count, 1,
        "should keep only 1 of 20 identical warnings, got {count}: {output}"
    );
    assert!(output.contains("[#log_compressed:"));
}

#[test]
fn test_compress_never_produces_larger_output() {
    let mut lines: Vec<String> = (0..29).map(|_| "Tests:".to_string()).collect();
    lines.push("x".to_string());
    assert_eq!(lines.len(), 30);
    let content = lines.join("\n");
    let mut msgs = vec![msg("tool", &content)];
    let applied = compress_logs(&mut msgs);
    assert!(
        applied.is_empty(),
        "should skip when compression would be larger, got: {applied:?}"
    );
    assert_eq!(
        msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap(),
        content,
        "content should be unchanged"
    );
}

#[test]
fn test_compress_keeps_stack_trace() {
    let mut lines: Vec<String> = Vec::new();
    lines.push(
        "========================= test session starts =========================".to_string(),
    );
    for i in 0..25 {
        lines.push(format!("test_module.py::test_{i} PASSED [ 50%]"));
    }
    lines.push("Traceback (most recent call last):".to_string());
    lines.push("  File \"test.py\", line 5, in <module>".to_string());
    lines.push("    foo()".to_string());
    lines.push("  File \"test.py\", line 3, in foo".to_string());
    lines.push("    raise ValueError(\"oops\")".to_string());
    lines.push("ValueError: oops".to_string());
    assert_eq!(lines.len(), 32);
    let content = lines.join("\n");
    let mut msgs = vec![msg("tool", &content)];
    let applied = compress_logs(&mut msgs);
    assert!(
        applied.contains(&TECHNIQUE),
        "should compress, got: {applied:?}"
    );
    let output = msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap();
    assert!(
        output.contains("Traceback"),
        "should keep Traceback line: {output}"
    );
    assert!(
        output.contains("File \"test.py\""),
        "should keep stack trace File lines: {output}"
    );
    assert!(
        output.contains("ValueError"),
        "should keep final error line: {output}"
    );
    assert!(output.contains("[#log_compressed:"));
    assert!(output.len() < content.len());
}

#[test]
fn test_detect_format_pytest() {
    let lines: Vec<&str> = vec!["===== test session starts ====="];
    assert_eq!(detect_format(&lines), Some(LogFormat::Pytest));
}

#[test]
fn test_detect_format_pytest_markers() {
    let lines: Vec<&str> = vec!["test_foo PASSED", "test_bar SKIPPED"];
    assert_eq!(detect_format(&lines), Some(LogFormat::Pytest));
}

#[test]
fn test_detect_format_npm_jest() {
    let lines: Vec<&str> = vec!["PASS  src/foo.test.js", "Tests:       4 passed, 1 failed"];
    assert_eq!(detect_format(&lines), Some(LogFormat::NpmJest));
}

#[test]
fn test_detect_format_cargo() {
    let lines: Vec<&str> = vec!["running 5 tests", "test result: ok. 5 passed"];
    assert_eq!(detect_format(&lines), Some(LogFormat::Cargo));
}

#[test]
fn test_detect_format_make() {
    let lines: Vec<&str> = vec!["make[1]: Entering directory '/foo'"];
    assert_eq!(detect_format(&lines), Some(LogFormat::Make));
}

#[test]
fn test_detect_format_generic() {
    let lines: Vec<&str> = vec![
        "error: something",
        "fail: another",
        "warn: third",
        "panic: fourth",
        "exception: fifth",
    ];
    assert_eq!(detect_format(&lines), Some(LogFormat::Generic));
}

#[test]
fn test_detect_format_none() {
    let lines: Vec<&str> = vec!["hello world", "foo bar"];
    assert_eq!(detect_format(&lines), None);
}

#[test]
fn test_classify_line_error() {
    assert_eq!(classify_line("error: foo"), LineKind::Error);
    assert_eq!(classify_line("FAILED"), LineKind::Error);
    assert_eq!(
        classify_line("Traceback (most recent call last):"),
        LineKind::Error
    );
    assert_eq!(classify_line("panic: runtime error"), LineKind::Error);
    assert_eq!(classify_line("ValueError: oops"), LineKind::Error);
}

#[test]
fn test_classify_line_warning() {
    assert_eq!(classify_line("warning: unused variable"), LineKind::Warning);
    assert_eq!(classify_line("WARN something"), LineKind::Warning);
}

#[test]
fn test_classify_line_summary() {
    assert_eq!(
        classify_line("test result: ok. 5 passed;"),
        LineKind::Summary
    );
    assert_eq!(classify_line("Tests: 5 passed"), LineKind::Summary);
    assert_eq!(classify_line("95 passed in 5.0s"), LineKind::Summary);
}

#[test]
fn test_classify_line_header() {
    assert_eq!(classify_line("===== test session ====="), LineKind::Header);
    assert_eq!(classify_line("----- section -----"), LineKind::Header);
    assert_eq!(classify_line("###### section ######"), LineKind::Header);
    assert_eq!(classify_line("Running tests"), LineKind::Header);
    assert_eq!(classify_line("Compiling foo v1.0"), LineKind::Header);
}

#[test]
fn test_classify_line_stack_trace() {
    assert_eq!(classify_line("  at foo (bar.js:1:2)"), LineKind::StackTrace);
    assert_eq!(
        classify_line("  File \"test.py\", line 5"),
        LineKind::StackTrace
    );
    assert_eq!(classify_line("  frame #0: 0x0001"), LineKind::StackTrace);
    assert_eq!(classify_line("  #0 0x0001 in foo()"), LineKind::StackTrace);
}

#[test]
fn test_classify_line_other() {
    assert_eq!(classify_line("test foo ... ok"), LineKind::Other);
    assert_eq!(classify_line("hello world"), LineKind::Other);
    assert_eq!(classify_line(""), LineKind::Other);
}

#[test]
fn test_dedup_key_normalizes_digits() {
    let k1 = dedup_key("warning: at line 12");
    let k2 = dedup_key("warning: at line 99");
    assert_eq!(k1, k2);
    assert!(k1.contains('*'));
}

#[test]
fn test_dedup_key_normalizes_paths() {
    let k1 = dedup_key("warning: in /home/user/foo.rs");
    let k2 = dedup_key("warning: in /var/bar/baz.rs");
    assert_eq!(k1, k2);
    assert!(k1.contains('*'));
}

#[test]
fn test_dedup_key_normalizes_hex() {
    let k1 = dedup_key("warning: address 0x1234");
    let k2 = dedup_key("warning: address 0xabcd");
    assert_eq!(k1, k2);
    assert!(k1.contains('*'));
}

#[test]
fn test_dedup_key_different_prefixes_differ() {
    let k1 = dedup_key("warning: foo");
    let k2 = dedup_key("error: foo");
    assert_ne!(k1, k2);
}

#[test]
fn test_compress_only_tool_and_assistant() {
    let mut lines: Vec<String> = Vec::new();
    for _ in 0..30 {
        lines.push("warning: unused variable: x".to_string());
    }
    let content = lines.join("\n");
    let mut msgs = vec![msg("system", &content), msg("user", &content)];
    let applied = compress_logs(&mut msgs);
    assert!(applied.is_empty(), "should not touch system/user messages");
    for m in &msgs {
        assert_eq!(
            m.content.as_ref().and_then(|c| c.as_str()).unwrap(),
            content
        );
    }
}
