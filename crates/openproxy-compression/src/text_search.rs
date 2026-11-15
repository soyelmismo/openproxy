pub(crate) fn contains_case_insensitive_ascii(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }
    let haystack_bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    let first_lo = needle_bytes[0].to_ascii_lowercase();
    let first_hi = needle_bytes[0].to_ascii_uppercase();
    let limit = haystack_bytes.len() - needle_bytes.len();
    let mut pos = 0;

    while pos <= limit {
        let Some(offset) = (if first_lo == first_hi {
            memchr::memchr(first_lo, &haystack_bytes[pos..=limit])
        } else {
            memchr::memchr2(first_lo, first_hi, &haystack_bytes[pos..=limit])
        }) else {
            return false;
        };
        let idx = pos + offset;
        if haystack_bytes[idx..idx + needle_bytes.len()].eq_ignore_ascii_case(needle_bytes) {
            return true;
        }
        pos = idx + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contains_case_insensitive_ascii() {
        let cases: [(&str, &str, bool); 10] = [
            ("", "", true),
            ("short", "longer_needle", false),
            ("PREFIX mid end", "prefix", true),
            ("start MID end", "mid", true),
            ("start mid END", "end", true),
            ("haystack text", "missing", false),
            ("café", "CAFÉ", false),
            ("hello 🦀 world", "🦀", true),
            ("hello 🦀 world", "WORLD", true),
            ("🦀", "ab", false),
        ];

        for (haystack, needle, expected) in cases {
            assert_eq!(
                contains_case_insensitive_ascii(haystack, needle),
                expected,
                "failed case: haystack={haystack:?}, needle={needle:?}"
            );
        }
    }
}
