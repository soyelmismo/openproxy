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
use std::collections::{BTreeSet, HashSet};

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
enum LineKind {
    Other,
    Error,
    Warning,
    Summary,
    Header,
    StackTrace,
}

impl LineKind {
    fn score(self) -> f32 {
        match self {
            LineKind::Error => 1.0,
            LineKind::StackTrace => 0.9,
            LineKind::Summary => 0.8,
            LineKind::Warning => 0.7,
            LineKind::Header => 0.5,
            LineKind::Other => 0.0,
        }
    }
}

/// Compresses build/test log output in tool results and assistant messages.
///
/// Operates on content that looks like build/test output (≥30 lines with
/// test-related patterns). Returns the technique name (`"lite::log_compressor"`)
/// once per message that was actually compressed.
pub fn compress_logs(msgs: &mut Messages) -> Vec<&'static str> {
    let mut applied = Vec::new();
    for msg in msgs.iter_mut() {
        // Only tool results and assistant messages can contain build output.
        if msg.role != "tool" && msg.role != "assistant" {
            continue;
        }
        // Take ownership of the text so we can rebind `msg.content` afterwards
        // without a dangling borrow.
        let text = match msg.content.as_ref().and_then(|c| c.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if let Some(compressed) = compress_log_content(&text)
            && compressed.len() < text.len()
        {
            msg.content = Some(Value::String(compressed));
            applied.push(TECHNIQUE);
        }
    }
    applied
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

/// Compress a single content string. Returns `None` if not compressible
/// (too short, no log format, no scoreable lines, or no lines selected).
fn compress_log_content(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() < MIN_LOG_LINES {
        return None;
    }
    // Must look like a build/test log.
    let _format = detect_format(&lines)?;
    // Score each line.
    let kinds: Vec<LineKind> = lines.iter().map(|l| classify_line(l)).collect();
    if !kinds.iter().any(|k| k.score() > 0.0) {
        return None;
    }
    let selected = select_lines(&lines, &kinds);
    if selected.is_empty() {
        return None;
    }
    let total = lines.len();
    let kept = selected.len();
    let mut out = String::with_capacity(text.len() / 2);
    if kept < total {
        out.push_str(&format!(
            "[#log_compressed: kept {} of {} lines]\n",
            kept, total
        ));
    }
    let mut first = true;
    for &idx in &selected {
        if !first {
            out.push('\n');
        }
        out.push_str(lines[idx]);
        first = false;
    }
    Some(out)
}

/// Detect log format from the first 50 lines.
fn detect_format(lines: &[&str]) -> Option<LogFormat> {
    let head: Vec<&str> = lines.iter().take(50).copied().collect();

    // Pytest
    for l in &head {
        if l.contains("===== test session starts =====") {
            return Some(LogFormat::Pytest);
        }
    }
    let pytest_markers = head
        .iter()
        .filter(|l| {
            l.contains("PASSED")
                || l.contains("FAILED")
                || l.contains("SKIPPED")
                || l.contains("ERROR")
        })
        .count();
    if pytest_markers >= 1 {
        return Some(LogFormat::Pytest);
    }

    // Npm/Jest
    for l in &head {
        if l.starts_with("PASS ")
            || l.starts_with("FAIL ")
            || l.contains("Test Suites:")
            || l.contains("Tests:")
        {
            return Some(LogFormat::NpmJest);
        }
    }

    // Cargo
    for l in &head {
        if l.starts_with("running ") && l.contains(" test") {
            return Some(LogFormat::Cargo);
        }
        if l.contains("test result:") {
            return Some(LogFormat::Cargo);
        }
        if l.starts_with("Compiling") || l.starts_with("Finished") {
            return Some(LogFormat::Cargo);
        }
    }

    // Make
    for l in &head {
        if l.starts_with("make[") {
            // Validate make[N]: structure (N is digits).
            if let Some(end) = l.find("]:") {
                let n = &l["make[".len()..end];
                if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
                    return Some(LogFormat::Make);
                }
            }
        }
        if l.contains("Entering directory") || l.contains("Leaving directory") {
            return Some(LogFormat::Make);
        }
    }

    // Generic: ≥5 lines match error/fail/warn/traceback/panic/exception.
    let generic_matches = head
        .iter()
        .filter(|l| {
            let low = l.to_lowercase();
            low.contains("error")
                || low.contains("fail")
                || low.contains("warn")
                || low.contains("traceback")
                || low.contains("panic")
                || low.contains("exception")
        })
        .count();
    if generic_matches >= 5 {
        return Some(LogFormat::Generic);
    }

    None
}

/// Classify a single line into a `LineKind`.
fn classify_line(line: &str) -> LineKind {
    // StackTrace: indented and starts with one of the patterns
    // (at ..., File "...", frame #N, #NN).
    let trimmed_start = line.trim_start_matches([' ', '\t']);
    let is_indented = trimmed_start.len() < line.len();
    if is_indented {
        if trimmed_start.starts_with("at ")
            || trimmed_start.starts_with("File \"")
            || trimmed_start.starts_with("frame #")
        {
            return LineKind::StackTrace;
        }
        // #NN pattern: '#' followed by at least one digit.
        if let Some(rest) = trimmed_start.strip_prefix('#')
            && rest
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
        {
            return LineKind::StackTrace;
        }
    }

    let low = line.to_lowercase();

    // Error: contains an error token (case-insensitive substring).
    // Substring match is intentional — "FAILED", "ValueError", "runtime error"
    // all carry error semantics for log compression purposes.
    if contains_error_token(&low) {
        return LineKind::Error;
    }

    // Warning: "warn" (case-insensitive).
    if low.contains("warn") {
        return LineKind::Warning;
    }

    // Summary: test result markers or lowercase passed/failed counts.
    // Note: "passed"/"failed" are matched case-sensitively (lowercase) so
    // that pytest uppercase PASSED/FAILED test-status lines do not get
    // misclassified as summaries — they're classified as Errors above
    // via the "fail" substring (or remain Other if they're PASSED lines).
    if line.contains("test result:")
        || line.contains("Test Suites:")
        || line.contains("Tests:")
        || line.contains("passed")
        || line.contains("failed")
    {
        return LineKind::Summary;
    }

    // Header: section markers or compilation/runner banners.
    if line.contains("=====")
        || line.contains("-----")
        || line.contains("######")
        || line.starts_with("Running")
        || line.starts_with("Compiling")
    {
        return LineKind::Header;
    }

    LineKind::Other
}

/// Check if a lowercased line contains an error token (substring match).
fn contains_error_token(low: &str) -> bool {
    low.contains("error")
        || low.contains("fatal")
        || low.contains("panic")
        || low.contains("exception")
        || low.contains("traceback")
        || low.contains("fail")
}

/// Select line indices to keep, applying the selection algorithm:
/// 1. Errors (+ context lines after each), capped at MAX_ERRORS.
/// 2. Stack trace runs, each capped at STACK_TRACE_MAX_LINES, at most
///    MAX_STACK_TRACES runs.
/// 3. Top MAX_WARNINGS warnings, deduped by normalized message prefix.
/// 4. All Summary + Header lines.
/// 5. Sort, dedup, truncate to MAX_TOTAL_LINES.
fn select_lines(lines: &[&str], kinds: &[LineKind]) -> Vec<usize> {
    let n = kinds.len();
    let mut selected: BTreeSet<usize> = BTreeSet::new();

    // 1. Errors + context.
    let mut error_count = 0;
    for i in 0..n {
        if kinds[i] == LineKind::Error {
            if error_count >= MAX_ERRORS {
                continue;
            }
            error_count += 1;
            selected.insert(i);
            for j in 1..=ERROR_CONTEXT_LINES {
                if i + j < n {
                    selected.insert(i + j);
                }
            }
        }
    }

    // 2. Stack traces (contiguous runs).
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
                    selected.insert(j);
                }
                traces_collected += 1;
            }
            i = end;
        } else {
            i += 1;
        }
    }

    // 3. Warnings (top MAX_WARNINGS, deduped).
    let mut warning_seen: HashSet<String> = HashSet::new();
    let mut warnings_kept = 0;
    for i in 0..n {
        if kinds[i] == LineKind::Warning && warnings_kept < MAX_WARNINGS {
            let key = dedup_key(lines[i]);
            if warning_seen.insert(key) {
                selected.insert(i);
                warnings_kept += 1;
            }
        }
    }

    // 4. All Summary + Header lines.
    for i in 0..n {
        if kinds[i] == LineKind::Summary || kinds[i] == LineKind::Header {
            selected.insert(i);
        }
    }

    // 5. Sort, dedup, truncate.
    let mut result: Vec<usize> = selected.into_iter().collect();
    result.truncate(MAX_TOTAL_LINES);
    result
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
    let sep_char_idx = line.chars().position(|c| c == ':' || c == '=');
    match sep_char_idx {
        Some(idx) => {
            // Convert char index to byte index.
            let byte_idx = line
                .char_indices()
                .nth(idx)
                .map(|(b, _)| b)
                .unwrap_or(line.len());
            // Include the separator in the prefix.
            let sep_end = (byte_idx + 1).min(line.len());
            let prefix = &line[..sep_end];
            let rest = if sep_end <= line.len() {
                &line[sep_end..]
            } else {
                ""
            };
            let mut out = String::with_capacity(prefix.len() + rest.len());
            out.push_str(prefix);
            out.push_str(&normalize_trailing(rest));
            out
        }
        None => normalize_trailing(line),
    }
}

/// Normalize the trailing region of a dedup key: replace digit runs, hex
/// literals, and filesystem paths with `*`. Operates on `char`s so it's
/// UTF-8 safe.
fn normalize_trailing(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // Hex literal: 0x[0-9a-fA-F]+
        if c == '0' && i + 1 < chars.len() && chars[i + 1] == 'x' {
            let mut j = i + 2;
            while j < chars.len() && chars[j].is_ascii_hexdigit() {
                j += 1;
            }
            if j > i + 2 {
                out.push('*');
                i = j;
                continue;
            }
        }
        // Digit run.
        if c.is_ascii_digit() {
            let mut j = i;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            out.push('*');
            i = j;
            continue;
        }
        // Path: '/' followed by one or more path chars (word char, /, ., -, _).
        if c == '/' {
            let mut j = i;
            while j < chars.len() {
                let cc = chars[j];
                if cc.is_ascii_alphanumeric() || cc == '/' || cc == '.' || cc == '_' || cc == '-' {
                    j += 1;
                } else {
                    break;
                }
            }
            if j > i + 1 {
                out.push('*');
                i = j;
                continue;
            }
        }
        // Regular char.
        out.push(c);
        i += 1;
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
            extra: Default::default(),
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
            lines.push(format!("test_module.py::test_pass_{} PASSED [ 50%]", i));
        }
        for i in 0..3 {
            lines.push(format!("test_module.py::test_fail_{} FAILED [ 50%]", i));
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
            "should compress pytest output, got: {:?}",
            applied
        );
        let output = msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap();
        assert!(
            output.contains("FAILED"),
            "should keep FAILED lines: {}",
            output
        );
        assert!(
            output.contains("passed"),
            "should keep summary line: {}",
            output
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
            lines.push(format!("test test_{} ... ok", i));
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
            "should compress cargo output, got: {:?}",
            applied
        );
        let output = msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap();
        assert!(
            output.contains("test result:"),
            "should keep test result line: {}",
            output
        );
        assert!(output.contains("[#log_compressed:"));
        assert!(output.len() < content.len());
    }

    #[test]
    fn test_compress_skips_short_logs() {
        // 10-line output: too short (< MIN_LOG_LINES=30).
        let lines: Vec<String> = (0..10).map(|i| format!("test {} PASSED", i)).collect();
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
                    "This is line {} of the prose. The quick brown fox jumps over the lazy dog.",
                    i
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
            lines.push(format!("test test_{} ... ok", i));
        }
        assert_eq!(lines.len(), 30);
        let content = lines.join("\n");
        let mut msgs = vec![msg("tool", &content)];
        let applied = compress_logs(&mut msgs);
        assert!(
            applied.contains(&TECHNIQUE),
            "should compress, got: {:?}",
            applied
        );
        let output = msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap();
        let count = output.matches("warning: unused variable: x").count();
        assert_eq!(
            count, 1,
            "should keep only 1 of 20 identical warnings, got {}: {}",
            count, output
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
            "should skip when compression would be larger, got: {:?}",
            applied
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
            lines.push(format!("test_module.py::test_{} PASSED [ 50%]", i));
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
            "should compress, got: {:?}",
            applied
        );
        let output = msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap();
        assert!(
            output.contains("Traceback"),
            "should keep Traceback line: {}",
            output
        );
        assert!(
            output.contains("File \"test.py\""),
            "should keep stack trace File lines: {}",
            output
        );
        assert!(
            output.contains("ValueError"),
            "should keep final error line: {}",
            output
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
