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

use crate::translation::OpenAIMessage;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

type Messages = Vec<OpenAIMessage>;

const MAX_HUNKS_PER_FILE: usize = 10;
const MAX_CONTEXT_LINES: usize = 2;
const MAX_FILES: usize = 20;
const MIN_DIFF_LINES: usize = 30;

/// Technique name returned when compression applies.
pub const TECHNIQUE: &str = "lite::diff_compressor";

/// Strict hunk header regex (for detection): `^@@ -\d+,\d+ \+\d+,\d+ @@`.
static HUNK_HEADER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^@@ -\d+,\d+ \+\d+,\d+ @@").unwrap());

/// Lenient hunk header regex (for parsing): allows optional counts
/// (e.g. `@@ -1 +1 @@` for single-line hunks).
static HUNK_HEADER_LENIENT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^@@ -\d+(?:,\d+)? \+\d+(?:,\d+)? @@").unwrap());

/// A single parsed diff file.
struct DiffFile {
    /// `diff --git a/foo b/foo` (empty for synthetic files w/o a header).
    header: String,
    /// `index`, `old mode`, `new mode`, `---`, `+++`, etc.
    metadata: Vec<String>,
    hunks: Vec<Hunk>,
}

/// A single hunk within a diff file.
struct Hunk {
    /// `@@ -10,5 +10,7 @@`
    header: String,
    lines: Vec<DiffLine>,
}

impl Hunk {
    /// Returns true if the hunk contains any addition or deletion lines.
    fn has_changes(&self) -> bool {
        self.lines
            .iter()
            .any(|l| matches!(l, DiffLine::Addition(_) | DiffLine::Deletion(_)))
    }
}

/// A single line within a hunk.
enum DiffLine {
    /// Starts with " ".
    Context(String),
    /// Starts with "+".
    Addition(String),
    /// Starts with "-".
    Deletion(String),
    /// "\ No newline at end of file" etc.
    Other(String),
}

impl DiffLine {
    fn as_str(&self) -> &str {
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
        // Take ownership of the text so we can rebind `msg.content` afterwards
        // without a dangling borrow.
        let text = match msg.content.as_ref().and_then(|c| c.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if let Some(compressed) = compress_diff_content(&text) {
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
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() < MIN_DIFF_LINES {
        return None;
    }
    if !is_git_diff(&lines) {
        return None;
    }
    let files = parse_diff(&lines);
    if files.is_empty() {
        return None;
    }
    let original_lines = lines.len();
    let compressed_body = compress_files(&files);
    if compressed_body.is_empty() {
        return None;
    }
    let output = format!(
        "[#diff_compressed: was {} lines]\n{}",
        original_lines, compressed_body
    );
    // Never produce a larger message.
    if output.len() >= text.len() {
        return None;
    }
    Some(output)
}

/// Detect git diff format: first 10 lines contain `diff --git` OR a
/// `@@ -a,b +c,d @@` hunk header.
fn is_git_diff(lines: &[&str]) -> bool {
    let mut has_diff_git = false;
    let mut has_hunk_header = false;
    for l in lines.iter().take(10) {
        if l.starts_with("diff --git ") {
            has_diff_git = true;
        }
        if HUNK_HEADER_RE.is_match(l) {
            has_hunk_header = true;
        }
    }
    has_diff_git || has_hunk_header
}

/// Parse a git diff into a list of files.
///
/// Lines before the first `diff --git` (orphan lines) are dropped. If a
/// `@@` hunk header appears without a current file (e.g. a diff fragment
/// with no `diff --git` prefix), a synthetic file with an empty header is
/// created to hold it.
fn parse_diff(lines: &[&str]) -> Vec<DiffFile> {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut current_file: Option<DiffFile> = None;
    let mut current_hunk: Option<Hunk> = None;

    for line in lines {
        if line.starts_with("diff --git ") {
            // Finalize current hunk and file.
            attach_hunk(&mut current_hunk, &mut current_file);
            if let Some(f) = current_file.take() {
                files.push(f);
            }
            current_file = Some(DiffFile {
                header: line.to_string(),
                metadata: Vec::new(),
                hunks: Vec::new(),
            });
            continue;
        }

        if HUNK_HEADER_LENIENT_RE.is_match(line) {
            // Finalize current hunk.
            attach_hunk(&mut current_hunk, &mut current_file);
            // Synthetic file for diff fragments without `diff --git`.
            if current_file.is_none() {
                current_file = Some(DiffFile {
                    header: String::new(),
                    metadata: Vec::new(),
                    hunks: Vec::new(),
                });
            }
            current_hunk = Some(Hunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
            continue;
        }

        if let Some(h) = current_hunk.as_mut() {
            h.lines.push(classify_diff_line(line));
        } else if let Some(f) = current_file.as_mut() {
            f.metadata.push(line.to_string());
        }
        // Orphan lines (before any `diff --git` or `@@`) are dropped.
    }

    // Finalize trailing hunk and file.
    attach_hunk(&mut current_hunk, &mut current_file);
    if let Some(f) = current_file.take() {
        files.push(f);
    }

    files
}

/// Attach the current hunk to the current file (if both exist), clearing
/// the hunk slot.
fn attach_hunk(hunk: &mut Option<Hunk>, file: &mut Option<DiffFile>) {
    if let (Some(h), Some(f)) = (hunk.take(), file.as_mut()) {
        f.hunks.push(h);
    }
}

/// Classify a hunk body line by its prefix.
fn classify_diff_line(line: &str) -> DiffLine {
    if line.starts_with(' ') {
        DiffLine::Context(line.to_string())
    } else if line.starts_with('+') {
        DiffLine::Addition(line.to_string())
    } else if line.starts_with('-') {
        DiffLine::Deletion(line.to_string())
    } else {
        DiffLine::Other(line.to_string())
    }
}

/// Compress a parsed diff back into a string.
fn compress_files(files: &[DiffFile]) -> String {
    let (kept_files, truncated_files) = cap_files(files);
    let mut out = String::new();
    for file in kept_files {
        if !file.header.is_empty() {
            out.push_str(&file.header);
            out.push('\n');
        }
        for m in &file.metadata {
            out.push_str(m);
            out.push('\n');
        }
        let (kept_hunks, truncated_hunks) = cap_hunks(&file.hunks);
        for hunk in kept_hunks {
            let reduced = reduce_context(&hunk.lines);
            if reduced.is_empty() {
                // Hunk had no keepable lines — skip its header too.
                continue;
            }
            out.push_str(&hunk.header);
            out.push('\n');
            for line in reduced {
                out.push_str(&line);
                out.push('\n');
            }
        }
        if truncated_hunks > 0 {
            out.push_str(&format!(
                "[#diff: {} more hunks in this file]\n",
                truncated_hunks
            ));
        }
    }
    if truncated_files > 0 {
        out.push_str(&format!(
            "[#diff: truncated {} more files]\n",
            truncated_files
        ));
    }
    out
}

/// Cap files at MAX_FILES. If more, keep first (MAX_FILES - 1) + marker.
fn cap_files(files: &[DiffFile]) -> (Vec<&DiffFile>, usize) {
    if files.len() <= MAX_FILES {
        return (files.iter().collect(), 0);
    }
    let kept: Vec<&DiffFile> = files.iter().take(MAX_FILES - 1).collect();
    let truncated = files.len() - (MAX_FILES - 1);
    (kept, truncated)
}

/// Cap hunks at MAX_HUNKS_PER_FILE. Prefer hunks with additions/deletions;
/// fill remaining slots with no-change hunks. Preserves original order.
fn cap_hunks(hunks: &[Hunk]) -> (Vec<&Hunk>, usize) {
    if hunks.len() <= MAX_HUNKS_PER_FILE {
        return (hunks.iter().collect(), 0);
    }
    let has_changes: Vec<bool> = hunks.iter().map(|h| h.has_changes()).collect();
    let mut kept_indices: Vec<usize> = Vec::new();
    // First pass: hunks with changes.
    for (i, &has) in has_changes.iter().enumerate() {
        if has && kept_indices.len() < MAX_HUNKS_PER_FILE {
            kept_indices.push(i);
        }
    }
    // Second pass: fill with no-change hunks.
    for (i, &has) in has_changes.iter().enumerate() {
        if !has && kept_indices.len() < MAX_HUNKS_PER_FILE {
            kept_indices.push(i);
        }
    }
    kept_indices.sort_unstable();
    let truncated = hunks.len() - kept_indices.len();
    let kept: Vec<&Hunk> = kept_indices.into_iter().map(|i| &hunks[i]).collect();
    (kept, truncated)
}

/// Reduce context lines: keep only MAX_CONTEXT_LINES context lines
/// immediately before and after each change block. Always keep all
/// additions, deletions, and "other" lines (e.g. "\ No newline at end of
/// file").
///
/// A "change block" is a maximal contiguous run of non-context lines
/// (Addition / Deletion / Other). Context lines more than MAX_CONTEXT_LINES
/// away from every change block are dropped.
fn reduce_context(lines: &[DiffLine]) -> Vec<String> {
    let n = lines.len();
    if n == 0 {
        return Vec::new();
    }
    let mut keep = vec![false; n];
    let mut i = 0;
    while i < n {
        if lines[i].is_context() {
            i += 1;
            continue;
        }
        // Start of a change block (non-context line).
        let block_start = i;
        while i < n && !lines[i].is_context() {
            keep[i] = true;
            i += 1;
        }
        // `i` is now just past the end of the change block.
        // Keep up to MAX_CONTEXT_LINES context lines immediately before block_start.
        let mut count = 0;
        let mut j = block_start;
        while j > 0 && count < MAX_CONTEXT_LINES {
            j -= 1;
            if lines[j].is_context() {
                keep[j] = true;
                count += 1;
            } else {
                // Hit a non-context line (previous change block) — stop.
                break;
            }
        }
        // Keep up to MAX_CONTEXT_LINES context lines immediately after the block.
        let mut count = 0;
        while i < n && count < MAX_CONTEXT_LINES {
            if lines[i].is_context() {
                keep[i] = true;
                count += 1;
                i += 1;
            } else {
                // Next change block starts here — let the outer loop handle it.
                break;
            }
        }
    }

    let mut out = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if keep[idx] {
            out.push(line.as_str().to_string());
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
            extra: Default::default(),
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
            "should compress basic diff, got: {:?}",
            applied
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
            "context should be reduced: {} < {}",
            compressed_context,
            original_context
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
                compressed.contains(&format!("-old_line_{}", h)),
                "deletion {} should be preserved",
                h
            );
            assert!(
                compressed.contains(&format!("+new_line_{}", h)),
                "addition {} should be preserved",
                h
            );
        }
    }

    #[test]
    fn test_compress_diff_caps_hunks() {
        // 1 file with 15 hunks, each with a change. Only 10 should be kept.
        let mut lines: Vec<String> = Vec::new();
        lines.push("diff --git a/foo.rs b/foo.rs".to_string());
        lines.push("index abc..def 100644".to_string());
        lines.push("--- a/foo.rs".to_string());
        lines.push("+++ b/foo.rs".to_string());
        for h in 0..15u32 {
            let base = (h * 5 + 1) as usize;
            lines.push(format!("@@ -{},3 +{},3 @@", base, base));
            lines.push(format!(" ctx_{}", h));
            lines.push(format!("-old_{}", h));
            lines.push(format!("+new_{}", h));
        }
        let content = lines.join("\n");
        let mut msgs = vec![msg("tool", &content)];
        let applied = compress_diffs(&mut msgs);
        assert!(
            applied.contains(&TECHNIQUE),
            "should compress, got: {:?}",
            applied
        );
        let compressed = msgs[0].content.as_ref().unwrap().as_str().unwrap();
        assert!(
            compressed.contains("[#diff: 5 more hunks in this file]"),
            "should have hunks truncation marker, got: {}",
            compressed
        );
        let hunk_count = count_substring(compressed, "@@ -");
        assert_eq!(
            hunk_count, 10,
            "should keep exactly 10 hunks, got {}",
            hunk_count
        );
    }

    #[test]
    fn test_compress_diff_caps_files() {
        // 25 files, each with 1 hunk. Only 19 + marker should be kept.
        let mut lines: Vec<String> = Vec::new();
        for f in 0..25u32 {
            lines.push(format!("diff --git a/f{}.rs b/f{}.rs", f, f));
            lines.push("index abc..def 100644".to_string());
            lines.push(format!("--- a/f{}.rs", f));
            lines.push(format!("+++ b/f{}.rs", f));
            lines.push("@@ -1,3 +1,3 @@".to_string());
            lines.push(format!(" ctx_{}", f));
            lines.push(format!("-old_{}", f));
            lines.push(format!("+new_{}", f));
        }
        let content = lines.join("\n");
        let mut msgs = vec![msg("tool", &content)];
        let applied = compress_diffs(&mut msgs);
        assert!(
            applied.contains(&TECHNIQUE),
            "should compress, got: {:?}",
            applied
        );
        let compressed = msgs[0].content.as_ref().unwrap().as_str().unwrap();
        assert!(
            compressed.contains("[#diff: truncated 6 more files]"),
            "should have files truncation marker, got: {}",
            compressed
        );
        let file_count = count_substring(compressed, "diff --git ");
        assert_eq!(
            file_count, 19,
            "should keep exactly 19 files, got {}",
            file_count
        );
    }

    #[test]
    fn test_compress_diff_preserves_additions_deletions() {
        let mut lines: Vec<String> = Vec::new();
        lines.push("diff --git a/foo.rs b/foo.rs".to_string());
        lines.push("index abc..def 100644".to_string());
        lines.push("--- a/foo.rs".to_string());
        lines.push("+++ b/foo.rs".to_string());
        lines.push("@@ -1,12 +1,12 @@".to_string());
        for i in 0..5u32 {
            lines.push(format!(" ctx_before_{}", i));
        }
        lines.push("-del1".to_string());
        lines.push("+add1".to_string());
        for i in 0..5u32 {
            lines.push(format!(" ctx_after_{}", i));
        }
        lines.push("@@ -20,12 +20,12 @@".to_string());
        for i in 0..5u32 {
            lines.push(format!(" ctx2_before_{}", i));
        }
        lines.push("-del2".to_string());
        lines.push("+add2".to_string());
        for i in 0..5u32 {
            lines.push(format!(" ctx2_after_{}", i));
        }
        let content = lines.join("\n");
        // 4 metadata + (1 header + 12 body) * 2 = 4 + 26 = 30 lines.
        assert_eq!(lines.len(), 30);
        let mut msgs = vec![msg("tool", &content)];
        let applied = compress_diffs(&mut msgs);
        assert!(
            applied.contains(&TECHNIQUE),
            "should compress, got: {:?}",
            applied
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
            lines.push(format!("This is plain text line {}", i));
        }
        let content = lines.join("\n");
        let mut msgs = vec![msg("tool", &content)];
        let applied = compress_diffs(&mut msgs);
        assert!(
            applied.is_empty(),
            "should not compress plain text, got: {:?}",
            applied
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
            "should not compress short diff, got: {:?}",
            applied
        );
        let after = msgs[0].content.as_ref().unwrap().as_str().unwrap();
        assert_eq!(after, content, "content should be unchanged");
    }

    #[test]
    fn test_compress_diff_never_produces_larger_output() {
        // 31-line diff with no context (all +/- lines) — nothing to compress,
        // so the compressed output (header + same body) would be larger.
        let mut lines: Vec<String> = Vec::new();
        lines.push("diff --git a/foo.rs b/foo.rs".to_string());
        lines.push("index abc..def 100644".to_string());
        lines.push("--- a/foo.rs".to_string());
        lines.push("+++ b/foo.rs".to_string());
        lines.push("@@ -1,13 +1,13 @@".to_string());
        for i in 0..13u32 {
            lines.push(format!("-old_{}", i));
            lines.push(format!("+new_{}", i));
        }
        let content = lines.join("\n");
        // 5 header + 26 body = 31 lines.
        assert_eq!(lines.len(), 31);
        let mut msgs = vec![msg("tool", &content)];
        let applied = compress_diffs(&mut msgs);
        assert!(
            applied.is_empty(),
            "should not produce larger output, got: {:?}",
            applied
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
            "should not compress system/user messages, got: {:?}",
            applied
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
            "should compress assistant messages, got: {:?}",
            applied
        );
    }
}
