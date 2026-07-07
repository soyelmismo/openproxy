//! `ContentDetector` + `ContentRouter`: shape-based dispatch to compressors.
//!
//! Inspired by headroom's `ContentDetector` + `ContentRouter`. Instead of
//! routing by the CLI command that produced the content (which the existing
//! `rtk` module already does), this module inspects the *shape* of a content
//! string and dispatches it to the appropriate compressor
//! ([`crate::compression::smart_crusher`] for JSON arrays,
//! [`crate::compression::diff_compressor`] for git diffs,
//! [`crate::compression::log_compressor`] for build/test output, etc.).
//!
//! ## Detection vs. routing
//!
//! [`detect`] classifies a content string into a [`ContentType`]. It is a pure
//! shape check (regex / first-N-lines scan) and never invokes a compressor,
//! so it is cheap to call and has no side effects.
//!
//! [`route_content`] calls [`detect`] and then hands the string to the
//! matching compressor's public string-level entry point (e.g.
//! [`smart_crusher::crush_json_string`]). It returns
//! `Some((compressed, technique))` only when a compressor both applied *and*
//! produced strictly smaller output; otherwise `None`. The caller is expected
//! to keep the original content when `None` is returned.
//!
//! ## Detection order
//!
//! The [`detect`] order is most-specific-first so that, e.g., a git diff
//! embedded in a larger build log is still classified as a [`GitDiff`]. The
//! order is:
//!
//! 1. [`ContentType::JsonArray`] — leading `[` + parses as a ≥5-element
//!    JSON array.
//! 2. [`ContentType::GitDiff`] — `diff --git` or a strict `@@ -a,b +c,d @@`
//!    hunk header in the first 10 lines.
//! 3. [`ContentType::BuildOutput`] — ≥2 of {pytest, cargo, jest, make,
//!    generic-error} patterns match in the first 50 lines.
//! 4. [`ContentType::SearchResults`] — ≥3 lines look like
//!    `path:line:content` (grep/ripgrep).
//! 5. [`ContentType::Tabular`] — CSV header (≥2 commas, few spaces) or a
//!    markdown table separator (`|---`) in the first 5 lines.
//! 6. [`ContentType::SourceCode`] — first non-empty line is an import-like
//!    prefix, or ≥3 lines match structural keywords (`fn `, `class `, …).
//! 7. [`ContentType::PlainText`] — fallback.
//!
//! [`GitDiff`]: ContentType::GitDiff

use crate::compression::{diff_compressor, log_compressor, smart_crusher};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

/// Detected content type based on content shape (not command).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    /// A JSON array of homogeneous objects (API response, DB rows, …).
    JsonArray,
    /// `git diff` output (`diff --git` / `@@ -a,b +c,d @@`).
    GitDiff,
    /// Build / test runner output (pytest, cargo, jest, make, …).
    BuildOutput,
    /// Source code in a recognized language.
    SourceCode,
    /// `path:line:content` output from grep / ripgrep.
    SearchResults,
    /// CSV header or markdown table.
    Tabular,
    /// Anything else — no compressor applies.
    PlainText,
}

// ─── Detection regexes ─────────────────────────────────────────────────────
//
// All regexes are compiled once via `once_cell::Lazy` and reused across
// calls. `^`-anchored patterns are applied per-line (the line is the whole
// search string), so they don't need the `(?m)` flag.

/// Strict hunk header: `^@@ -\d+,\d+ \+\d+,\d+ @@`.
static HUNK_HEADER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^@@ -\d+,\d+ \+\d+,\d+ @@").unwrap());

/// `^path:line:` pattern from grep / ripgrep output.
static SEARCH_RESULT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[\w/.\-]+:\d+:").unwrap());

/// Source-code structural keywords: `fn `, `func `, `def `, `class `, etc.
static SOURCE_KEYWORD_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(fn |func |def |class |struct |enum |interface |public |private |protected )")
        .unwrap()
});

/// Source-code import-like first line: `import `, `from `, `use `,
/// `package `, `#include `, `require(`.
static SOURCE_IMPORT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(import |from |use |package |#include |require\()").unwrap());

/// Generic error/warn/etc. token (case-insensitive) used by the
/// "≥5 matching lines" sub-rule of BuildOutput detection.
static GENERIC_ERROR_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(error|fail|warn|traceback|panic|exception)").unwrap());

/// `^make[N]:` (N is digits).
static MAKE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^make\[\d+\]:").unwrap());

/// `^running N tests` (cargo).
static CARGO_RUNNING_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^running \d+ tests").unwrap());

/// Maximum number of lines to scan for detection. The spec says "first 100
/// lines" for the overall scan; sub-scans (git diff, build output, tabular)
/// apply their own smaller windows via `.take(N)` on the head iterator.
/// 200 lines covers typical test output (header + body + errors + summary).
const DETECT_SCAN_LINES: usize = 200;

/// Minimum array length for `ContentType::JsonArray` (matches
/// `smart_crusher::MIN_ITEMS`).
const JSON_ARRAY_MIN_ITEMS: usize = 5;

/// Detect the content type of a text string.
///
/// Order matters — see the [module docs](self) for the full precedence list.
/// The scan is bounded to the first [`DETECT_SCAN_LINES`] lines so detection
/// is O(N) in the (truncated) input length, never in the full content size.
pub fn detect(content: &str) -> ContentType {
    let lines: Vec<&str> = content.lines().take(DETECT_SCAN_LINES).collect();

    if is_json_array(content) {
        return ContentType::JsonArray;
    }
    if is_git_diff(&lines) {
        return ContentType::GitDiff;
    }
    if is_build_output(&lines) {
        return ContentType::BuildOutput;
    }
    if is_search_results(&lines) {
        return ContentType::SearchResults;
    }
    if is_tabular(&lines) {
        return ContentType::Tabular;
    }
    if is_source_code(&lines) {
        return ContentType::SourceCode;
    }
    ContentType::PlainText
}

/// Route a single message's content to the appropriate compressor.
///
/// Returns `Some((compressed_content, technique_name))` when a compressor
/// both applied and produced strictly smaller output; otherwise `None`.
///
/// This is the main entry point for content-shape-based routing. The caller
/// is expected to keep the original content when `None` is returned.
///
/// # v1 coverage
///
/// - [`ContentType::JsonArray`] → [`smart_crusher::crush_json_string`].
/// - [`ContentType::GitDiff`] → [`diff_compressor::compress_diff_string`].
/// - [`ContentType::BuildOutput`] → [`log_compressor::compress_log_string`].
/// - [`ContentType::SearchResults`] / [`ContentType::Tabular`] /
///   [`ContentType::SourceCode`] / [`ContentType::PlainText`] → `None` (no
///   shape-specific compressor exists yet in v1).
pub fn route_content(content: &str) -> Option<(String, &'static str)> {
    match detect(content) {
        ContentType::JsonArray => smart_crusher::crush_json_string(content),
        ContentType::GitDiff => diff_compressor::compress_diff_string(content),
        ContentType::BuildOutput => log_compressor::compress_log_string(content),
        ContentType::SearchResults => None,
        ContentType::Tabular => None,
        ContentType::SourceCode => None,
        ContentType::PlainText => None,
    }
}

// ─── Per-type detectors ────────────────────────────────────────────────────

/// `JsonArray`: first non-whitespace char is `[` *and* the string parses as
/// a `Value::Array` with ≥ [`JSON_ARRAY_MIN_ITEMS`] items.
fn is_json_array(content: &str) -> bool {
    let trimmed = content.trim_start();
    if !trimmed.starts_with('[') {
        return false;
    }
    // serde_json is lenient about leading/trailing whitespace, so parsing
    // the original `content` is equivalent to parsing `trimmed`.
    matches!(
        serde_json::from_str::<Value>(content),
        Ok(Value::Array(a)) if a.len() >= JSON_ARRAY_MIN_ITEMS
    )
}

/// `GitDiff`: first 10 lines contain `diff --git` OR a strict
/// `@@ -\d+,\d+ \+\d+,\d+ @@` hunk header.
fn is_git_diff(lines: &[&str]) -> bool {
    lines
        .iter()
        .take(10)
        .any(|l| l.starts_with("diff --git ") || HUNK_HEADER_RE.is_match(l))
}

/// `BuildOutput`: first 50 lines match ≥2 of these patterns:
/// - pytest: line contains `===== test session starts =====`
/// - cargo: line matches `^running \d+ tests` or starts with `test result:`
/// - jest: line starts with `PASS ` or `FAIL `, or contains `Test Suites:`
/// - make: line matches `^make[N]:`
/// - generic: ≥5 lines match `(error|fail|warn|traceback|panic|exception)`
///   (case-insensitive).
fn is_build_output(lines: &[&str]) -> bool {
    // Scan up to 200 lines for build-output patterns — test output
    // often has a header in the first few lines and errors/summaries
    // at the end, so scanning only 50 lines misses the error signals.
    let head: Vec<&str> = lines.iter().take(200).copied().collect();
    let mut matches = 0;

    // pytest
    if head
        .iter()
        .any(|l| l.contains("===== test session starts ====="))
    {
        matches += 1;
    }
    // cargo
    if head
        .iter()
        .any(|l| CARGO_RUNNING_RE.is_match(l) || l.starts_with("test result:"))
    {
        matches += 1;
    }
    // jest
    if head
        .iter()
        .any(|l| l.starts_with("PASS ") || l.starts_with("FAIL ") || l.contains("Test Suites:"))
    {
        matches += 1;
    }
    // make
    if head.iter().any(|l| MAKE_RE.is_match(l)) {
        matches += 1;
    }
    // generic: ≥5 lines matching the error/warn token regex.
    let generic_count = head.iter().filter(|l| GENERIC_ERROR_RE.is_match(l)).count();
    if generic_count >= 5 {
        matches += 1;
    }

    matches >= 2
}

/// `SearchResults`: ≥3 lines match `^[\w/.\-]+:\d+:` (the grep/ripgrep
/// `path:line:content` shape). Scans the (already first-100-lines) head.
fn is_search_results(lines: &[&str]) -> bool {
    let count = lines
        .iter()
        .filter(|l| SEARCH_RESULT_RE.is_match(l))
        .count();
    count >= 3
}

/// `Tabular`: first line looks like a CSV header (≥2 commas, no prose) OR a
/// markdown table separator (`|---`) appears in the first 5 lines.
///
/// "No prose" is approximated as `space_count <= comma_count` — a real CSV
/// header like `id,name,email` has 0 spaces and 2 commas, while a sentence
/// like `Hello, world. How are you, today?` has 4 spaces and 2 commas.
///
/// Lines starting with `[` or `{` are excluded from the CSV check so that a
/// single-line JSON array/object literal (which can have many commas and no
/// spaces) is not mistaken for a CSV header. JSON shapes are handled by
/// [`is_json_array`] upstream.
fn is_tabular(lines: &[&str]) -> bool {
    if let Some(first) = lines.first() {
        let trimmed = first.trim_start();
        // Don't confuse a JSON array/object literal with a CSV header.
        if !trimmed.starts_with('[') && !trimmed.starts_with('{') {
            let comma_count = first.matches(',').count();
            let space_count = first.matches(' ').count();
            if comma_count >= 2 && space_count <= comma_count {
                return true;
            }
        }
    }
    // Markdown table: a separator line containing `|---` (covers
    // `|---|---|`, `| --- | --- |`, `|:---|:---|` via the leading pipe).
    lines.iter().take(5).any(|l| l.contains("|---"))
}

/// `SourceCode`: first non-empty line starts with an import-like prefix
/// (`import `, `from `, `use `, `package `, `#include `, `require(`) OR ≥3
/// lines match a structural keyword (`fn `, `func `, `def `, `class `,
/// `struct `, `enum `, `interface `, `public `, `private `, `protected `).
fn is_source_code(lines: &[&str]) -> bool {
    // First non-empty line check: starts with `import `, `use `, etc.
    if let Some(first) = lines.iter().find(|l| !l.trim().is_empty()).copied()
        && SOURCE_IMPORT_RE.is_match(first)
    {
        return true;
    }
    // Structural keyword density check.
    let keyword_matches = lines
        .iter()
        .filter(|l| SOURCE_KEYWORD_RE.is_match(l))
        .count();
    keyword_matches >= 3
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── detect() tests ────────────────────────────────────────────────────

    #[test]
    fn test_detect_json_array() {
        let mut items = Vec::new();
        for i in 1..=5 {
            items.push(serde_json::json!({ "a": i }));
        }
        let content = serde_json::to_string(&Value::Array(items)).unwrap();
        assert_eq!(detect(&content), ContentType::JsonArray);
    }

    #[test]
    fn test_detect_json_array_requires_min_items() {
        // 4 items: below the JSON_ARRAY_MIN_ITEMS=5 floor.
        let items: Vec<Value> = (1..=4).map(|i| serde_json::json!({ "a": i })).collect();
        let content = serde_json::to_string(&Value::Array(items)).unwrap();
        assert_eq!(detect(&content), ContentType::PlainText);
    }

    #[test]
    fn test_detect_git_diff() {
        let content = "diff --git a/foo b/foo\n\
index abc..def 100644\n\
--- a/foo\n\
+++ b/foo\n\
@@ -1,3 +1,3 @@\n\
 ctx\n\
-old\n\
+new\n";
        assert_eq!(detect(content), ContentType::GitDiff);
    }

    #[test]
    fn test_detect_git_diff_hunk_only() {
        // No `diff --git` line, but a valid `@@ -a,b +c,d @@` hunk header.
        let content = "@@ -10,5 +10,7 @@\n\
 context\n\
-old\n\
+new\n";
        assert_eq!(detect(content), ContentType::GitDiff);
    }

    #[test]
    fn test_detect_build_output_pytest() {
        // Pytest session banner (1 pattern) + 5+ FAILED lines (generic, 2nd
        // pattern) = ≥2 patterns → BuildOutput.
        let mut lines: Vec<String> = Vec::new();
        lines.push(
            "========================= test session starts =========================".to_string(),
        );
        for i in 1..=5 {
            lines.push(format!("test_module.py::test_{} PASSED [ 50%]", i));
        }
        for i in 6..=10 {
            lines.push(format!("test_module.py::test_{} FAILED [ 50%]", i));
        }
        lines.push(
            "========================= short test summary info ========================="
                .to_string(),
        );
        for i in 6..=10 {
            lines.push(format!("FAILED test_module.py::test_{}", i));
        }
        let content = lines.join("\n");
        assert_eq!(detect(&content), ContentType::BuildOutput);
    }

    #[test]
    fn test_detect_build_output_cargo() {
        // `running 10 tests` (cargo pattern) + 5+ FAILED lines (generic) =
        // ≥2 patterns → BuildOutput.
        let mut lines: Vec<String> = Vec::new();
        lines.push("running 10 tests".to_string());
        for i in 1..=5 {
            lines.push(format!("test test_{} ... ok", i));
        }
        for i in 6..=10 {
            lines.push(format!("test test_{} ... FAILED", i));
        }
        lines.push("test result: FAILED. 5 passed; 5 failed; 0 ignored; 0 measured".to_string());
        let content = lines.join("\n");
        assert_eq!(detect(&content), ContentType::BuildOutput);
    }

    #[test]
    fn test_detect_search_results() {
        // 4 lines matching `^path:line:` (≥3 required).
        let content = "src/main.rs:42:fn main() {\n\
src/main.rs:43:    println!(\"hello\");\n\
src/utils.rs:10:pub fn helper() {\n\
src/utils.rs:25:    let x = 5;\n";
        assert_eq!(detect(content), ContentType::SearchResults);
    }

    #[test]
    fn test_detect_plain_text() {
        let content = "Hello world. This is a plain text message. It has no special structure.";
        assert_eq!(detect(content), ContentType::PlainText);
    }

    #[test]
    fn test_detect_empty_string_is_plain_text() {
        assert_eq!(detect(""), ContentType::PlainText);
    }

    // ─── route_content() tests ─────────────────────────────────────────────

    #[test]
    fn test_route_json_array() {
        let mut items = Vec::new();
        for i in 1..=10 {
            items.push(serde_json::json!({
                "id": i,
                "name": format!("user{}", i),
                "status": "ok",
            }));
        }
        let content = serde_json::to_string(&Value::Array(items)).unwrap();
        let result = route_content(&content);
        let (compressed, technique) = result.expect("should route to smart_crusher");
        assert!(
            technique == smart_crusher::LOSSLESS_TECHNIQUE
                || technique == smart_crusher::LOSSY_TECHNIQUE,
            "unexpected technique: {}",
            technique
        );
        assert!(compressed.len() < content.len());
        assert!(
            compressed.starts_with("#schema:") || compressed.starts_with("[#crushed:"),
            "unexpected compressed output: {}",
            compressed
        );
    }

    #[test]
    fn test_route_git_diff() {
        // 49-line diff (4 metadata + 5 hunks × 9 body lines): ≥ MIN_DIFF_LINES=30.
        let mut lines: Vec<String> = Vec::new();
        lines.push("diff --git a/foo.rs b/foo.rs".to_string());
        lines.push("index abc..def 100644".to_string());
        lines.push("--- a/foo.rs".to_string());
        lines.push("+++ b/foo.rs".to_string());
        for h in 0..5u32 {
            let base = (h * 10 + 1) as usize;
            lines.push(format!("@@ -{},8 +{},8 @@", base, base));
            for c in 0..3u32 {
                lines.push(format!(" context_{}_{}", h, c));
            }
            lines.push(format!("-old_line_{}", h));
            lines.push(format!("+new_line_{}", h));
            for c in 0..3u32 {
                lines.push(format!(" context_{}_{}", h, c + 3));
            }
        }
        let content = lines.join("\n");
        assert!(
            content.lines().count() >= 30,
            "test diff should have ≥30 lines, got {}",
            content.lines().count()
        );
        let result = route_content(&content);
        let (compressed, technique) = result.expect("should route to diff_compressor");
        assert_eq!(technique, diff_compressor::TECHNIQUE);
        assert!(compressed.len() < content.len());
        assert!(
            compressed.starts_with("[#diff_compressed:"),
            "unexpected compressed output: {}",
            compressed
        );
    }

    #[test]
    fn test_route_build_output() {
        // 32-line pytest output: 1 banner + 25 PASSED + 5 FAILED + 1 summary.
        // ≥ MIN_LOG_LINES=30 and compressible by log_compressor.
        let mut lines: Vec<String> = Vec::new();
        lines.push(
            "========================= test session starts =========================".to_string(),
        );
        for i in 0..25 {
            lines.push(format!("test_module.py::test_{} PASSED [ 50%]", i));
        }
        for i in 0..5 {
            lines.push(format!("test_module.py::test_{} FAILED [ 50%]", i));
        }
        lines.push(
            "========================= 5 failed, 25 passed in 5.0s ========================="
                .to_string(),
        );
        assert_eq!(lines.len(), 32);
        let content = lines.join("\n");
        let result = route_content(&content);
        let (compressed, technique) = result.expect("should route to log_compressor");
        assert_eq!(technique, log_compressor::TECHNIQUE);
        assert!(compressed.len() < content.len());
        assert!(
            compressed.contains("[#log_compressed:"),
            "unexpected compressed output: {}",
            compressed
        );
    }

    #[test]
    fn test_route_plain_text_returns_none() {
        let content = "Hello world. This is a plain text message. It has no special structure.";
        assert_eq!(
            route_content(content),
            None,
            "plain text should not be compressed"
        );
    }

    #[test]
    fn test_route_source_code_returns_none() {
        // Source-code shape, but v1 has no source compressor → None.
        let content = "use std::io;\n\
fn main() {\n\
    println!(\"hello\");\n\
}\n\
fn helper() {}\n";
        assert_eq!(detect(content), ContentType::SourceCode);
        assert_eq!(route_content(content), None);
    }

    #[test]
    fn test_route_search_results_returns_none() {
        // Search-results shape, but v1 has no search compressor → None.
        let content = "src/main.rs:42:fn main() {\n\
src/main.rs:43:    println!(\"hello\");\n\
src/utils.rs:10:pub fn helper() {\n";
        assert_eq!(detect(content), ContentType::SearchResults);
        assert_eq!(route_content(content), None);
    }
}
