use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Arc;

use super::smart_truncate::{smart_truncate, CompiledTruncateConfig};

// ─── Compiled filter structs ─────────────────────────────────────────────────

/// A filter with all patterns pre-compiled and rule names pre-computed.
///
/// Built ONCE at startup (in a `once_cell::sync::Lazy` static) and shared
/// across all requests via `Arc<CompiledFilter>`. This eliminates the
/// per-message `RtkFilter` reconstruction (≈15 `String` + ≈5 `Vec`
/// allocations) and the per-message `regex::Regex::new` calls (5–15 per
/// message) that the previous `&RtkFilter` API performed.
///
/// The `rule_*` fields are pre-computed `&'static str` (via `Box::leak`,
/// called ~100 times total at startup for ~3 KB of process-lifetime
/// leaked strings — acceptable for a one-shot filter cache).
pub struct CompiledFilter {
    pub id: &'static str,
    pub strip_ansi: bool,
    pub filter_stderr: bool,
    /// `(compiled_regex, replacement)` pairs. Replacement is a static
    /// literal for every builtin/generic filter.
    pub replace: Vec<(regex::Regex, &'static str)>,
    pub match_output: Vec<CompiledMatchOutputRule>,
    pub strip_patterns: Vec<regex::Regex>,
    pub keep_patterns: Vec<regex::Regex>,
    pub collapse_patterns: Vec<regex::Regex>,
    pub truncate_line_at: usize,
    pub on_empty: &'static str,
    pub truncate: Option<CompiledTruncateConfig>,
    // Pre-computed rule names — avoid `format!("{}::xxx", filter.id)` per call.
    pub rule_strip_ansi: &'static str,
    pub rule_filter_stderr: &'static str,
    pub rule_replace: &'static str,
    pub rule_match_output: &'static str,
    pub rule_strip: &'static str,
    pub rule_keep: &'static str,
    pub rule_collapse: &'static str,
    pub rule_truncate_line: &'static str,
    pub rule_truncate: &'static str,
    pub rule_on_empty: &'static str,
}

/// Short-circuit rule: if `re` matches and `unless` (if present) does not,
/// replace the entire output with `message`.
pub struct CompiledMatchOutputRule {
    pub re: regex::Regex,
    pub message: &'static str,
    pub unless: Option<regex::Regex>,
}

// ─── Construction helpers ────────────────────────────────────────────────────

/// Leak a `String` to `&'static str`. Called only at filter-construction
/// time (once per filter × ~10 rules ≈ ~100 small strings ≈ ~3 KB total
/// leaked). Acceptable for a process-lifetime cache.
fn leak_string(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Compile a regex pattern, panicking on error. Called only at
/// filter-construction time; all patterns are static literals, so a
/// compile error is a programmer bug that should fail startup loudly.
fn compile_re(pattern: &str) -> regex::Regex {
    regex::Regex::new(pattern)
        .unwrap_or_else(|e| panic!("invalid filter pattern {:?}: {}", pattern, e))
}

/// Pre-compute a rule name like `"git-status::strip_ansi"`.
fn rule_name(id: &'static str, suffix: &'static str) -> &'static str {
    leak_string(format!("{}::{}", id, suffix))
}

/// Build a `CompiledFilter` skeleton with all `rule_*` names pre-computed
/// from `id`. The caller fills in the actual filter logic fields.
macro_rules! filter_skeleton {
    ($id:expr, $strip_ansi:expr, $filter_stderr:expr, $replace:expr, $match_output:expr,
     $strip_patterns:expr, $keep_patterns:expr, $collapse_patterns:expr,
     $truncate_line_at:expr, $on_empty:expr, $truncate:expr) => {{
        let id: &'static str = $id;
        CompiledFilter {
            id,
            strip_ansi: $strip_ansi,
            filter_stderr: $filter_stderr,
            replace: $replace,
            match_output: $match_output,
            strip_patterns: $strip_patterns,
            keep_patterns: $keep_patterns,
            collapse_patterns: $collapse_patterns,
            truncate_line_at: $truncate_line_at,
            on_empty: $on_empty,
            truncate: $truncate,
            rule_strip_ansi: rule_name(id, "strip_ansi"),
            rule_filter_stderr: rule_name(id, "filter_stderr"),
            rule_replace: rule_name(id, "replace"),
            rule_match_output: rule_name(id, "match_output"),
            rule_strip: rule_name(id, "strip"),
            rule_keep: rule_name(id, "keep"),
            rule_collapse: rule_name(id, "collapse"),
            rule_truncate_line: rule_name(id, "truncate_line"),
            rule_truncate: rule_name(id, "truncate"),
            rule_on_empty: rule_name(id, "on_empty"),
        }
    }};
}

// ─── Static STDERR regex (compiled once) ─────────────────────────────────────
//
// `filter_stderr_prefixes` was previously compiling this regex on every
// call. Phase B already moved `strip_ansi` to memchr; this finishes the
// job for the stderr-prefix path.
static STDERR_RE: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(r"(?m)^\s*(?:stderr|err)\s*(?:\||:)\s*").unwrap()
});

fn filter_stderr_prefixes(text: &str) -> String {
    STDERR_RE.replace_all(text, "").into_owned()
}

// ─── Static filter registry ──────────────────────────────────────────────────
//
// Built once on first access; shared via `Arc<CompiledFilter>` thereafter.
// Insertion order does not matter — `get_builtin_filter` does a single
// `HashMap::get` lookup.

pub static BUILTIN_FILTERS: Lazy<HashMap<&'static str, Arc<CompiledFilter>>> = Lazy::new(|| {
    let mut m = HashMap::with_capacity(8);
    m.insert("git-status", Arc::new(make_git_status_filter()));
    m.insert("git-diff", Arc::new(make_git_diff_filter()));
    m.insert("cargo-test", Arc::new(make_cargo_test_filter()));
    m.insert("npm-test", Arc::new(make_npm_test_filter()));
    m.insert("docker-ps", Arc::new(make_docker_ps_filter()));
    m.insert("error-stacktrace", Arc::new(make_error_stacktrace_filter()));
    m.insert("shell-ls", Arc::new(make_shell_ls_filter()));
    m.insert("generic-error", Arc::new(make_generic_error_filter()));
    m
});

pub static GENERIC_FILTER: Lazy<Arc<CompiledFilter>> = Lazy::new(|| Arc::new(make_generic_filter()));

/// Obtiene el filtro built-in para un tipo de comando detectado.
///
/// Returns a cheaply-cloned `Arc<CompiledFilter>` — no struct
/// reconstruction, no regex compilation.
pub fn get_builtin_filter(detected_id: &str) -> Option<Arc<CompiledFilter>> {
    BUILTIN_FILTERS.get(detected_id).cloned()
}

/// Obtiene el filtro genérico de propósito general.
///
/// Returns a cheaply-cloned `Arc<CompiledFilter>` pointing at the single
/// process-wide `GENERIC_FILTER` instance.
pub fn get_generic_filter() -> Arc<CompiledFilter> {
    GENERIC_FILTER.clone()
}

// ─── Builtin filter constructors ─────────────────────────────────────────────
//
// Each `make_*_filter` is called exactly once per process lifetime, from
// inside the `Lazy::new` closure of `BUILTIN_FILTERS` / `GENERIC_FILTER`.
// Patterns are translated verbatim from the previous `get_builtin_filter`
// match arms — any divergence is a bug.

fn make_git_status_filter() -> CompiledFilter {
    filter_skeleton!(
        "git-status",
        /* strip_ansi */ true,
        /* filter_stderr */ false,
        /* replace */ vec![],
        /* match_output */ vec![],
        /* strip_patterns */ vec![
            compile_re(r"^\s*(\(use .*\))$"),
            compile_re(r"^\s*$"),
        ],
        /* keep_patterns */ vec![
            compile_re(r"^On branch "),
            compile_re(r"^Your branch "),
            compile_re(r"^Changes "),
            compile_re(r"^Untracked files:"),
            compile_re(r"^\s*(modified|new file|deleted|renamed):"),
            compile_re(r"^\s*[MADRCU?!]{1,2}\s+"),
            compile_re(r"nothing (added|committed)"),
        ],
        /* collapse_patterns */ vec![],
        /* truncate_line_at */ 0,
        /* on_empty */ "",
        /* truncate */ Some(CompiledTruncateConfig {
            max_lines: 60,
            head_lines: 15,
            tail_lines: 15,
            priority_patterns: vec![compile_re(r"(?i)(modified|deleted|Untracked)")],
        })
    )
}

fn make_git_diff_filter() -> CompiledFilter {
    filter_skeleton!(
        "git-diff",
        true,
        false,
        vec![],
        vec![],
        vec![compile_re(r"^\s*$")],
        vec![
            compile_re(r"^diff --git "),
            compile_re(r"^index "),
            compile_re(r"^--- "),
            compile_re(r"^\+\+\+ "),
            compile_re(r"^@@ "),
            compile_re(r"^[+-]"),
        ],
        vec![],
        0,
        "",
        Some(CompiledTruncateConfig {
            max_lines: 100,
            head_lines: 25,
            tail_lines: 25,
            priority_patterns: vec![compile_re(r"^@@ ")],
        })
    )
}

fn make_cargo_test_filter() -> CompiledFilter {
    filter_skeleton!(
        "cargo-test",
        true,
        false,
        vec![],
        vec![CompiledMatchOutputRule {
            re: compile_re(r"test result:.*ok\b"),
            message: "✓ all tests passed",
            unless: Some(compile_re(r"FAILED")),
        }],
        vec![
            compile_re(r"^\s*$"),
            compile_re(r"^\s*(Compiling|Finished|warning:)"),
            compile_re(r"^\s*(running \d+ tests?)"),
        ],
        vec![
            compile_re(r"^test .* FAILED"),
            compile_re(r"^test result:"),
            compile_re(r"^failures:"),
            compile_re(r"^\s+-->"),
            compile_re(r"^error\["),
        ],
        vec![],
        0,
        "✓ all tests passed",
        Some(CompiledTruncateConfig {
            max_lines: 60,
            head_lines: 5,
            tail_lines: 10,
            priority_patterns: vec![compile_re(r"(?i)(FAILED|error|panic)")],
        })
    )
}

fn make_npm_test_filter() -> CompiledFilter {
    filter_skeleton!(
        "npm-test",
        true,
        false,
        vec![],
        vec![CompiledMatchOutputRule {
            re: compile_re(r"Tests:\s+\d+\s+passed"),
            message: "✓ tests passed",
            unless: Some(compile_re(r"failed")),
        }],
        vec![
            compile_re(r"^\s*$"),
            compile_re(r"^\s*(PASS|FAIL)\s+"),
        ],
        vec![
            compile_re(r"FAIL\s+"),
            compile_re(r"✖\s+"),
            compile_re(r"×\s+"),
            compile_re(r"❯\s+"),
            compile_re(r"✓\s+"),
        ],
        vec![],
        0,
        "✓ tests passed",
        Some(CompiledTruncateConfig {
            max_lines: 60,
            head_lines: 5,
            tail_lines: 10,
            priority_patterns: vec![compile_re(r"(?i)(FAIL|error|✖)")],
        })
    )
}

fn make_docker_ps_filter() -> CompiledFilter {
    filter_skeleton!(
        "docker-ps",
        true,
        false,
        vec![],
        vec![],
        vec![],
        vec![
            compile_re(r"^CONTAINER ID"),
            compile_re(r"^[0-9a-f]{12}"),
        ],
        vec![],
        0,
        "(no containers)",
        Some(CompiledTruncateConfig {
            max_lines: 50,
            head_lines: 10,
            tail_lines: 5,
            priority_patterns: vec![],
        })
    )
}

fn make_error_stacktrace_filter() -> CompiledFilter {
    filter_skeleton!(
        "error-stacktrace",
        true,
        false,
        vec![],
        vec![],
        vec![],
        vec![
            compile_re(r"^(thread|panicked|Error|error)"),
            compile_re(r"^\s+at "),
            compile_re(r"^\s+\d+:"),
            compile_re(r"^\s+\["),
            compile_re(r"^Caused by:"),
            compile_re(r"^  .*: "),
        ],
        vec![compile_re(r"^\s+at ")],
        200,
        "",
        Some(CompiledTruncateConfig {
            max_lines: 50,
            head_lines: 5,
            tail_lines: 5,
            priority_patterns: vec![compile_re(r"(?i)(panicked|fatal|Error)")],
        })
    )
}

fn make_shell_ls_filter() -> CompiledFilter {
    filter_skeleton!(
        "shell-ls",
        true,
        false,
        vec![(compile_re(r"^total \d+"), "")],
        vec![],
        vec![compile_re(r"^\s*$")],
        vec![],
        vec![],
        0,
        "(empty directory)",
        Some(CompiledTruncateConfig {
            max_lines: 80,
            head_lines: 20,
            tail_lines: 10,
            priority_patterns: vec![],
        })
    )
}

fn make_generic_error_filter() -> CompiledFilter {
    filter_skeleton!(
        "generic-error",
        true,
        false,
        vec![],
        vec![],
        vec![],
        vec![compile_re(r"(?i)(error|failed|exception|traceback|panic|FAIL)")],
        vec![],
        0,
        "",
        Some(CompiledTruncateConfig {
            max_lines: 30,
            head_lines: 5,
            tail_lines: 5,
            priority_patterns: vec![compile_re(r"(?i)(error|failed)")],
        })
    )
}

fn make_generic_filter() -> CompiledFilter {
    filter_skeleton!(
        "generic",
        true,
        true,
        vec![],
        vec![],
        vec![
            compile_re(r"^\s*$"),
            compile_re(r"^\s*(warning:)"),
        ],
        vec![],
        vec![],
        0,
        "",
        Some(CompiledTruncateConfig {
            max_lines: 120,
            head_lines: 15,
            tail_lines: 10,
            priority_patterns: vec![compile_re(r"(?i)(error|failed|warning)")],
        })
    )
}

// ─── Filter pipeline ─────────────────────────────────────────────────────────

/// Aplica el pipeline de filtrado de un `CompiledFilter` al texto.
///
/// Returns `(filtered_text, applied_rule_names)` where each rule name is
/// a pre-computed `&'static str` (e.g. `"git-status::strip_ansi"`) — no
/// `format!` allocation per call.
///
/// The 10 pipeline stages run in the same order as the previous
/// `RtkFilter`-based implementation; behavior is identical.
pub fn apply_line_filter(text: &str, filter: &CompiledFilter) -> (String, Vec<&'static str>) {
    let mut applied_rules: Vec<&'static str> = Vec::new();
    let mut result = text.to_string();

    // 1. Strip ANSI codes
    if filter.strip_ansi {
        let stripped = strip_ansi(&result);
        if stripped != result {
            applied_rules.push(filter.rule_strip_ansi);
            result = stripped;
        }
    }

    // 2. Filter stderr prefixes (uses static STDERR_RE — no per-call compile)
    if filter.filter_stderr {
        let filtered = filter_stderr_prefixes(&result);
        if filtered != result {
            applied_rules.push(filter.rule_filter_stderr);
            result = filtered;
        }
    }

    // 3. Regex replacements (patterns pre-compiled)
    for (re, replacement) in &filter.replace {
        let replaced = re.replace_all(&result, *replacement).into_owned();
        if replaced != result {
            applied_rules.push(filter.rule_replace);
            result = replaced;
        }
    }

    // 4. Match output (short-circuit) — patterns pre-compiled
    for rule in &filter.match_output {
        if rule.re.is_match(&result) {
            let should_skip = rule
                .unless
                .as_ref()
                .map(|u| u.is_match(&result))
                .unwrap_or(false);
            if !should_skip {
                applied_rules.push(filter.rule_match_output);
                return (rule.message.to_string(), applied_rules);
            }
        }
    }

    // 5. Strip patterns (drop matching lines) — patterns pre-compiled
    if !filter.strip_patterns.is_empty() {
        let original_line_count = result.lines().count();
        let stripped: Vec<&str> = result
            .lines()
            .filter(|l| !filter.strip_patterns.iter().any(|r| r.is_match(l)))
            .collect();
        if stripped.len() != original_line_count {
            applied_rules.push(filter.rule_strip);
            result = stripped.join("\n");
        }
    }

    // 6. Keep patterns (only keep matching lines) — patterns pre-compiled
    if !filter.keep_patterns.is_empty() {
        let kept: Vec<&str> = result
            .lines()
            .filter(|l| filter.keep_patterns.iter().any(|r| r.is_match(l)))
            .collect();
        if !kept.is_empty() {
            applied_rules.push(filter.rule_keep);
            result = kept.join("\n");
        }
    }

    // 7. Collapse patterns (non-consecutive dedup) — patterns pre-compiled
    if !filter.collapse_patterns.is_empty() {
        let original_line_count = result.lines().count();
        let mut seen = std::collections::HashSet::new();
        let collapsed: Vec<&str> = result
            .lines()
            .filter(|l| {
                if filter.collapse_patterns.iter().any(|r| r.is_match(l)) {
                    let key = l.trim();
                    if seen.contains(key) {
                        return false;
                    }
                    seen.insert(key);
                }
                true
            })
            .collect();
        if collapsed.len() != original_line_count {
            applied_rules.push(filter.rule_collapse);
            result = collapsed.join("\n");
        }
    }

    // 8. Truncate lines (unicode-safe)
    if filter.truncate_line_at > 0 {
        let truncated: Vec<String> = result
            .lines()
            .map(|l| truncate_unicode_safe(l, filter.truncate_line_at))
            .collect();
        let new = truncated.join("\n");
        if new != result {
            applied_rules.push(filter.rule_truncate_line);
            result = new;
        }
    }

    // 9. Smart truncate (pre-compiled priority_patterns)
    if let Some(ref tc) = filter.truncate {
        let (truncated, did_truncate, _dropped) = smart_truncate(&result, tc);
        if did_truncate {
            applied_rules.push(filter.rule_truncate);
            result = truncated;
        }
    }

    // 10. On empty fallback
    if result.trim().is_empty() && !filter.on_empty.is_empty() {
        applied_rules.push(filter.rule_on_empty);
        result = filter.on_empty.to_string();
    }

    (result, applied_rules)
}

// ─── ANSI stripping (memchr-based, from Phase A) ─────────────────────────────

/// Strip ANSI CSI escape sequences from `text`.
///
/// CSI sequences are: ESC `[` [param bytes 0x30-0x3F] [intermediate bytes
/// 0x20-0x2F] [final byte 0x40-0x7E]. This covers color codes (SGR),
/// cursor movement, erase, etc.
///
/// Uses a byte scanner with memchr to find the next ESC (0x1B) — ~10x
/// faster than the regex it replaces, and no per-call regex compilation.
///
/// SAFETY: we only remove ASCII bytes (all CSI grammar bytes are ASCII),
/// so UTF-8 multi-byte sequences in the content are never split. The
/// final `String::from_utf8_unchecked` is safe.
fn strip_ansi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0x1B {
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                // Skip the ESC [ and everything until the final byte (0x40..=0x7E).
                i += 2;
                while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) {
                    i += 1;
                }
                // Skip the final byte too (if present — malformed input just ends).
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                // Lone ESC (at EOF, or not followed by '['): drop the ESC and
                // continue scanning from the next byte.
                i += 1;
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    // Safety: we only removed ASCII bytes (0x1B, 0x5B, and 0x20..=0x7E).
    // ASCII bytes are always single-byte in UTF-8, so removing them never
    // splits a multi-byte sequence. The remaining bytes are a valid UTF-8
    // subsequence of the original valid UTF-8 string.
    unsafe { String::from_utf8_unchecked(out) }
}

fn truncate_unicode_safe(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return s.to_string();
    }
    if max_chars <= 3 {
        return chars[..max_chars].iter().collect();
    }
    let prefix: String = chars[..max_chars - 3].iter().collect();
    format!("{}...", prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_git_status_strips_advice_lines() {
        let filter = get_builtin_filter("git-status").unwrap();
        let input = "On branch main\n  (use \"git add\" to update)\n\tmodified: foo.rs\n";
        let (result, rules) = apply_line_filter(input, &filter);
        assert!(!rules.is_empty());
        assert!(result.contains("On branch main"));
        assert!(result.contains("modified: foo.rs"));
        assert!(!result.contains("use \"git add\""));
    }

    #[test]
    fn test_cargo_test_match_output_short_circuits() {
        let filter = get_builtin_filter("cargo-test").unwrap();
        let input = "running 5 tests\ntest result: ok. 5 passed\n";
        let (result, rules) = apply_line_filter(input, &filter);
        assert!(rules.contains(&"cargo-test::match_output"));
        assert_eq!(result, "✓ all tests passed");
    }

    #[test]
    fn test_strip_ansi_removes_codes() {
        let input = "\u{1b}[32mgreen\u{1b}[0m";
        let output = strip_ansi(input);
        assert_eq!(output, "green");
    }

    #[test]
    fn strip_ansi_removes_color_codes() {
        let input = "\x1b[31mred text\x1b[0m";
        assert_eq!(strip_ansi(input), "red text");
    }

    #[test]
    fn strip_ansi_removes_bold_and_color() {
        let input = "\x1b[1;32mbold green\x1b[0m";
        assert_eq!(strip_ansi(input), "bold green");
    }

    #[test]
    fn strip_ansi_preserves_plain_text() {
        let input = "just plain text";
        assert_eq!(strip_ansi(input), "just plain text");
    }

    #[test]
    fn strip_ansi_handles_empty_string() {
        assert_eq!(strip_ansi(""), "");
    }

    #[test]
    fn strip_ansi_handles_malformed_escape_at_eof() {
        // ESC [ with no final byte — should be dropped, not panic.
        assert_eq!(strip_ansi("text\x1b["), "text");
        // ESC alone at EOF
        assert_eq!(strip_ansi("text\x1b"), "text");
        // ESC [ partial param then EOF
        assert_eq!(strip_ansi("text\x1b[31"), "text");
    }

    #[test]
    fn strip_ansi_preserves_multibyte_utf8() {
        let input = "\x1b[32mhello 世界\x1b[0m 😀";
        assert_eq!(strip_ansi(input), "hello 世界 😀");
    }

    #[test]
    fn strip_ansi_handles_cursor_movement() {
        // Cursor up: ESC [ A
        let input = "line1\x1b[Aline2";
        assert_eq!(strip_ansi(input), "line1line2");
    }

    #[test]
    fn strip_ansi_handles_multiple_escapes_in_sequence() {
        let input = "\x1b[31m\x1b[1mbold red\x1b[0m\x1b[0m";
        assert_eq!(strip_ansi(input), "bold red");
    }

    #[test]
    fn builtin_filters_registry_contains_all_eight_ids() {
        let ids = [
            "git-status",
            "git-diff",
            "cargo-test",
            "npm-test",
            "docker-ps",
            "error-stacktrace",
            "shell-ls",
            "generic-error",
        ];
        for id in ids {
            assert!(BUILTIN_FILTERS.contains_key(id), "missing builtin: {}", id);
        }
    }

    #[test]
    fn builtin_filters_share_one_instance_via_arc() {
        // Two lookups return Arcs pointing at the same allocation — this is
        // the whole point of the Lazy + Arc design (no per-call rebuild).
        let a = get_builtin_filter("git-status").unwrap();
        let b = get_builtin_filter("git-status").unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn generic_filter_is_static_singleton() {
        let a = get_generic_filter();
        let b = get_generic_filter();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn unknown_filter_id_returns_none() {
        assert!(get_builtin_filter("nonexistent-id").is_none());
    }

    #[test]
    fn filter_stderr_prefixes_strips_stderr_markers() {
        let input = "stderr| something\nerr: else\nplain line";
        let out = filter_stderr_prefixes(input);
        assert_eq!(out, "something\nelse\nplain line");
    }
}
