use super::*;
use serde_json::Value;

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

fn make_basic_diff() -> String {
    let mut lines = vec![
        "diff --git a/foo.rs b/foo.rs".into(),
        "index abc..def 100644".into(),
        "--- a/foo.rs".into(),
        "+++ b/foo.rs".into(),
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
            lines.push(format!(" context_{h}_{}", c + 3));
        }
    }
    lines.join("\n")
}

#[test]
fn test_compress_diff_basic() {
    let content = make_basic_diff();
    assert!(content.lines().count() >= MIN_DIFF_LINES);
    let original_context = count_lines_starting_with(&content, ' ');
    let mut msgs = vec![msg("tool", &content)];
    let applied = compress_diffs(&mut msgs);
    assert!(applied.contains(&TECHNIQUE));
    let compressed = msgs[0].content.as_ref().unwrap().as_str().unwrap();
    assert!(compressed.starts_with("[#diff_compressed: was "));
    assert!(count_lines_starting_with(compressed, ' ') < original_context);
    assert!(compressed.len() < content.len());
    for h in 0..5u32 {
        assert!(compressed.contains(&format!("-old_line_{h}")));
        assert!(compressed.contains(&format!("+new_line_{h}")));
    }
}

#[test]
fn test_compress_diff_caps_hunks() {
    let mut lines = vec![
        "diff --git a/foo.rs b/foo.rs".into(),
        "index abc..def 100644".into(),
        "--- a/foo.rs".into(),
        "+++ b/foo.rs".into(),
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
    assert!(applied.contains(&TECHNIQUE));
    let compressed = msgs[0].content.as_ref().unwrap().as_str().unwrap();
    assert!(compressed.contains("[#diff: 5 more hunks in this file]"));
    assert_eq!(count_substring(compressed, "@@ -"), 10);
}

#[test]
fn test_compress_diff_caps_files() {
    let mut lines = Vec::new();
    for f in 0..25u32 {
        lines.push(format!("diff --git a/f{f}.rs b/f{f}.rs"));
        lines.push("index abc..def 100644".into());
        lines.push(format!("--- a/f{f}.rs"));
        lines.push(format!("+++ b/f{f}.rs"));
        lines.push("@@ -1,3 +1,3 @@".into());
        lines.push(format!(" ctx_{f}"));
        lines.push(format!("-old_{f}"));
        lines.push(format!("+new_{f}"));
    }
    let content = lines.join("\n");
    let mut msgs = vec![msg("tool", &content)];
    let applied = compress_diffs(&mut msgs);
    assert!(applied.contains(&TECHNIQUE));
    let compressed = msgs[0].content.as_ref().unwrap().as_str().unwrap();
    assert!(compressed.contains("[#diff: truncated 6 more files]"));
    assert_eq!(count_substring(compressed, "diff --git "), 19);
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
    let mut lines = vec![
        "diff --git a/foo.rs b/foo.rs".into(),
        "index abc..def 100644".into(),
        "--- a/foo.rs".into(),
        "+++ b/foo.rs".into(),
    ];
    lines.extend(build_test_hunk_lines("@@ -1,12 +1,12 @@", "ctx", "1"));
    lines.extend(build_test_hunk_lines("@@ -20,12 +20,12 @@", "ctx2", "2"));
    let content = lines.join("\n");
    assert_eq!(lines.len(), 30);
    let mut msgs = vec![msg("tool", &content)];
    let applied = compress_diffs(&mut msgs);
    assert!(applied.contains(&TECHNIQUE));
    let compressed = msgs[0].content.as_ref().unwrap().as_str().unwrap();
    assert!(compressed.contains("-del1") && compressed.contains("+add1"));
    assert!(compressed.contains("-del2") && compressed.contains("+add2"));
}

#[test]
fn test_compress_diff_skips_non_diff() {
    let content = (0..50)
        .map(|i| format!("This is plain text line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut msgs = vec![msg("tool", &content)];
    assert!(compress_diffs(&mut msgs).is_empty());
    assert_eq!(msgs[0].content.as_ref().unwrap().as_str().unwrap(), content);
}

#[test]
fn test_compress_diff_skips_short_diff() {
    let content = "diff --git a/foo.rs b/foo.rs\nindex abc..def 100644\n--- a/foo.rs\n+++ b/foo.rs\n@@ -1,3 +1,3 @@\n ctx1\n-old\n+new\n ctx2\n ctx3";
    let mut msgs = vec![msg("tool", content)];
    assert!(compress_diffs(&mut msgs).is_empty());
    assert_eq!(msgs[0].content.as_ref().unwrap().as_str().unwrap(), content);
}

#[test]
fn test_compress_diff_never_produces_larger_output() {
    let mut lines = vec![
        "diff --git a/foo.rs b/foo.rs".into(),
        "index abc..def 100644".into(),
        "--- a/foo.rs".into(),
        "+++ b/foo.rs".into(),
        "@@ -1,13 +1,13 @@".into(),
    ];
    for i in 0..13u32 {
        lines.push(format!("-old_{i}"));
        lines.push(format!("+new_{i}"));
    }
    let content = lines.join("\n");
    let mut msgs = vec![msg("tool", &content)];
    assert!(compress_diffs(&mut msgs).is_empty());
    assert_eq!(msgs[0].content.as_ref().unwrap().as_str().unwrap(), content);
}

#[test]
fn test_compress_diff_skips_system_and_user_messages() {
    let content = make_basic_diff();
    let mut msgs = vec![msg("system", &content), msg("user", &content)];
    assert!(compress_diffs(&mut msgs).is_empty());
    for m in &msgs {
        assert_eq!(m.content.as_ref().unwrap().as_str().unwrap(), content);
    }
}

#[test]
fn test_compress_diff_processes_assistant_messages() {
    let content = make_basic_diff();
    let mut msgs = vec![msg("assistant", &content)];
    assert!(compress_diffs(&mut msgs).contains(&TECHNIQUE));
}

#[test]
fn test_compress_diff_string() {
    let content = make_basic_diff();
    let (compressed, tech) = compress_diff_string(&content).unwrap();
    assert_eq!(tech, TECHNIQUE);
    assert!(compressed.starts_with("[#diff_compressed: was "));
}
