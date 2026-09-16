//! Range protection for code blocks and syntax structures.

use std::ops::Range;

use super::regexes::{
    REGEX_DATA_URI, REGEX_FENCED_CODE, REGEX_INLINE_CODE, REGEX_JSON_KEY,
    REGEX_URI_USERINFO_PASSWORD, REGEX_URL, REGEX_URL_SENSITIVE_PARAM,
};

/// Merge overlapping or adjacent ranges.
pub fn merge_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    if ranges.len() <= 1 {
        return ranges;
    }
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        if let Some(last) = merged.last_mut()
            && r.start <= last.end
        {
            last.end = last.end.max(r.end);
            continue;
        }
        merged.push(r);
    }
    merged
}

/// Binary search check whether [start, end) overlaps with any protected range.
#[inline]
pub fn is_in_protected_range(ranges: &[Range<usize>], start: usize, end: usize) -> bool {
    let idx = ranges.partition_point(|r| r.end <= start);
    if idx < ranges.len() {
        start < ranges[idx].end && end > ranges[idx].start
    } else {
        false
    }
}

/// Identify syntax-protected spans (URLs with carveouts, JSON keys)
/// that must not have their structural boundaries corrupted.
pub fn find_syntax_protected_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();

    // 1. URLs (https://..., ftp://..., postgres://..., etc.)
    for m in REGEX_URL.find_iter(text) {
        let url_str = m.as_str();
        let mut carveouts: Vec<Range<usize>> = Vec::new();

        // A. Password in userinfo: postgres://user:password@host
        if let Some(cap) = REGEX_URI_USERINFO_PASSWORD.captures(url_str)
            && let Some(pass_m) = cap.get(1)
        {
            carveouts.push(m.start() + pass_m.start()..m.start() + pass_m.end());
        }

        // B. Sensitive query parameters: https://host/path?token=SECRET_VALUE&...
        for cap in REGEX_URL_SENSITIVE_PARAM.captures_iter(url_str) {
            if let Some(param_val) = cap.get(1) {
                carveouts.push(m.start() + param_val.start()..m.start() + param_val.end());
            }
        }

        if carveouts.is_empty() {
            ranges.push(m.range());
        } else {
            carveouts.sort_by_key(|r| r.start);
            let mut curr = m.start();
            for r in carveouts {
                if r.start > curr {
                    ranges.push(curr..r.start);
                }
                curr = curr.max(r.end);
            }
            if curr < m.end() {
                ranges.push(curr..m.end());
            }
        }
    }

    // 2. Data URIs (data:image/jpeg;base64,...)
    for m in REGEX_DATA_URI.find_iter(text) {
        ranges.push(m.range());
    }

    // 3. JSON keys ("key":)
    for cap in REGEX_JSON_KEY.captures_iter(text) {
        if let Some(key_match) = cap.get(1) {
            ranges.push(key_match.range());
        }
    }

    merge_ranges(ranges)
}

/// Identify code blocks (fenced ``` and inline `) to avoid heuristic false-positives
/// on code variables, types, or syntax.
pub fn find_code_protected_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();

    // 1. Fenced code blocks ```...``` and ~~~...~~~
    for m in REGEX_FENCED_CODE.find_iter(text) {
        ranges.push(m.range());
    }

    // 2. Inline code `...`
    for m in REGEX_INLINE_CODE.find_iter(text) {
        ranges.push(m.range());
    }

    merge_ranges(ranges)
}

/// Identify all protected spans (combining syntax and code protection).
pub fn find_protected_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = find_syntax_protected_ranges(text);
    ranges.extend(find_code_protected_ranges(text));
    merge_ranges(ranges)
}
