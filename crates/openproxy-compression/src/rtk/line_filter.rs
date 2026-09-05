use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use super::smart_truncate::{CompiledTruncateConfig, smart_truncate};

// ─── Compiled filter structs ─────────────────────────────────────────────────

/// A filter with all patterns pre-compiled and rule names pre-computed.
///
/// Built ONCE at startup (in a `std::sync::LazyLock` static) and shared
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
    pub replace: Box<[(regex::Regex, &'static str)]>,
    pub match_output: Box<[CompiledMatchOutputRule]>,
    pub strip_patterns: Box<[regex::Regex]>,
    pub keep_patterns: Box<[regex::Regex]>,
    pub collapse_patterns: Box<[regex::Regex]>,
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
    regex::Regex::new(pattern).unwrap_or_else(|e| panic!("invalid filter pattern {pattern:?}: {e}"))
}

/// Pre-compute a rule name like `"git-status::strip_ansi"`.
fn rule_name(id: &'static str, suffix: &'static str) -> &'static str {
    leak_string(format!("{id}::{suffix}"))
}

macro_rules! compiled_filter {
    (@set $b:ident, id, $v:literal) => { $b.id = $v; };
    (@set $b:ident, strip_ansi, $v:expr) => { $b.strip_ansi = $v; };
    (@set $b:ident, filter_stderr, $v:expr) => { $b.filter_stderr = $v; };
    (@set $b:ident, replace, [$(($re:literal, $rep:literal)),* $(,)?]) => {
        $b.replace = vec![$((compile_re($re), $rep)),*].into_boxed_slice();
    };
    (@set $b:ident, match_output, [$({ re: $re:literal, msg: $msg:literal, unless: $unless:literal $(,)? }),* $(,)?]) => {
        $b.match_output = vec![$(CompiledMatchOutputRule {
            re: compile_re($re),
            message: $msg,
            unless: Some(compile_re($unless)),
        }),*].into_boxed_slice();
    };
    (@set $b:ident, match_output, [$({ re: $re:literal, msg: $msg:literal $(,)? }),* $(,)?]) => {
        $b.match_output = vec![$(CompiledMatchOutputRule {
            re: compile_re($re),
            message: $msg,
            unless: None,
        }),*].into_boxed_slice();
    };
    (@set $b:ident, strip, [$($p:literal),* $(,)?]) => {
        $b.strip_patterns = vec![$(compile_re($p)),*].into_boxed_slice();
    };
    (@set $b:ident, keep, [$($p:literal),* $(,)?]) => {
        $b.keep_patterns = vec![$(compile_re($p)),*].into_boxed_slice();
    };
    (@set $b:ident, collapse, [$($p:literal),* $(,)?]) => {
        $b.collapse_patterns = vec![$(compile_re($p)),*].into_boxed_slice();
    };
    (@set $b:ident, truncate_line_at, $v:literal) => { $b.truncate_line_at = $v; };
    (@set $b:ident, on_empty, $v:literal) => { $b.on_empty = $v; };
    (@set $b:ident, truncate, { max: $max:literal, head: $head:literal, tail: $tail:literal, priority: [$($pri:literal),* $(,)?] $(,)? }) => {
        $b.truncate = Some(CompiledTruncateConfig {
            max_lines: $max,
            head_lines: $head,
            tail_lines: $tail,
            priority_patterns: vec![$(compile_re($pri)),*].into_boxed_slice(),
        });
    };
    (@set $b:ident, truncate, { max: $max:literal, head: $head:literal, tail: $tail:literal $(,)? }) => {
        $b.truncate = Some(CompiledTruncateConfig {
            max_lines: $max,
            head_lines: $head,
            tail_lines: $tail,
            priority_patterns: Box::new([]),
        });
    };

    ($($field:ident : $val:tt),* $(,)?) => {{
        struct Builder {
            id: &'static str,
            strip_ansi: bool,
            filter_stderr: bool,
            replace: Box<[(regex::Regex, &'static str)]>,
            match_output: Box<[CompiledMatchOutputRule]>,
            strip_patterns: Box<[regex::Regex]>,
            keep_patterns: Box<[regex::Regex]>,
            collapse_patterns: Box<[regex::Regex]>,
            truncate_line_at: usize,
            on_empty: &'static str,
            truncate: Option<CompiledTruncateConfig>,
        }
        let mut b = Builder {
            id: "",
            strip_ansi: true,
            filter_stderr: false,
            replace: Box::new([]),
            match_output: Box::new([]),
            strip_patterns: Box::new([]),
            keep_patterns: Box::new([]),
            collapse_patterns: Box::new([]),
            truncate_line_at: 0,
            on_empty: "",
            truncate: None,
        };
        $(
            compiled_filter!(@set b, $field, $val);
        )*
        let id = b.id;
        CompiledFilter {
            id,
            strip_ansi: b.strip_ansi,
            filter_stderr: b.filter_stderr,
            replace: b.replace,
            match_output: b.match_output,
            strip_patterns: b.strip_patterns,
            keep_patterns: b.keep_patterns,
            collapse_patterns: b.collapse_patterns,
            truncate_line_at: b.truncate_line_at,
            on_empty: b.on_empty,
            truncate: b.truncate,
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
static STDERR_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?m)^\s*(?:stderr|err)\s*(?:\||:)\s*").expect("valid regex")
});

fn filter_stderr_prefixes(text: &str) -> String {
    STDERR_RE.replace_all(text, "").into_owned()
}

// ─── Unified RTK Rules & Filter Registry ────────────────────────────────────

/// A unified RTK rule binding a command identifier, its detector function, and its optional compiled filter builder.
#[derive(Clone, Copy)]
pub struct RtkCommandRule {
    pub id: &'static str,
    pub detector: super::command_detector::DetectorFn,
    pub filter_builder: Option<fn() -> CompiledFilter>,
}

#[macro_export]
macro_rules! define_rtk_rule {
    ($id:literal, detector: $detector:expr, filter: $filter:expr) => {
        $crate::rtk::line_filter::RtkCommandRule {
            id: $id,
            detector: $detector,
            filter_builder: Some($filter),
        }
    };
    ($id:literal, detector: $detector:expr) => {
        $crate::rtk::line_filter::RtkCommandRule {
            id: $id,
            detector: $detector,
            filter_builder: None,
        }
    };
}

/// Unified registry of all RTK command rules pairing detectors with their filters.
pub static RTK_RULES: &[RtkCommandRule] = &[
    define_rtk_rule!("git-status", detector: super::command_detector::detect_git_status, filter: make_git_status_filter),
    define_rtk_rule!("git-diff", detector: super::command_detector::detect_git_diff, filter: make_git_diff_filter),
    define_rtk_rule!("git-log", detector: super::command_detector::detect_git_log),
    define_rtk_rule!("git-branch", detector: super::command_detector::detect_git_branch),
    define_rtk_rule!("cargo-test", detector: super::command_detector::detect_cargo_test, filter: make_cargo_test_filter),
    define_rtk_rule!("cargo-build", detector: super::command_detector::detect_cargo_build),
    define_rtk_rule!("npm-test", detector: super::command_detector::detect_npm_test, filter: make_npm_test_filter),
    define_rtk_rule!("npm-install", detector: super::command_detector::detect_npm_install),
    define_rtk_rule!("docker-ps", detector: super::command_detector::detect_docker_ps, filter: make_docker_ps_filter),
    define_rtk_rule!("docker-logs", detector: super::command_detector::detect_docker_logs),
    define_rtk_rule!("kubernetes", detector: super::command_detector::detect_kubernetes),
    define_rtk_rule!("shell-ls", detector: super::command_detector::detect_shell_ls, filter: make_shell_ls_filter),
    define_rtk_rule!("shell-grep", detector: super::command_detector::detect_shell_grep),
    define_rtk_rule!("error-stacktrace", detector: super::command_detector::detect_error_stacktrace, filter: make_error_stacktrace_filter),
    define_rtk_rule!("generic-error", detector: super::command_detector::detect_generic_error, filter: make_generic_error_filter),
];

// ─── Static filter registry ──────────────────────────────────────────────────
//
// Built once on first access; shared via `Arc<CompiledFilter>` thereafter.
// Insertion order does not matter — `get_builtin_filter` does a single
// `HashMap::get` lookup.

pub static BUILTIN_FILTERS: LazyLock<HashMap<&'static str, Arc<CompiledFilter>>> =
    LazyLock::new(|| {
        let mut m = HashMap::with_capacity(RTK_RULES.len());
        for rule in RTK_RULES {
            if let Some(builder) = rule.filter_builder {
                m.insert(rule.id, Arc::new(builder()));
            }
        }
        m
    });

pub static GENERIC_FILTER: LazyLock<Arc<CompiledFilter>> =
    LazyLock::new(|| Arc::new(make_generic_filter()));

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
    Arc::clone(&GENERIC_FILTER)
}

// ─── Builtin filter constructors ─────────────────────────────────────────────
//
// Each `make_*_filter` is called exactly once per process lifetime, from
// inside the `Lazy::new` closure of `BUILTIN_FILTERS` / `GENERIC_FILTER`.
// Patterns are translated verbatim from the previous `get_builtin_filter`
// match arms — any divergence is a bug.

fn make_git_status_filter() -> CompiledFilter {
    compiled_filter!(
        id: "git-status",
        strip: [r"^\s*(\(use .*\))$", r"^\s*$"],
        keep: [
            r"^On branch ",
            r"^Your branch ",
            r"^Changes ",
            r"^Untracked files:",
            r"^\s*(modified|new file|deleted|renamed):",
            r"^\s*[MADRCU?!]{1,2}\s+",
            r"nothing (added|committed)",
        ],
        truncate: {
            max: 60,
            head: 15,
            tail: 15,
            priority: [r"(?i)(modified|deleted|Untracked)"],
        },
    )
}

fn make_git_diff_filter() -> CompiledFilter {
    compiled_filter!(
        id: "git-diff",
        strip: [r"^\s*$"],
        keep: [
            r"^diff --git ",
            r"^index ",
            r"^--- ",
            r"^\+\+\+ ",
            r"^@@ ",
            r"^[+-]",
        ],
        truncate: {
            max: 100,
            head: 25,
            tail: 25,
            priority: [r"^@@ "],
        },
    )
}

fn make_cargo_test_filter() -> CompiledFilter {
    compiled_filter!(
        id: "cargo-test",
        match_output: [{
            re: r"test result:.*ok\b",
            msg: "✓ all tests passed",
            unless: r"FAILED",
        }],
        strip: [
            r"^\s*$",
            r"^\s*(Compiling|Finished|warning:)",
            r"^\s*(running \d+ tests?)",
        ],
        keep: [
            r"^test .* FAILED",
            r"^test result:",
            r"^failures:",
            r"^\s+-->",
            r"^error\[",
        ],
        on_empty: "✓ all tests passed",
        truncate: {
            max: 60,
            head: 5,
            tail: 10,
            priority: [r"(?i)(FAILED|error|panic)"],
        },
    )
}

fn make_npm_test_filter() -> CompiledFilter {
    compiled_filter!(
        id: "npm-test",
        match_output: [{
            re: r"Tests:\s+\d+\s+passed",
            msg: "✓ tests passed",
            unless: r"failed",
        }],
        strip: [r"^\s*$", r"^\s*(PASS|FAIL)\s+"],
        keep: [
            r"FAIL\s+",
            r"✖\s+",
            r"×\s+",
            r"❯\s+",
            r"✓\s+",
        ],
        on_empty: "✓ tests passed",
        truncate: {
            max: 60,
            head: 5,
            tail: 10,
            priority: [r"(?i)(FAIL|error|✖)"],
        },
    )
}

fn make_docker_ps_filter() -> CompiledFilter {
    compiled_filter!(
        id: "docker-ps",
        keep: [r"^CONTAINER ID", r"^[0-9a-f]{12}"],
        on_empty: "(no containers)",
        truncate: {
            max: 50,
            head: 10,
            tail: 5,
        },
    )
}

fn make_error_stacktrace_filter() -> CompiledFilter {
    compiled_filter!(
        id: "error-stacktrace",
        keep: [
            r"^(thread|panicked|Error|error)",
            r"^\s+at ",
            r"^\s+\d+:",
            r"^\s+\[",
            r"^Caused by:",
            r"^  .*: ",
        ],
        collapse: [r"^\s+at "],
        truncate_line_at: 200,
        truncate: {
            max: 50,
            head: 5,
            tail: 5,
            priority: [r"(?i)(panicked|fatal|Error)"],
        },
    )
}

fn make_shell_ls_filter() -> CompiledFilter {
    compiled_filter!(
        id: "shell-ls",
        replace: [(r"^total \d+", "")],
        strip: [r"^\s*$"],
        on_empty: "(empty directory)",
        truncate: {
            max: 80,
            head: 20,
            tail: 10,
        },
    )
}

fn make_generic_error_filter() -> CompiledFilter {
    compiled_filter!(
        id: "generic-error",
        keep: [r"(?i)(error|failed|exception|traceback|panic|FAIL)"],
        truncate: {
            max: 30,
            head: 5,
            tail: 5,
            priority: [r"(?i)(error|failed)"],
        },
    )
}

fn make_generic_filter() -> CompiledFilter {
    compiled_filter!(
        id: "generic",
        filter_stderr: true,
        strip: [r"^\s*$", r"^\s*(warning:)"],
        truncate: {
            max: 120,
            head: 15,
            tail: 10,
            priority: [r"(?i)(error|failed|warning)"],
        },
    )
}

// ─── Filter pipeline ─────────────────────────────────────────────────────────

fn apply_cleanups_and_replaces(
    result: &mut String,
    filter: &CompiledFilter,
    applied: &mut Vec<&'static str>,
) {
    if filter.strip_ansi
        && let Cow::Owned(s) = strip_ansi(result)
    {
        applied.push(filter.rule_strip_ansi);
        *result = s;
    }
    if filter.filter_stderr {
        let filtered = filter_stderr_prefixes(result);
        if filtered != *result {
            applied.push(filter.rule_filter_stderr);
            *result = filtered;
        }
    }
    for (re, replacement) in &filter.replace {
        let replaced = re.replace_all(result, *replacement).into_owned();
        if replaced != *result {
            applied.push(filter.rule_replace);
            *result = replaced;
        }
    }
}

fn check_match_output_stage(
    result: &str,
    filter: &CompiledFilter,
    applied: &mut Vec<&'static str>,
) -> Option<String> {
    for rule in &filter.match_output {
        if rule.re.is_match(result) {
            let should_skip = rule.unless.as_ref().is_some_and(|u| u.is_match(result));
            if !should_skip {
                applied.push(filter.rule_match_output);
                return Some(rule.message.to_string());
            }
        }
    }
    None
}

thread_local! {
    static ANSI_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    static LINE_SPANS_BUF: std::cell::RefCell<Vec<(usize, usize)>> = const { std::cell::RefCell::new(Vec::new()) };
    static SEEN_COLLAPSE_SET: std::cell::RefCell<std::collections::HashSet<String>> = std::cell::RefCell::new(std::collections::HashSet::new());
    static OUTPUT_BUF: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

fn apply_line_filtering_pipeline(
    result: &mut String,
    filter: &CompiledFilter,
    applied: &mut Vec<&'static str>,
) {
    let has_strip = !filter.strip_patterns.is_empty();
    let has_keep = !filter.keep_patterns.is_empty();
    let has_collapse = !filter.collapse_patterns.is_empty();
    let has_line_trunc = filter.truncate_line_at > 0;

    if !has_strip && !has_keep && !has_collapse && !has_line_trunc {
        return;
    }

    LINE_SPANS_BUF.with_borrow_mut(|line_spans| {
        line_spans.clear();
        let base = result.as_ptr() as usize;
        for line in result.lines() {
            let start = line.as_ptr() as usize - base;
            let end = start + line.len();
            line_spans.push((start, end));
        }

        let mut modified = false;

        if has_strip {
            let before_len = line_spans.len();
            line_spans.retain(|&(start, end)| {
                let l = &result[start..end];
                !filter.strip_patterns.iter().any(|r| r.is_match(l))
            });
            if line_spans.len() != before_len {
                applied.push(filter.rule_strip);
                modified = true;
            }
        }

        if has_keep {
            let any_kept = line_spans.iter().any(|&(start, end)| {
                let l = &result[start..end];
                filter.keep_patterns.iter().any(|r| r.is_match(l))
            });
            if any_kept {
                line_spans.retain(|&(start, end)| {
                    let l = &result[start..end];
                    filter.keep_patterns.iter().any(|r| r.is_match(l))
                });
                applied.push(filter.rule_keep);
                modified = true;
            }
        }

        if has_collapse {
            let before_len = line_spans.len();
            SEEN_COLLAPSE_SET.with_borrow_mut(|seen| {
                seen.clear();
                line_spans.retain(|&(start, end)| {
                    let line = &result[start..end];
                    if filter.collapse_patterns.iter().any(|r| r.is_match(line)) {
                        let key = line.trim();
                        if seen.contains(key) {
                            return false;
                        }
                        seen.insert(key.to_string());
                    }
                    true
                });
            });
            if line_spans.len() != before_len {
                applied.push(filter.rule_collapse);
                modified = true;
            }
        }

        let mut any_truncated = false;
        if has_line_trunc {
            for &(start, end) in line_spans.iter() {
                let line = &result[start..end];
                if let Some(cut) = find_cut_byte(line, filter.truncate_line_at)
                    && cut < line.len()
                {
                    any_truncated = true;
                    break;
                }
            }
            if any_truncated {
                applied.push(filter.rule_truncate_line);
            }
        }

        if modified || any_truncated {
            OUTPUT_BUF.with_borrow_mut(|out| {
                out.clear();
                out.reserve(result.len());
                let mut first = true;
                for &(start, end) in line_spans.iter() {
                    if !first {
                        out.push('\n');
                    }
                    first = false;
                    let line = &result[start..end];
                    if any_truncated {
                        let t = truncate_unicode_safe(line, filter.truncate_line_at);
                        out.push_str(&t);
                    } else {
                        out.push_str(line);
                    }
                }
                result.clear();
                result.push_str(out);
            });
        }
    });
}

fn apply_truncation_stages(
    result: &mut String,
    filter: &CompiledFilter,
    applied: &mut Vec<&'static str>,
) {
    if let Some(ref tc) = filter.truncate {
        let (truncated, did_truncate, _dropped) = smart_truncate(result, tc);
        if did_truncate {
            applied.push(filter.rule_truncate);
            *result = truncated;
        }
    }
    if result.trim().is_empty() && !filter.on_empty.is_empty() {
        applied.push(filter.rule_on_empty);
        *result = filter.on_empty.to_string();
    }
}

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

    apply_cleanups_and_replaces(&mut result, filter, &mut applied_rules);
    if let Some(short_circuit) = check_match_output_stage(&result, filter, &mut applied_rules) {
        return (short_circuit, applied_rules);
    }
    apply_line_filtering_pipeline(&mut result, filter, &mut applied_rules);
    apply_truncation_stages(&mut result, filter, &mut applied_rules);

    (result, applied_rules)
}

// ─── ANSI stripping (memchr-based, from Phase A) ─────────────────────────────

fn skip_csi_sequence(bytes: &[u8], mut i: usize) -> usize {
    i += 2;
    while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) {
        i += 1;
    }
    if i < bytes.len() {
        i += 1;
    }
    i
}

/// Strip ANSI CSI escape sequences from `text`.
///
/// CSI sequences are: ESC `[` [param bytes 0x30-0x3F] [intermediate bytes
/// 0x20-0x2F] [final byte 0x40-0x7E]. This covers color codes (SGR),
/// cursor movement, erase, etc.
///
/// Uses a byte scanner with memchr to find the next ESC (0x1B) — ~10x
/// faster than the regex it replaces, and no per-call regex compilation.
/// Fast-paths to `Cow::Borrowed` if no ESC byte is found.
///
/// SAFETY: we only remove ASCII bytes (all CSI grammar bytes are ASCII),
/// so UTF-8 multi-byte sequences in the content are never split. The
/// final `String::from_utf8_unchecked` is safe.
fn strip_ansi(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let Some(first_esc) = memchr::memchr(0x1B, bytes) else {
        return Cow::Borrowed(text);
    };

    ANSI_BUF.with_borrow_mut(|out| {
        out.clear();
        out.reserve(bytes.len());
        out.extend_from_slice(&bytes[..first_esc]);
        let mut i = first_esc;
        while i < bytes.len() {
            if bytes[i] == 0x1B {
                if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                    i = skip_csi_sequence(bytes, i);
                } else {
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
        let s = std::str::from_utf8(out).unwrap_or_default();
        Cow::Owned(s.to_string())
    })
}

fn find_cut_byte(s: &str, max_chars: usize) -> Option<usize> {
    let target_cut = if max_chars > 3 {
        max_chars - 3
    } else {
        max_chars
    };
    let mut cut_byte = None;
    let mut count = 0;
    for (byte_idx, _) in s.char_indices() {
        if count == target_cut {
            cut_byte = Some(byte_idx);
        }
        count += 1;
        if count > max_chars {
            return Some(cut_byte.unwrap_or(s.len()));
        }
    }
    None
}

fn truncate_unicode_safe(s: &str, max_chars: usize) -> Cow<'_, str> {
    if max_chars == 0 {
        return Cow::Borrowed(s);
    }
    let Some(mut cut) = find_cut_byte(s, max_chars) else {
        return Cow::Borrowed(s);
    };
    if !s.is_char_boundary(cut) {
        cut = s.floor_char_boundary(cut);
    }
    if max_chars <= 3 {
        Cow::Borrowed(&s[..cut])
    } else {
        Cow::Owned(format!("{}...", &s[..cut]))
    }
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
            assert!(BUILTIN_FILTERS.contains_key(id), "missing builtin: {id}");
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

    #[test]
    fn test_truncate_unicode_safe() {
        assert_eq!(truncate_unicode_safe("hello world", 0), "hello world");
        assert_eq!(truncate_unicode_safe("hello", 5), "hello");
        assert_eq!(truncate_unicode_safe("hello", 10), "hello");
        assert_eq!(truncate_unicode_safe("hello", 2), "he");
        assert_eq!(truncate_unicode_safe("hello world", 5), "he...");
        assert_eq!(truncate_unicode_safe("🦀🦀🦀🦀", 4), "🦀🦀🦀🦀");
        assert_eq!(truncate_unicode_safe("🦀🦀🦀🦀🦀", 4), "🦀...");
        assert_eq!(truncate_unicode_safe("🦀🦀🦀🦀", 2), "🦀🦀");

        // Verify Borrowed vs Owned
        let s = "short";
        assert!(matches!(truncate_unicode_safe(s, 10), Cow::Borrowed(_)));
        let long = "longer string here";
        assert!(matches!(truncate_unicode_safe(long, 6), Cow::Owned(_)));
    }

    #[test]
    fn test_rtk_rules_unified() {
        assert_eq!(RTK_RULES.len(), 15);
        for rule in RTK_RULES {
            assert!(!rule.id.is_empty());
            if let Some(builder) = rule.filter_builder {
                let compiled = builder();
                assert_eq!(compiled.id, rule.id);
            }
        }
    }
}
