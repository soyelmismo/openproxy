use std::fmt::Write;

/// Pre-compiled truncation configuration.
///
/// Built ONCE at startup (alongside its owning `CompiledFilter`) and shared
/// across all requests via `Arc<CompiledFilter>`. The `priority_patterns`
/// are stored as compiled `regex::Regex` values, eliminating the per-call
/// `Regex::new` loop that the old `TruncateConfig` performed.
pub struct CompiledTruncateConfig {
    pub max_lines: usize,
    pub head_lines: usize,
    pub tail_lines: usize,
    pub priority_patterns: Box<[regex::Regex]>,
}

/// Trunca texto manteniendo head + priority + tail, con marcador.
///
/// Receives a `&CompiledTruncateConfig` whose `priority_patterns` are
/// already-compiled regexes — no per-call `Regex::new` is performed.
///
/// The function:
///  - Counts newlines via `memchr::memchr_iter` (SIMD-accelerated) to take
///    the early-return fast path without materializing a `Vec<&str>`.
///  - Writes the result directly into a single `String` (no
///    `Vec<String>` + `join`).
///  - Preserves the exact `split('\n')` semantics of the original
///    implementation (a trailing newline yields a trailing empty element).
fn append_priority_middle(
    out: &mut String,
    middle: &[&str],
    head: &[&str],
    tail: &[&str],
    patterns: &[regex::Regex],
) {
    for l in middle {
        let is_priority = patterns.iter().any(|r| r.is_match(l));
        let already = head.contains(l) || tail.contains(l);
        if is_priority && !already {
            out.push('\n');
            out.push_str(l);
        }
    }
}

fn assemble_truncated_output(
    head: &[&str],
    middle: &[&str],
    tail: &[&str],
    dropped: usize,
    patterns: &[regex::Regex],
    est_capacity: usize,
) -> String {
    let mut out = String::with_capacity(est_capacity);
    let mut first = true;
    for l in head {
        if !first {
            out.push('\n');
        }
        out.push_str(l);
        first = false;
    }
    if !first {
        out.push('\n');
    }
    let _ = write!(out, "[rtk:truncated {dropped} lines]");
    append_priority_middle(&mut out, middle, head, tail, patterns);
    for l in tail {
        out.push('\n');
        out.push_str(l);
    }
    out
}

/// Trunca texto manteniendo head + priority + tail, con marcador.
///
/// Receives a `&CompiledTruncateConfig` whose `priority_patterns` are
/// already-compiled regexes — no per-call `Regex::new` is performed.
///
/// The function:
///  - Counts newlines via `memchr::memchr_iter` (SIMD-accelerated) to take
///    the early-return fast path without materializing a `Vec<&str>`.
///  - Writes the result directly into a single `String` (no
///    `Vec<String>` + `join`).
///  - Preserves the exact `split('\n')` semantics of the original
///    implementation (a trailing newline yields a trailing empty element).
pub fn smart_truncate(text: &str, config: &CompiledTruncateConfig) -> (String, bool, usize) {
    let line_count = memchr::memchr_iter(b'\n', text.as_bytes()).count() + 1;
    if line_count <= config.max_lines {
        return (text.to_string(), false, 0);
    }

    let lines: Vec<&str> = text.split('\n').collect();
    let head_end = config.head_lines.min(lines.len());
    let head = &lines[..head_end];
    let tail_start = lines.len().saturating_sub(config.tail_lines);
    let tail = &lines[tail_start..];

    let middle_start = head_end.min(tail_start);
    let middle = &lines[middle_start..tail_start];
    let dropped = lines.len().saturating_sub(head.len() + tail.len());

    let mut est = text.len() / 2;
    if !text.is_char_boundary(est) {
        est = text.floor_char_boundary(est);
    }
    let out =
        assemble_truncated_output(head, middle, tail, dropped, &config.priority_patterns, est);

    (out, true, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: compile a single priority pattern for tests (mirrors the
    /// production default `TruncateConfig::default().priority_patterns`).
    fn default_priority() -> Box<[regex::Regex]> {
        static RE: std::sync::LazyLock<Box<[regex::Regex]>> = std::sync::LazyLock::new(|| {
            vec![
                regex::Regex::new(r"(?i)(error|failed|exception|traceback|FAIL|panic|✖|✗)")
                    .unwrap(),
            ]
            .into_boxed_slice()
        });
        RE.clone()
    }

    #[test]
    fn test_no_truncation_when_under_limit() {
        let text = "a\nb\nc\n";
        let config = CompiledTruncateConfig {
            max_lines: 10,
            head_lines: 20,
            tail_lines: 20,
            priority_patterns: default_priority(),
        };
        let (result, truncated, _) = smart_truncate(text, &config);
        assert!(!truncated);
        assert_eq!(result, text);
    }

    #[test]
    fn test_truncation_preserves_head_tail() {
        let text: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        let text = text.join("\n");
        let config = CompiledTruncateConfig {
            max_lines: 20,
            head_lines: 5,
            tail_lines: 5,
            priority_patterns: default_priority(),
        };
        let (result, truncated, dropped) = smart_truncate(&text, &config);
        assert!(truncated);
        assert!(dropped > 0);
        assert!(result.starts_with("line 0\nline 1\n"));
        assert!(result.ends_with("line 98\nline 99"));
        assert!(result.contains("[rtk:truncated"));
    }
}
