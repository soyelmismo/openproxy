//! LogCompressor: format-aware build/test log compressor.
//!
//! Inspired by headroom's LogCompressor. Detects common build/test log
//! formats (pytest, npm/jest, cargo, make, generic) and compresses them
//! by keeping errors, warnings, stack traces, summary lines, and section
//! headers, while dropping the bulk of passed-test noise.
//!
//! ## Safety
//! - Only operates on `role == "tool"` and `role == "assistant"` messages.
//! - Only operates on content with ≥ `MIN_LOG_LINES` lines.
//! - Only operates when a known log format is detected.
//! - Only applies the compressed output when it is strictly smaller than
//!   the original (never produces a larger message).
//! - Skips content with no scoreable lines (no errors/warnings/summaries/
//!   headers/stack traces).

use openproxy_types::OpenAIMessage;
use serde_json::Value;
use std::fmt::Write;

type Messages = Vec<OpenAIMessage>;

const MIN_LOG_LINES: usize = 30;
const MAX_ERRORS: usize = 10;
const ERROR_CONTEXT_LINES: usize = 3;
const MAX_STACK_TRACES: usize = 3;
const STACK_TRACE_MAX_LINES: usize = 20;
const MAX_WARNINGS: usize = 5;
const MAX_TOTAL_LINES: usize = 100;

/// Technique name returned when compression applies.
pub const TECHNIQUE: &str = "lite::log_compressor";

/// Detected build/test log format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogFormat {
    Pytest,
    NpmJest,
    Cargo,
    Make,
    Generic,
}

/// Line classification used internally for scoring/selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
enum LineKind {
    Other = 0,
    Error = 1,
    Warning = 2,
    Summary = 3,
    Header = 4,
    StackTrace = 5,
}

impl LineKind {
    fn score(self) -> f32 {
        const SCORES: [f32; 6] = [0.0, 1.0, 0.7, 0.8, 0.5, 0.9];
        SCORES[self as usize]
    }
}

fn compress_single_msg(msg: &mut OpenAIMessage) -> Option<&'static str> {
    if msg.role != "tool" && msg.role != "assistant" {
        return None;
    }
    let text = msg.content.as_ref()?.as_str()?;
    let compressed = compress_log_content(text)?;
    if compressed.len() < text.len() {
        msg.content = Some(Value::String(compressed));
        Some(TECHNIQUE)
    } else {
        None
    }
}

/// Compresses build/test log output in tool results and assistant messages.
///
/// Operates on content that looks like build/test output (≥30 lines with
/// test-related patterns). Returns the technique name (`"lite::log_compressor"`)
/// once per message that was actually compressed.
pub fn compress_logs(msgs: &mut Messages) -> Vec<&'static str> {
    msgs.iter_mut().filter_map(compress_single_msg).collect()
}

/// Compress a single log content string. Returns `Some((compressed, technique))`
/// if compression applied, or `None` otherwise.
///
/// This is the per-string entry point that powers the content router. It
/// delegates to the private `compress_log_content` (which enforces the
/// `MIN_LOG_LINES` floor, the log-format detection, and the scoreable-lines
/// check) and applies the same "strictly smaller than input" guard that
/// [`compress_logs`] uses on the messages-vec path.
pub fn compress_log_string(text: &str) -> Option<(String, &'static str)> {
    compress_log_content(text)
        .filter(|c| c.len() < text.len())
        .map(|c| (c, TECHNIQUE))
}

fn validate_and_score(lines: &[&str]) -> Option<Vec<LineKind>> {
    if lines.len() < MIN_LOG_LINES {
        return None;
    }
    let _format = detect_format(lines)?;
    let mut kinds = Vec::with_capacity(lines.len());
    let mut has_scoreable = false;
    for &l in lines {
        let kind = classify_line(l);
        if kind.score() > 0.0 {
            has_scoreable = true;
        }
        kinds.push(kind);
    }
    if has_scoreable {
        Some(kinds)
    } else {
        None
    }
}

fn format_selected_lines(lines: &[&str], selected: &[usize]) -> String {
    let total = lines.len();
    let kept = selected.len();
    let estimated_cap = selected
        .iter()
        .filter_map(|&idx| lines.get(idx))
        .map(|l| l.len() + 1)
        .sum::<usize>()
        + 64;
    let mut out = String::with_capacity(estimated_cap);
    if kept < total {
        let _ = writeln!(out, "[#log_compressed: kept {kept} of {total} lines]");
    }
    let mut first = true;
    for &idx in selected {
        if let Some(line) = lines.get(idx) {
            if !first {
                out.push('\n');
            }
            out.push_str(line);
            first = false;
        }
    }
    out
}

/// Compress a single content string. Returns `None` if not compressible
/// (too short, no log format, no scoreable lines, or no lines selected).
fn compress_log_content(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let kinds = validate_and_score(&lines)?;
    let selected = select_lines(&lines, &kinds);
    if selected.is_empty() {
        return None;
    }
    Some(format_selected_lines(&lines, &selected))
}

fn is_pytest_log(head: &[&str]) -> bool {
    head.iter().any(|l| {
        l.contains("===== test session starts =====")
            || l.contains("PASSED")
            || l.contains("FAILED")
            || l.contains("SKIPPED")
            || l.contains("ERROR")
    })
}

fn is_npm_jest_log(head: &[&str]) -> bool {
    head.iter().any(|l| {
        l.starts_with("PASS ")
            || l.starts_with("FAIL ")
            || l.contains("Test Suites:")
            || l.contains("Tests:")
    })
}

fn is_cargo_log(head: &[&str]) -> bool {
    head.iter().any(|l| {
        (l.starts_with("running ") && l.contains(" test"))
            || l.contains("test result:")
            || l.starts_with("Compiling")
            || l.starts_with("Finished")
    })
}

fn is_make_target_header(l: &str) -> bool {
    if let Some(end) = l.find("]:")
        && let Some(n) = l.strip_prefix("make[")
    {
        let end_idx = end.saturating_sub("make[".len());
        if !n.is_char_boundary(end_idx) {
            return false;
        }
        let target = &n[..end_idx];
        !target.is_empty() && target.chars().all(|c| c.is_ascii_digit())
    } else {
        false
    }
}

fn is_make_log(head: &[&str]) -> bool {
    head.iter().any(|l| {
        is_make_target_header(l)
            || l.contains("Entering directory")
            || l.contains("Leaving directory")
    })
}

fn is_generic_log(head: &[&str]) -> bool {
    const GENERIC_TOKENS: &[&str] = &["error", "fail", "warn", "traceback", "panic", "exception"];
    head.iter()
        .filter(|l| {
            GENERIC_TOKENS
                .iter()
                .any(|t| contains_case_insensitive_ascii(l, t))
        })
        .count()
        >= 5
}

/// Detect log format from the first 50 lines.
fn detect_format(lines: &[&str]) -> Option<LogFormat> {
    let head = &lines[..lines.len().min(50)];
    if is_pytest_log(head) {
        Some(LogFormat::Pytest)
    } else if is_npm_jest_log(head) {
        Some(LogFormat::NpmJest)
    } else if is_cargo_log(head) {
        Some(LogFormat::Cargo)
    } else if is_make_log(head) {
        Some(LogFormat::Make)
    } else if is_generic_log(head) {
        Some(LogFormat::Generic)
    } else {
        None
    }
}

fn is_stack_trace_line(line: &str) -> bool {
    let trimmed = line.trim_start_matches([' ', '\t']);
    if trimmed.len() >= line.len() {
        return false;
    }
    if trimmed.starts_with("at ")
        || trimmed.starts_with("File \"")
        || trimmed.starts_with("frame #")
    {
        return true;
    }
    trimmed
        .strip_prefix('#')
        .is_some_and(|rest| rest.chars().next().is_some_and(|c| c.is_ascii_digit()))
}

fn is_summary_line(line: &str) -> bool {
    const SUMMARY_MARKERS: &[&str] =
        &["test result:", "Test Suites:", "Tests:", "passed", "failed"];
    SUMMARY_MARKERS.iter().any(|m| line.contains(m))
}

fn is_header_line(line: &str) -> bool {
    line.contains("=====")
        || line.contains("-----")
        || line.contains("######")
        || line.starts_with("Running")
        || line.starts_with("Compiling")
}

/// Classify a single line into a `LineKind`.
fn classify_line(line: &str) -> LineKind {
    if is_stack_trace_line(line) {
        LineKind::StackTrace
    } else if contains_error_token(line) {
        LineKind::Error
    } else if contains_case_insensitive_ascii(line, "warn") {
        LineKind::Warning
    } else if is_summary_line(line) {
        LineKind::Summary
    } else if is_header_line(line) {
        LineKind::Header
    } else {
        LineKind::Other
    }
}

fn contains_case_insensitive_ascii(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Check if a line contains an error token (case-insensitive substring match).
fn contains_error_token(line: &str) -> bool {
    const ERROR_TOKENS: &[&str] = &["error", "fatal", "panic", "exception", "traceback", "fail"];
    ERROR_TOKENS
        .iter()
        .any(|t| contains_case_insensitive_ascii(line, t))
}

fn collect_error_indices(kinds: &[LineKind], selected: &mut Vec<usize>) {
    let n = kinds.len();
    let mut error_count = 0;
    for (i, kind) in kinds.iter().enumerate() {
        if *kind == LineKind::Error {
            if error_count >= MAX_ERRORS {
                continue;
            }
            error_count += 1;
            selected.push(i);
            for j in 1..=ERROR_CONTEXT_LINES {
                if i + j < n {
                    selected.push(i + j);
                }
            }
        }
    }
}

fn collect_stack_trace_indices(kinds: &[LineKind], selected: &mut Vec<usize>) {
    let n = kinds.len();
    let mut traces_collected = 0;
    let mut i = 0;
    while i < n {
        if kinds[i] == LineKind::StackTrace {
            let mut end = i;
            while end < n && kinds[end] == LineKind::StackTrace {
                end += 1;
            }
            if traces_collected < MAX_STACK_TRACES {
                let take = (end - i).min(STACK_TRACE_MAX_LINES);
                for j in i..i + take {
                    selected.push(j);
                }
                traces_collected += 1;
            }
            i = end;
        } else {
            i += 1;
        }
    }
}

fn collect_warning_indices(lines: &[&str], kinds: &[LineKind], selected: &mut Vec<usize>) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut warning_seen: Vec<u64> = Vec::with_capacity(MAX_WARNINGS);
    for (i, (kind, line)) in kinds.iter().zip(lines.iter()).enumerate() {
        if *kind == LineKind::Warning && warning_seen.len() < MAX_WARNINGS {
            let key_str = dedup_key(line);
            let mut hasher = DefaultHasher::new();
            key_str.hash(&mut hasher);
            let key = hasher.finish();
            if !warning_seen.contains(&key) {
                selected.push(i);
                warning_seen.push(key);
            }
        }
    }
}

fn collect_summary_and_header_indices(kinds: &[LineKind], selected: &mut Vec<usize>) {
    for (i, kind) in kinds.iter().enumerate() {
        if *kind == LineKind::Summary || *kind == LineKind::Header {
            selected.push(i);
        }
    }
}

/// Select line indices to keep, applying the selection algorithm:
/// 1. Errors (+ context lines after each), capped at MAX_ERRORS.
/// 2. Stack trace runs, each capped at STACK_TRACE_MAX_LINES, at most
///    MAX_STACK_TRACES runs.
/// 3. Top MAX_WARNINGS warnings, deduped by normalized message prefix.
/// 4. All Summary + Header lines.
/// 5. Sort, dedup, truncate to MAX_TOTAL_LINES.
fn select_lines(lines: &[&str], kinds: &[LineKind]) -> Vec<usize> {
    let mut selected: Vec<usize> = Vec::with_capacity(MAX_TOTAL_LINES);
    collect_error_indices(kinds, &mut selected);
    collect_stack_trace_indices(kinds, &mut selected);
    collect_warning_indices(lines, kinds, &mut selected);
    collect_summary_and_header_indices(kinds, &mut selected);

    selected.sort_unstable();
    selected.dedup();
    selected.truncate(MAX_TOTAL_LINES);
    selected
}

/// Compute the dedup key for a warning line.
///
/// Splits on the first `:` or `=`, then normalizes the trailing region
/// (everything after the separator) by replacing digit runs, hex literals
/// (`0x...`), and filesystem paths (`/...`) with `*`. The prefix (up to and
/// including the separator) is kept verbatim. If there's no separator, the
/// entire line is normalized.
///
/// This collapses warnings that differ only in numeric/path/hex details
/// (e.g. `warning: unused variable at line 12` and
/// `warning: unused variable at line 99`) into a single dedup bucket.
fn dedup_key(line: &str) -> String {
    if let Some((byte_idx, ch)) = line.char_indices().find(|&(_, c)| c == ':' || c == '=') {
        let sep_end = byte_idx + ch.len_utf8();
        let (prefix, rest) = line.split_at(sep_end);
        let mut out = String::with_capacity(prefix.len() + rest.len());
        out.push_str(prefix);
        out.push_str(&normalize_trailing(rest));
        out
    } else {
        normalize_trailing(line)
    }
}

fn scan_hex_run(bytes: &[u8], i: usize) -> Option<usize> {
    if bytes.get(i) == Some(&b'0') && bytes.get(i + 1) == Some(&b'x') {
        let mut j = i + 2;
        while j < bytes.len() && bytes[j].is_ascii_hexdigit() {
            j += 1;
        }
        if j > i + 2 {
            return Some(j);
        }
    }
    None
}

fn scan_digit_run(bytes: &[u8], i: usize) -> Option<usize> {
    if bytes.get(i).is_some_and(|b| b.is_ascii_digit()) {
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        return Some(j);
    }
    None
}

fn scan_path_run(bytes: &[u8], i: usize) -> Option<usize> {
    if bytes.get(i) == Some(&b'/') {
        let mut j = i;
        while j < bytes.len() {
            let b = bytes[j];
            if b.is_ascii_alphanumeric() || b == b'/' || b == b'.' || b == b'_' || b == b'-' {
                j += 1;
            } else {
                break;
            }
        }
        if j > i + 1 {
            return Some(j);
        }
    }
    None
}

/// Normalize the trailing region of a dedup key: replace digit runs, hex
/// literals, and filesystem paths with `*`. Operates on bytes/char boundaries
/// so it's UTF-8 safe and zero-alloc.
fn normalize_trailing(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(next_i) = scan_hex_run(bytes, i)
            .or_else(|| scan_digit_run(bytes, i))
            .or_else(|| scan_path_run(bytes, i))
        {
            out.push('*');
            i = next_i;
        } else {
            let b = bytes[i];
            if b.is_ascii() {
                out.push(b as char);
                i += 1;
            } else if let Some(ch) = s[i..].chars().next() {
                out.push(ch);
                i += ch.len_utf8();
            } else {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
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
        // 100-line pytest output: 1 header + 95 PASSED + 3 FAILED + 1 summary.
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
        // 80-line cargo test output: 1 running + 78 ok + 1 test result.
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
        // 10-line output: too short (< MIN_LOG_LINES=30).
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
        // 30+ lines of plain prose: no log format detected.
        let lines: Vec<String> = (0..30)
            .map(|i| {
                format!(
                    "This is line {i} of the prose. The quick brown fox jumps over the lazy dog."
                )
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
        // 30 lines: 20 identical warnings + 10 ok lines.
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
        // 29 short Summary lines ("Tests:", 6 bytes) + 1 Other line ("x", 1 byte).
        // Compression would keep 29 lines and add a ~40-byte header — the
        // header overhead exceeds the savings from dropping 1 line, so the
        // compressor must skip and leave the message untouched.
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
        // 32-line output: 1 header + 25 PASSED + Python traceback (6 lines).
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

    // ─── Unit tests for helpers ────────────────────────────────────────────

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
        // Note: lines containing "fail"/"error" substrings classify as Error
        // first (e.g. "1 failed" → Error via "fail"), so we use clean inputs.
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
        // Header markers require 5+ chars (-----/######/=====).
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
        // Two warnings differing only in a digit collapse to the same key.
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
        // system/user messages are skipped.
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
}
