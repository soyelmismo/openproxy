pub(crate) fn contains_case_insensitive_ascii(haystack: &str, needle: &str) -> bool {
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
