//! SmartCrusher: JSON array compressor inspired by headroom's SmartCrusher.
//!
//! Compresses `role == "tool"` messages whose content parses as a JSON array
//! of homogeneous objects (typical API responses, DB query results, kubectl
//! output) using one of two strategies:
//!
//! 1. **Lossless CSV-schema compaction** — if ≥80% of items share ≥80% of the
//!    union of field names, render the array as a `#schema:` header followed
//!    by one CSV row per item. Used only when it saves ≥15% of bytes.
//! 2. **Lossy fallback** — keep first 30%, last 15%, all error-tagged items,
//!    dedup identical items, cap at 15. Used only when the result is smaller
//!    than the original.
//!
//! Both strategies are skipped (no-op) if they would not reduce size, so the
//! function is safe to call unconditionally on any conversation. All ratio
//! comparisons use integer math (`count * den >= total * num`) to avoid the
//! classic `0.8 * 5 = 4.000000001` floating-point rounding trap that would
//! otherwise silently raise the coverage threshold.

use openproxy_types::OpenAIMessage;
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};
use std::fmt::Write;

type Messages = Vec<OpenAIMessage>;

/// Minimum array size to consider crushing.
const MIN_ITEMS: usize = 5;
/// Field-coverage thresholds (4/5 = 80%).
const COVERAGE_NUM: usize = 4;
const COVERAGE_DEN: usize = 5;
/// Lossless savings threshold: output must be < input * 85/100 (≥15% saved).
const LOSSLESS_KEEP_NUM: usize = 85;
const LOSSLESS_KEEP_DEN: usize = 100;
/// Lossy selection fractions: first 3/10 = 30%, last 3/20 = 15%.
const LOSSY_FIRST_NUM: usize = 3;
const LOSSY_FIRST_DEN: usize = 10;
const LOSSY_LAST_NUM: usize = 3;
const LOSSY_LAST_DEN: usize = 20;
/// Hard cap on items after lossy crush.
const LOSSY_MAX_ITEMS: usize = 15;
/// Tokens (case-insensitive substring) that mark a value as an error.
const ERROR_TOKENS: &[&str] = &["error", "fail", "fatal", "exception", "crash"];

/// Technique names returned on success.
pub const LOSSLESS_TECHNIQUE: &str = "lite::smart_crusher_lossless";
pub const LOSSY_TECHNIQUE: &str = "lite::smart_crusher_lossy";

/// Compress a single JSON array string. Returns `Some((compressed, technique))`
/// if compression applied, or `None` otherwise.
///
/// This is the per-string entry point that powers the content router. It
/// parses `text` as a JSON value, requires it to be an array of ≥
/// `MIN_ITEMS` objects, and tries the lossless CSV-schema path before
/// falling back to the lossy crush path. The output is only returned when
/// it is strictly smaller than the input (per the lossless/lossy size
/// guards), so the function is safe to call unconditionally on any string.
fn parse_object_array(text: &str) -> Option<Vec<Value>> {
    let Value::Array(arr) = serde_json::from_str::<Value>(text).ok()? else {
        return None;
    };
    if arr.len() < MIN_ITEMS || !arr.iter().all(Value::is_object) {
        return None;
    }
    Some(arr)
}

fn try_lossless_route(arr: &[Value], original_len: usize) -> Option<(String, &'static str)> {
    let out = try_lossless_csv(arr)?;
    if out.len() * LOSSLESS_KEEP_DEN < original_len * LOSSLESS_KEEP_NUM {
        Some((out, LOSSLESS_TECHNIQUE))
    } else {
        None
    }
}

fn try_lossy_route(arr: &[Value], original_len: usize) -> Option<(String, &'static str)> {
    let out = try_lossy(arr)?;
    if out.len() < original_len {
        Some((out, LOSSY_TECHNIQUE))
    } else {
        None
    }
}

/// Compress a single JSON array string. Returns `Some((compressed, technique))`
/// if compression applied, or `None` otherwise.
///
/// This is the per-string entry point that powers the content router. It
/// parses `text` as a JSON value, requires it to be an array of ≥
/// `MIN_ITEMS` objects, and tries the lossless CSV-schema path before
/// falling back to the lossy crush path. The output is only returned when
/// it is strictly smaller than the input (per the lossless/lossy size
/// guards), so the function is safe to call unconditionally on any string.
pub fn crush_json_string(text: &str) -> Option<(String, &'static str)> {
    let arr = parse_object_array(text)?;
    try_lossless_route(&arr, text.len()).or_else(|| try_lossy_route(&arr, text.len()))
}

/// Compresses JSON tool result arrays. Operates on `role == "tool"` messages
/// whose content parses as a JSON array of objects with ≥5 items. Returns the
/// technique names that applied (e.g. "lite::smart_crusher_lossless",
/// "lite::smart_crusher_lossy").
///
/// Non-tool messages, non-JSON content, JSON that isn't an array, arrays with
/// fewer than 5 items, and arrays containing non-object items are all skipped
/// (the function returns an empty `Vec` for those messages and moves on).
pub fn smart_crush_tool_results(msgs: &mut Messages) -> Vec<&'static str> {
    let mut applied: Vec<&'static str> = Vec::new();
    for msg in msgs.iter_mut() {
        if msg.role != "tool" {
            continue;
        }
        let Some(text) = msg.content.as_ref().and_then(|c| c.as_str()) else {
            continue;
        };
        if let Some((compressed, technique)) = crush_json_string(text) {
            msg.content = Some(Value::String(compressed));
            applied.push(technique);
        }
    }
    applied
}

/// `count / total >= num / den` via integer math (no float).
fn at_least(count: usize, total: usize, num: usize, den: usize) -> bool {
    if total == 0 {
        return false;
    }
    count * den >= total * num
}

/// Ceiling of `total * num / den` (integer math).
fn ceil_div_times(total: usize, num: usize, den: usize) -> usize {
    if total == 0 {
        return 0;
    }
    (total * num).div_ceil(den)
}

/// Sorted union of all field names across all items.
fn all_fields<'a>(arr: &'a [Value]) -> Vec<&'a str> {
    let mut fields: BTreeSet<&'a str> = BTreeSet::new();
    for item in arr {
        if let Some(obj) = item.as_object() {
            for k in obj.keys() {
                fields.insert(k.as_str());
            }
        }
    }
    fields.into_iter().collect()
}

fn object_has_sufficient_fields(obj: &serde_json::Map<String, Value>, fields: &[&str]) -> bool {
    let present = fields.iter().filter(|f| obj.contains_key(**f)).count();
    at_least(present, fields.len(), COVERAGE_NUM, COVERAGE_DEN)
}

/// ≥80% of items must contain ≥80% of `fields`.
fn check_field_coverage(arr: &[Value], fields: &[&str]) -> bool {
    if fields.is_empty() || arr.is_empty() {
        return false;
    }
    let items_passing = arr
        .iter()
        .filter_map(Value::as_object)
        .filter(|obj| object_has_sufficient_fields(obj, fields))
        .count();
    at_least(items_passing, arr.len(), COVERAGE_NUM, COVERAGE_DEN)
}

/// CSV-escape a single cell. Quote if it contains comma, quote, newline, or
/// CR (standard RFC 4180 quoting). Embedded quotes are doubled.
fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        let escaped = s.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

/// Render a JSON value as a CSV cell.
fn json_value_to_csv_cell(val: &Value) -> String {
    match val {
        Value::Null => String::new(),
        Value::String(s) => csv_escape(s),
        Value::Array(_) | Value::Object(_) => csv_escape(&val.to_string()),
        scalar => scalar.to_string(),
    }
}

fn format_csv_row(obj: &serde_json::Map<String, Value>, fields: &[&str]) -> String {
    fields
        .iter()
        .map(|field| obj.get(*field).map_or_else(String::new, json_value_to_csv_cell))
        .collect::<Vec<_>>()
        .join(",")
}

/// Try the lossless CSV-schema path. Returns `None` if coverage fails or
/// there are no fields.
fn try_lossless_csv(arr: &[Value]) -> Option<String> {
    let fields = all_fields(arr);
    if fields.is_empty() || !check_field_coverage(arr, &fields) {
        return None;
    }
    let mut out = String::with_capacity(arr.len() * 32);
    out.push_str("#schema:");
    out.push_str(&fields.join(","));
    for item in arr {
        let obj = item.as_object()?;
        out.push('\n');
        out.push_str(&format_csv_row(obj, &fields));
    }
    Some(out)
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

/// True if `s` contains any error token (case-insensitive substring).
fn string_has_error_token(s: &str) -> bool {
    ERROR_TOKENS
        .iter()
        .any(|t| contains_case_insensitive_ascii(s, t))
}

/// Recursively check whether any string inside a JSON value contains an
/// error token. Numbers/bools/null can never match alphabetic tokens, so
/// they short-circuit to `false`.
fn value_has_error_token(v: &Value) -> bool {
    match v {
        Value::String(s) => string_has_error_token(s),
        Value::Array(a) => a.iter().any(value_has_error_token),
        Value::Object(o) => o.values().any(value_has_error_token),
        _ => false,
    }
}

/// True if any field *value* of `item` matches an error token. Keys are
/// intentionally NOT checked — only values, per the spec.
fn item_has_error_token(item: &Value) -> bool {
    item.as_object()
        .is_some_and(|o| o.values().any(value_has_error_token))
}

fn collect_lossy_indices(arr: &[Value]) -> BTreeSet<usize> {
    let n = arr.len();
    let first_n = ceil_div_times(n, LOSSY_FIRST_NUM, LOSSY_FIRST_DEN);
    let last_n = ceil_div_times(n, LOSSY_LAST_NUM, LOSSY_LAST_DEN);

    let mut indices: BTreeSet<usize> = BTreeSet::new();
    indices.extend(0..first_n.min(n));
    indices.extend(n.saturating_sub(last_n)..n);
    for (i, item) in arr.iter().enumerate() {
        if item_has_error_token(item) {
            indices.insert(i);
        }
    }
    indices
}

fn deduplicate_lossy_items(arr: &[Value], indices: &BTreeSet<usize>) -> Vec<Value> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut kept_items: Vec<Value> = Vec::new();
    for &i in indices {
        if let Some(item) = arr.get(i) {
            let serialized = item.to_string();
            if seen.insert(serialized) {
                kept_items.push(item.to_owned());
            }
        }
    }
    if kept_items.len() > LOSSY_MAX_ITEMS {
        kept_items.truncate(LOSSY_MAX_ITEMS);
    }
    kept_items
}

/// Try the lossy crush path. Returns `None` if nothing would be dropped
/// (i.e. every item ended up in the kept set after dedup + cap).
fn try_lossy(arr: &[Value]) -> Option<String> {
    let n = arr.len();
    let kept_indices = collect_lossy_indices(arr);
    let kept_items = deduplicate_lossy_items(arr, &kept_indices);

    let kept = kept_items.len();
    let dropped = n.saturating_sub(kept);
    if dropped == 0 {
        return None;
    }

    let mut out = String::with_capacity(64 + kept * 64);
    let _ = writeln!(out, "[#crushed: kept {kept} of {n} items]");
    out.push_str(&Value::Array(kept_items).to_string());
    let _ = write!(out, "\n{{\"_dropped\":{dropped}}}");
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn test_smart_crusher_lossless_csv_schema() {
        let mut items = Vec::new();
        for i in 1..=10 {
            items.push(json!({
                "id": i,
                "name": format!("user{}", i),
                "status": "ok"
            }));
        }
        let content = serde_json::to_string(&Value::Array(items)).unwrap();
        let mut msgs = vec![msg("tool", &content)];
        let applied = smart_crush_tool_results(&mut msgs);
        assert_eq!(applied, vec![LOSSLESS_TECHNIQUE]);
        let out = msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap();
        // Keys serialize alphabetically (BTreeMap default): id, name, status.
        assert!(out.starts_with("#schema:id,name,status"), "got: {out}");
        assert!(out.contains("user1"));
        assert!(out.contains("user10"));
        // No JSON braces should remain in a CSV rendering.
        assert!(!out.contains('{'));
        assert!(!out.contains('}'));
    }

    #[test]
    fn test_smart_crusher_lossy_keeps_errors() {
        let mut items = Vec::new();
        for i in 0..20 {
            let status = if i == 2 || i == 10 || i == 19 {
                "error"
            } else {
                "ok"
            };
            // Each item gets a unique field name so the union of field names
            // (21 of them) is huge compared to per-item field count (2).
            // That makes the ≥80% coverage check fail and we land on the
            // lossy path.
            let mut obj = serde_json::Map::new();
            obj.insert(format!("k{i}"), Value::String("v".to_string()));
            obj.insert("status".to_string(), Value::String(status.to_string()));
            items.push(Value::Object(obj));
        }
        let content = serde_json::to_string(&Value::Array(items)).unwrap();
        let mut msgs = vec![msg("tool", &content)];
        let applied = smart_crush_tool_results(&mut msgs);
        assert_eq!(applied, vec![LOSSY_TECHNIQUE]);
        let out = msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap();
        assert!(out.starts_with("[#crushed:"), "got: {out}");
        // All three error items must be retained, regardless of position
        // (one is in the head, one in the middle, one in the tail).
        let error_count = out.matches("error").count();
        assert_eq!(error_count, 3, "expected 3 error items kept, got: {out}");
    }

    #[test]
    fn test_smart_crusher_skips_small_arrays() {
        let items = vec![
            json!({"id": 1, "name": "a"}),
            json!({"id": 2, "name": "b"}),
            json!({"id": 3, "name": "c"}),
        ];
        let content = serde_json::to_string(&Value::Array(items)).unwrap();
        let original = &content;
        let mut msgs = vec![msg("tool", &content)];
        let applied = smart_crush_tool_results(&mut msgs);
        assert!(applied.is_empty());
        assert_eq!(
            msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap(),
            original
        );
    }

    #[test]
    fn test_smart_crusher_skips_non_json() {
        let original = "just some plain text, not json at all";
        let mut msgs = vec![msg("tool", original)];
        let applied = smart_crush_tool_results(&mut msgs);
        assert!(applied.is_empty());
        assert_eq!(
            msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap(),
            original
        );
    }

    #[test]
    fn test_smart_crusher_skips_non_array_json() {
        let original = r#"{"key":"value"}"#;
        let mut msgs = vec![msg("tool", original)];
        let applied = smart_crush_tool_results(&mut msgs);
        assert!(applied.is_empty());
        assert_eq!(
            msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap(),
            original
        );
    }

    #[test]
    fn test_smart_crusher_dedups_identical() {
        let mut items = Vec::new();
        for _ in 0..5 {
            items.push(json!({"a": "x"}));
        }
        for _ in 0..5 {
            items.push(json!({"b": "y"}));
        }
        let content = serde_json::to_string(&Value::Array(items)).unwrap();
        let mut msgs = vec![msg("tool", &content)];
        let applied = smart_crush_tool_results(&mut msgs);
        assert_eq!(applied, vec![LOSSY_TECHNIQUE]);
        let out = msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap();
        // Coverage fails (each item has 1 of 2 fields), so we land on lossy.
        // Selection: first 3 = {a:x},{a:x},{a:x} (dedup → 1), last 2 =
        // {b:y},{b:y} (dedup → 1). Total kept = 2, dropped = 8.
        assert!(out.contains("[{\"a\":\"x\"},{\"b\":\"y\"}]"), "got: {out}");
        assert!(out.contains("\"_dropped\":8"), "got: {out}");
    }

    #[test]
    fn test_smart_crusher_never_produces_larger_output() {
        // 5 items with totally disjoint keys: the union of field names is 5,
        // each item has 1 field, so ≥80% coverage fails and lossless is
        // skipped. Lossy would emit a 29-byte header + JSON + dropped
        // summary that exceeds the 50-byte input, so it must be skipped too
        // — message untouched, no technique reported.
        let items = vec![
            json!({"a": "b"}),
            json!({"c": "d"}),
            json!({"e": "f"}),
            json!({"g": "h"}),
            json!({"i": "j"}),
        ];
        let content = serde_json::to_string(&Value::Array(items)).unwrap();
        let original = &content;
        let mut msgs = vec![msg("tool", &content)];
        let applied = smart_crush_tool_results(&mut msgs);
        assert!(
            applied.is_empty(),
            "expected no technique, got: {applied:?}"
        );
        assert_eq!(
            msgs[0].content.as_ref().and_then(|c| c.as_str()).unwrap(),
            original
        );
    }
}
