//! DiffCompressor: hunk-aware git diff compressor.
//!
//! Inspired by headroom's DiffCompressor. Detects `git diff` output and
//! compresses it by:
//! 1. Capping hunks-per-file (default 10) — preferring hunks with
//!    additions/deletions.
//! 2. Reducing context lines around changes (default 2 instead of git's
//!    default 3).
//! 3. Always preserving additions (`+`) and deletions (`-`) verbatim.
//! 4. Capping total files (default 20).
//!
//! ## Safety
//! - Only operates on `role == "tool"` and `role == "assistant"` messages.
//! - Only operates on content with ≥ `MIN_DIFF_LINES` lines.
//! - Only operates when git diff format is detected.
//! - Only applies the compressed output when it is strictly smaller than
//!   the original (never produces a larger message).

use openproxy_types::OpenAIMessage;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::sync::LazyLock;

type Messages = Vec<OpenAIMessage>;

const MAX_HUNKS_PER_FILE: usize = 10;
const MAX_CONTEXT_LINES: usize = 2;
const MAX_FILES: usize = 20;
const MIN_DIFF_LINES: usize = 30;

/// Technique name returned when compression applies.
pub const TECHNIQUE: &str = "lite::diff_compressor";

/// Strict hunk header regex (for detection): `^@@ -\d+,\d+ \+\d+,\d+ @@`.
pub(crate) static HUNK_HEADER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^@@ -\d+,\d+ \+\d+,\d+ @@").expect("valid regex"));

/// Lenient hunk header regex (for parsing): allows optional counts
/// (e.g. `@@ -1 +1 @@` for single-line hunks).
static HUNK_HEADER_LENIENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^@@ -\d+(?:,\d+)? \+\d+(?:,\d+)? @@").expect("valid regex"));

/// A single parsed diff file.
struct DiffFile<'a> {
    /// `diff --git a/foo b/foo` (empty for synthetic files w/o a header).
    header: &'a str,
    /// `index`, `old mode`, `new mode`, `---`, `+++`, etc.
    metadata: Box<[&'a str]>,
    hunks: Box<[Hunk<'a>]>,
}

/// A single hunk within a diff file.
struct Hunk<'a> {
    /// `@@ -10,5 +10,7 @@`
    header: &'a str,
    lines: Box<[DiffLine<'a>]>,
}

struct DiffFileBuilder<'a> {
    header: &'a str,
    metadata: Vec<&'a str>,
    hunks: Vec<Hunk<'a>>,
}

impl<'a> DiffFileBuilder<'a> {
    fn new(header: &'a str) -> Self {
        Self {
            header,
            metadata: Vec::with_capacity(4),
            hunks: Vec::with_capacity(4),
        }
    }

    fn freeze(self) -> DiffFile<'a> {
        DiffFile {
            header: self.header,
            metadata: self.metadata.into_boxed_slice(),
            hunks: self.hunks.into_boxed_slice(),
        }
    }
}

struct HunkBuilder<'a> {
    header: &'a str,
    lines: Vec<DiffLine<'a>>,
}

impl<'a> HunkBuilder<'a> {
    fn new(header: &'a str) -> Self {
        Self {
            header,
            lines: Vec::with_capacity(16),
        }
    }

    fn freeze(self) -> Hunk<'a> {
        Hunk {
            header: self.header,
            lines: self.lines.into_boxed_slice(),
        }
    }
}

impl Hunk<'_> {
    /// Returns true if the hunk contains any addition or deletion lines.
    fn has_changes(&self) -> bool {
        self.lines
            .iter()
            .any(|l| matches!(l, DiffLine::Addition(_) | DiffLine::Deletion(_)))
    }
}

/// A single line within a hunk.
enum DiffLine<'a> {
    /// Starts with " ".
    Context(&'a str),
    /// Starts with "+".
    Addition(&'a str),
    /// Starts with "-".
    Deletion(&'a str),
    /// "\ No newline at end of file" etc.
    Other(&'a str),
}

impl<'a> DiffLine<'a> {
    fn as_str(&self) -> &'a str {
        match self {
            DiffLine::Context(s)
            | DiffLine::Addition(s)
            | DiffLine::Deletion(s)
            | DiffLine::Other(s) => s,
        }
    }

    fn is_context(&self) -> bool {
        matches!(self, DiffLine::Context(_))
    }
}

/// Compresses git diff output in tool results and assistant messages.
///
/// Detects `diff --git` or `@@` hunk headers and caps hunks/context/files.
/// Returns the technique name once per message that was actually compressed.
pub fn compress_diffs(msgs: &mut Messages) -> Vec<&'static str> {
    let mut applied = Vec::new();
    for msg in msgs.iter_mut() {
        // Only tool results and assistant messages can contain diff output.
        if msg.role != "tool" && msg.role != "assistant" {
            continue;
        }
        let Some(text) = msg.content.as_ref().and_then(|c| c.as_str()) else {
            continue;
        };
        if let Some(compressed) = compress_diff_content(text) {
            msg.content = Some(Value::String(compressed));
            applied.push(TECHNIQUE);
        }
    }
    applied
}

/// Compress a single diff content string. Returns `Some((compressed, technique))`
/// if compression applied, or `None` otherwise.
///
/// This is the per-string entry point that powers the content router. It
/// delegates to the private `compress_diff_content` (which already enforces
/// the `MIN_DIFF_LINES` floor, the git-diff shape check, and the
/// "strictly smaller than input" guard) and tags the result with
/// [`TECHNIQUE`].
pub fn compress_diff_string(text: &str) -> Option<(String, &'static str)> {
    compress_diff_content(text).map(|c| (c, TECHNIQUE))
}

/// Compress a single content string. Returns `None` if not compressible
/// (too short, not a diff, nothing to compress, or compressed output is not
/// strictly smaller than the original).
fn compress_diff_content(text: &str) -> Option<String> {
    let lines: Box<[&str]> = text.lines().collect();
    if lines.len() < MIN_DIFF_LINES || !is_git_diff(&lines) {
        return None;
    }
    let files = parse_diff(&lines);
    if files.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let _ = writeln!(out, "[#diff_compressed: was {} lines]", lines.len());
    let header_len = out.len();
    compress_files(&files, &mut out);
    if out.len() == header_len || out.len() >= text.len() {
        None
    } else {
        Some(out)
    }
}

/// Detect git diff format: first 10 lines contain `diff --git` OR a
/// `@@ -a,b +c,d @@` hunk header.
fn is_git_diff(lines: &[&str]) -> bool {
    lines
        .iter()
        .take(10)
        .any(|l| l.starts_with("diff --git ") || HUNK_HEADER_RE.is_match(l))
}

fn handle_diff_git_header<'a>(
    line: &'a str,
    current_hunk: &mut Option<HunkBuilder<'a>>,
    current_file: &mut Option<DiffFileBuilder<'a>>,
    files: &mut Vec<DiffFile<'a>>,
) {
    attach_hunk(current_hunk, current_file);
    if let Some(f) = current_file.take() {
        files.push(f.freeze());
    }
    *current_file = Some(DiffFileBuilder::new(line));
}

fn handle_hunk_header<'a>(
    line: &'a str,
    current_hunk: &mut Option<HunkBuilder<'a>>,
    current_file: &mut Option<DiffFileBuilder<'a>>,
) {
    attach_hunk(current_hunk, current_file);
    if current_file.is_none() {
        *current_file = Some(DiffFileBuilder::new(""));
    }
    *current_hunk = Some(HunkBuilder::new(line));
}

fn handle_diff_body_line<'a>(
    line: &'a str,
    current_hunk: &mut Option<HunkBuilder<'a>>,
    current_file: &mut Option<DiffFileBuilder<'a>>,
) {
    if let Some(h) = current_hunk.as_mut() {
        h.lines.push(classify_diff_line(line));
    } else if let Some(f) = current_file.as_mut() {
        f.metadata.push(line);
    }
}

/// Parse a git diff into a list of files.
///
/// Lines before the first `diff --git` (orphan lines) are dropped. If a
/// `@@` hunk header appears without a current file (e.g. a diff fragment
/// with no `diff --git` prefix), a synthetic file with an empty header is
/// created to hold it.
fn parse_diff<'a>(lines: &[&'a str]) -> Box<[DiffFile<'a>]> {
    let mut files: Vec<DiffFile<'a>> = Vec::with_capacity((lines.len() / 10).clamp(1, MAX_FILES));
    let mut current_file: Option<DiffFileBuilder<'a>> = None;
    let mut current_hunk: Option<HunkBuilder<'a>> = None;

    for &line in lines {
        if line.starts_with("diff --git ") {
            handle_diff_git_header(line, &mut current_hunk, &mut current_file, &mut files);
        } else if HUNK_HEADER_LENIENT_RE.is_match(line) {
            handle_hunk_header(line, &mut current_hunk, &mut current_file);
        } else {
            handle_diff_body_line(line, &mut current_hunk, &mut current_file);
        }
    }

    // Finalize trailing hunk and file.
    attach_hunk(&mut current_hunk, &mut current_file);
    if let Some(f) = current_file.take() {
        files.push(f.freeze());
    }

    files.into_boxed_slice()
}

/// Attach the current hunk to the current file (if both exist), clearing
/// the hunk slot.
fn attach_hunk<'a>(hunk: &mut Option<HunkBuilder<'a>>, file: &mut Option<DiffFileBuilder<'a>>) {
    if let (Some(h), Some(f)) = (hunk.take(), file.as_mut()) {
        f.hunks.push(h.freeze());
    }
}

/// Classify a hunk body line by its prefix.
fn classify_diff_line(line: &str) -> DiffLine<'_> {
    if line.starts_with(' ') {
        DiffLine::Context(line)
    } else if line.starts_with('+') {
        DiffLine::Addition(line)
    } else if line.starts_with('-') {
        DiffLine::Deletion(line)
    } else {
        DiffLine::Other(line)
    }
}

fn render_file_hunks(hunks: &[Hunk<'_>], out: &mut String) {
    let (kept_hunks, truncated_hunks) = cap_hunks(hunks);
    for hunk in &kept_hunks {
        let before_hunk = out.len();
        out.push_str(hunk.header);
        out.push('\n');
        let header_end = out.len();
        render_reduced_context(&hunk.lines, out);
        if out.len() == header_end {
            out.truncate(before_hunk);
        }
    }
    if truncated_hunks > 0 {
        let _ = writeln!(out, "[#diff: {truncated_hunks} more hunks in this file]");
    }
}

fn render_diff_file(file: &DiffFile<'_>, out: &mut String) {
    if !file.header.is_empty() {
        out.push_str(file.header);
        out.push('\n');
    }
    for m in &file.metadata {
        out.push_str(m);
        out.push('\n');
    }
    render_file_hunks(&file.hunks, out);
}

/// Compress a parsed diff into the destination string buffer.
fn compress_files(files: &[DiffFile<'_>], out: &mut String) {
    let (kept_files, truncated_files) = cap_files(files);
    for file in kept_files {
        render_diff_file(file, out);
    }
    if truncated_files > 0 {
        let _ = writeln!(out, "[#diff: truncated {truncated_files} more files]");
    }
}

/// Cap files at MAX_FILES. If more, keep first (MAX_FILES - 1) + marker.
fn cap_files<'a>(files: &'a [DiffFile<'a>]) -> (&'a [DiffFile<'a>], usize) {
    if files.len() <= MAX_FILES {
        (files, 0)
    } else {
        (&files[..MAX_FILES - 1], files.len() - (MAX_FILES - 1))
    }
}

fn collect_hunk_indices(hunks: &[Hunk<'_>]) -> Box<[usize]> {
    let mut kept_indices: Vec<usize> = Vec::with_capacity(MAX_HUNKS_PER_FILE.min(hunks.len()));
    for (i, hunk) in hunks.iter().enumerate() {
        if hunk.has_changes() && kept_indices.len() < MAX_HUNKS_PER_FILE {
            kept_indices.push(i);
        }
    }
    if kept_indices.len() < MAX_HUNKS_PER_FILE {
        for (i, hunk) in hunks.iter().enumerate() {
            if !hunk.has_changes() && kept_indices.len() < MAX_HUNKS_PER_FILE {
                kept_indices.push(i);
            }
        }
    }
    kept_indices.sort_unstable();
    kept_indices.into_boxed_slice()
}

/// Cap hunks at MAX_HUNKS_PER_FILE. Prefer hunks with additions/deletions;
/// fill remaining slots with no-change hunks. Preserves original order.
fn cap_hunks<'b, 'a>(hunks: &'b [Hunk<'a>]) -> (Box<[&'b Hunk<'a>]>, usize) {
    if hunks.len() <= MAX_HUNKS_PER_FILE {
        return (hunks.iter().collect(), 0);
    }
    let kept_indices = collect_hunk_indices(hunks);
    let truncated = hunks.len() - kept_indices.len();
    let kept: Box<[&'b Hunk<'a>]> = kept_indices
        .iter()
        .filter_map(|&i| hunks.get(i))
        .collect();
    (kept, truncated)
}

/// Mark contiguous change block (non-context lines) as kept.
fn mark_change_block(lines: &[DiffLine<'_>], keep: &mut [bool], mut i: usize) -> usize {
    while i < lines.len() && !lines[i].is_context() {
        keep[i] = true;
        i += 1;
    }
    i
}

/// Mark up to MAX_CONTEXT_LINES context lines immediately before block_start.
fn mark_context_before(lines: &[DiffLine<'_>], keep: &mut [bool], mut j: usize) {
    let mut count = 0;
    while j > 0 && count < MAX_CONTEXT_LINES {
        j -= 1;
        if !lines[j].is_context() {
            break;
        }
        keep[j] = true;
        count += 1;
    }
}

/// Mark up to MAX_CONTEXT_LINES context lines immediately after a change block.
fn mark_context_after(lines: &[DiffLine<'_>], keep: &mut [bool], mut i: usize) -> usize {
    let mut count = 0;
    while i < lines.len() && count < MAX_CONTEXT_LINES {
        if !lines[i].is_context() {
            break;
        }
        keep[i] = true;
        count += 1;
        i += 1;
    }
    i
}

fn mark_context_lines(lines: &[DiffLine<'_>], keep: &mut [bool]) {
    let mut i = 0;
    while i < lines.len() {
        if lines[i].is_context() {
            i += 1;
            continue;
        }
        let block_start = i;
        i = mark_change_block(lines, keep, i);
        mark_context_before(lines, keep, block_start);
        i = mark_context_after(lines, keep, i);
    }
}

/// Reduce context lines and render directly into the destination string.
///
/// Keeps only MAX_CONTEXT_LINES context lines immediately before and after each
/// change block. Always keeps all additions, deletions, and "other" lines.
fn render_reduced_context(lines: &[DiffLine<'_>], out: &mut String) {
    if lines.is_empty() {
        return;
    }
    if lines.len() <= 128 {
        let mut keep = [false; 128];
        let keep_slice = &mut keep[..lines.len()];
        mark_context_lines(lines, keep_slice);
        for (line, &should_keep) in lines.iter().zip(keep_slice.iter()) {
            if should_keep {
                out.push_str(line.as_str());
                out.push('\n');
            }
        }
    } else {
        let mut keep = vec![false; lines.len()].into_boxed_slice();
        mark_context_lines(lines, &mut keep);
        for (line, &should_keep) in lines.iter().zip(keep.iter()) {
            if should_keep {
                out.push_str(line.as_str());
                out.push('\n');
            }
        }
    }
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

    fn count_lines_starting_with(text: &str, prefix: char) -> usize {
        text.lines().filter(|l| l.starts_with(prefix)).count()
    }

    fn count_substring(text: &str, needle: &str) -> usize {
        text.matches(needle).count()
    }

    /// Build a ~49-line diff with 5 hunks, each with 3 context + 1 del + 1 add + 3 context.
    fn make_basic_diff() -> String {
        let mut lines: Vec<String> = vec![
            "diff --git a/foo.rs b/foo.rs".to_string(),
            "index abc..def 100644".to_string(),
            "--- a/foo.rs".to_string(),
            "+++ b/foo.rs".to_string(),
        ];
        for h in 0..5u32 {
            let base = (h * 10 + 1) as usize;
            lines.push(format!("@@ -{base},8 +{base},8 @@"));
            for c in 0..3u32 {
                lines.push(format!(" context_{h}_{c}"));
            }
            lines.push(format!("-old_line_{h}"));
            lines.push(format!("+new_line_{h}"));
            for c in 0..3u32 {
                lines.push(format!(" context_{}_{}", h, c + 3));
            }
        }
        lines.join("\n")
    }

    #[test]
    fn test_compress_diff_basic() {
        let content = make_basic_diff();
        assert!(
            content.lines().count() >= MIN_DIFF_LINES,
            "basic diff should have >= {} lines, got {}",
            MIN_DIFF_LINES,
            content.lines().count()
        );
        let original_context = count_lines_starting_with(&content, ' ');
        let mut msgs = vec![msg("tool", &content)];
        let applied = compress_diffs(&mut msgs);
        assert!(
            applied.contains(&TECHNIQUE),
            "should compress basic diff, got: {applied:?}"
        );
        let compressed = msgs[0].content.as_ref().unwrap().as_str().unwrap();
        assert!(
            compressed.starts_with("[#diff_compressed: was "),
            "should have header, got: {:?}",
            compressed.get(..40)
        );
        let compressed_context = count_lines_starting_with(compressed, ' ');
        assert!(
            compressed_context < original_context,
            "context should be reduced: {compressed_context} < {original_context}"
        );
        assert!(
            compressed.len() < content.len(),
            "compressed should be smaller: {} < {}",
            compressed.len(),
            content.len()
        );
        // All additions and deletions preserved.
        for h in 0..5u32 {
            assert!(
                compressed.contains(&format!("-old_line_{h}")),
                "deletion {h} should be preserved"
            );
            assert!(
                compressed.contains(&format!("+new_line_{h}")),
                "addition {h} should be preserved"
            );
        }
    }

    #[test]
    fn test_compress_diff_caps_hunks() {
        // 1 file with 15 hunks, each with a change. Only 10 should be kept.
        let mut lines: Vec<String> = vec![
            "diff --git a/foo.rs b/foo.rs".to_string(),
            "index abc..def 100644".to_string(),
            "--- a/foo.rs".to_string(),
            "+++ b/foo.rs".to_string(),
        ];
        for h in 0..15u32 {
            let base = (h * 5 + 1) as usize;
            lines.push(format!("@@ -{base},3 +{base},3 @@"));
            lines.push(format!(" ctx_{h}"));
            lines.push(format!("-old_{h}"));
            lines.push(format!("+new_{h}"));
        }
        let content = lines.join("\n");
        let mut msgs = vec![msg("tool", &content)];
        let applied = compress_diffs(&mut msgs);
        assert!(
            applied.contains(&TECHNIQUE),
            "should compress, got: {applied:?}"
        );
        let compressed = msgs[0].content.as_ref().unwrap().as_str().unwrap();
        assert!(
            compressed.contains("[#diff: 5 more hunks in this file]"),
            "should have hunks truncation marker, got: {compressed}"
        );
        let hunk_count = count_substring(compressed, "@@ -");
        assert_eq!(
            hunk_count, 10,
            "should keep exactly 10 hunks, got {hunk_count}"
        );
    }

    #[test]
    fn test_compress_diff_caps_files() {
        // 25 files, each with 1 hunk. Only 19 + marker should be kept.
        let mut lines: Vec<String> = Vec::new();
        for f in 0..25u32 {
            lines.push(format!("diff --git a/f{f}.rs b/f{f}.rs"));
            lines.push("index abc..def 100644".to_string());
            lines.push(format!("--- a/f{f}.rs"));
            lines.push(format!("+++ b/f{f}.rs"));
            lines.push("@@ -1,3 +1,3 @@".to_string());
            lines.push(format!(" ctx_{f}"));
            lines.push(format!("-old_{f}"));
            lines.push(format!("+new_{f}"));
        }
        let content = lines.join("\n");
        let mut msgs = vec![msg("tool", &content)];
        let applied = compress_diffs(&mut msgs);
        assert!(
            applied.contains(&TECHNIQUE),
            "should compress, got: {applied:?}"
        );
        let compressed = msgs[0].content.as_ref().unwrap().as_str().unwrap();
        assert!(
            compressed.contains("[#diff: truncated 6 more files]"),
            "should have files truncation marker, got: {compressed}"
        );
        let file_count = count_substring(compressed, "diff --git ");
        assert_eq!(
            file_count, 19,
            "should keep exactly 19 files, got {file_count}"
        );
    }

    fn build_test_hunk_lines(header: &str, prefix: &str, tag: &str) -> Vec<String> {
        let mut lines = vec![header.to_string()];
        for i in 0..5u32 {
            lines.push(format!(" {prefix}_before_{i}"));
        }
        lines.push(format!("-del{tag}"));
        lines.push(format!("+add{tag}"));
        for i in 0..5u32 {
            lines.push(format!(" {prefix}_after_{i}"));
        }
        lines
    }

    #[test]
    fn test_compress_diff_preserves_additions_deletions() {
        let mut lines: Vec<String> = vec![
            "diff --git a/foo.rs b/foo.rs".to_string(),
            "index abc..def 100644".to_string(),
            "--- a/foo.rs".to_string(),
            "+++ b/foo.rs".to_string(),
        ];
        lines.extend(build_test_hunk_lines("@@ -1,12 +1,12 @@", "ctx", "1"));
        lines.extend(build_test_hunk_lines("@@ -20,12 +20,12 @@", "ctx2", "2"));
        let content = lines.join("\n");
        // 4 metadata + (1 header + 12 body) * 2 = 4 + 26 = 30 lines.
        assert_eq!(lines.len(), 30);
        let mut msgs = vec![msg("tool", &content)];
        let applied = compress_diffs(&mut msgs);
        assert!(
            applied.contains(&TECHNIQUE),
            "should compress, got: {applied:?}"
        );
        let compressed = msgs[0].content.as_ref().unwrap().as_str().unwrap();
        assert!(
            compressed.contains("-del1"),
            "deletion del1 should be preserved"
        );
        assert!(
            compressed.contains("+add1"),
            "addition add1 should be preserved"
        );
        assert!(
            compressed.contains("-del2"),
            "deletion del2 should be preserved"
        );
        assert!(
            compressed.contains("+add2"),
            "addition add2 should be preserved"
        );
    }

    #[test]
    fn test_compress_diff_skips_non_diff() {
        let mut lines: Vec<String> = Vec::new();
        for i in 0..50u32 {
            lines.push(format!("This is plain text line {i}"));
        }
        let content = lines.join("\n");
        let mut msgs = vec![msg("tool", &content)];
        let applied = compress_diffs(&mut msgs);
        assert!(
            applied.is_empty(),
            "should not compress plain text, got: {applied:?}"
        );
        let after = msgs[0].content.as_ref().unwrap().as_str().unwrap();
        assert_eq!(after, content, "content should be unchanged");
    }

    #[test]
    fn test_compress_diff_skips_short_diff() {
        let content = "diff --git a/foo.rs b/foo.rs\n\
index abc..def 100644\n\
--- a/foo.rs\n\
+++ b/foo.rs\n\
@@ -1,3 +1,3 @@\n\
 ctx1\n\
-old\n\
+new\n\
 ctx2\n\
 ctx3";
        // 10 lines — below MIN_DIFF_LINES (30).
        assert_eq!(content.lines().count(), 10);
        let mut msgs = vec![msg("tool", content)];
        let applied = compress_diffs(&mut msgs);
        assert!(
            applied.is_empty(),
            "should not compress short diff, got: {applied:?}"
        );
        let after = msgs[0].content.as_ref().unwrap().as_str().unwrap();
        assert_eq!(after, content, "content should be unchanged");
    }

    #[test]
    fn test_compress_diff_never_produces_larger_output() {
        // 31-line diff with no context (all +/- lines) — nothing to compress,
        // so the compressed output (header + same body) would be larger.
        let mut lines: Vec<String> = vec![
            "diff --git a/foo.rs b/foo.rs".to_string(),
            "index abc..def 100644".to_string(),
            "--- a/foo.rs".to_string(),
            "+++ b/foo.rs".to_string(),
            "@@ -1,13 +1,13 @@".to_string(),
        ];
        for i in 0..13u32 {
            lines.push(format!("-old_{i}"));
            lines.push(format!("+new_{i}"));
        }
        let content = lines.join("\n");
        // 5 header + 26 body = 31 lines.
        assert_eq!(lines.len(), 31);
        let mut msgs = vec![msg("tool", &content)];
        let applied = compress_diffs(&mut msgs);
        assert!(
            applied.is_empty(),
            "should not produce larger output, got: {applied:?}"
        );
        let after = msgs[0].content.as_ref().unwrap().as_str().unwrap();
        assert_eq!(after, content, "content should be unchanged");
    }

    #[test]
    fn test_compress_diff_skips_system_and_user_messages() {
        // Even with a valid diff, system/user messages should not be touched.
        let content = make_basic_diff();
        let mut msgs = vec![msg("system", &content), msg("user", &content)];
        let applied = compress_diffs(&mut msgs);
        assert!(
            applied.is_empty(),
            "should not compress system/user messages, got: {applied:?}"
        );
        for m in &msgs {
            let after = m.content.as_ref().unwrap().as_str().unwrap();
            assert_eq!(after, content, "system/user content should be unchanged");
        }
    }

    #[test]
    fn test_compress_diff_processes_assistant_messages() {
        let content = make_basic_diff();
        let mut msgs = vec![msg("assistant", &content)];
        let applied = compress_diffs(&mut msgs);
        assert!(
            applied.contains(&TECHNIQUE),
            "should compress assistant messages, got: {applied:?}"
        );
    }
}
