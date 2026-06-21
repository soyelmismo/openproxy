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
    pub priority_patterns: Vec<regex::Regex>,
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
    // split('\n') on `"a\nb\nc\n"` yields `["a", "b", "c", ""]` — i.e.
    // (newline_count + 1) elements. Reproduce that count via memchr so we
    // can take the early-return path without allocating a Vec<&str>.
    let line_count = memchr::memchr_iter(b'\n', text.as_bytes()).count() + 1;
    if line_count <= config.max_lines {
        return (text.to_string(), false, 0);
    }

    // Slow path — materialize the lines so we can slice head/tail/middle.
    let lines: Vec<&str> = text.split('\n').collect();
    let head_end = config.head_lines.min(lines.len());
    let head = &lines[..head_end];
    let tail_start = lines.len().saturating_sub(config.tail_lines);
    let tail = &lines[tail_start..];

    // Middle = lines strictly between head and tail (may be empty if the
    // head and tail slices overlap or touch).
    let middle_start = head_end.min(tail_start);
    let middle = &lines[middle_start..tail_start];

    let dropped = lines.len() - head.len() - tail.len();

    // Build the result directly into one String. Pre-allocate roughly the
    // size of the kept sections (head + tail) plus the marker and the
    // expected priority lines (heuristic: middle is at most as large as
    // head + tail in the common case).
    let est_capacity = text.len() / 2;
    let mut out = String::with_capacity(est_capacity);

    // Head
    let mut first = true;
    for l in head {
        if !first {
            out.push('\n');
        }
        out.push_str(l);
        first = false;
    }

    // Marker (always present when truncating).
    if !first {
        out.push('\n');
    }
    out.push_str("[rtk:truncated ");
    out.push_str(&dropped.to_string());
    out.push_str(" lines]");

    // Priority lines from the middle (skip any that already appear in head
    // or tail — preserves original dedup behavior).
    for l in middle {
        if config.priority_patterns.iter().any(|r| r.is_match(l)) {
            let already = head.iter().any(|h| *h == *l) || tail.iter().any(|t| *t == *l);
            if !already {
                out.push('\n');
                out.push_str(l);
            }
        }
    }

    // Tail
    for l in tail {
        out.push('\n');
        out.push_str(l);
    }

    (out, true, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: compile a single priority pattern for tests (mirrors the
    /// production default `TruncateConfig::default().priority_patterns`).
    fn default_priority() -> Vec<regex::Regex> {
        vec![regex::Regex::new(r"(?i)(error|failed|exception|traceback|FAIL|panic|✖|✗)").unwrap()]
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
        let text: Vec<String> = (0..100).map(|i| format!("line {}", i)).collect();
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
