use std::borrow::Cow;

#[inline]
pub(crate) fn to_lower_cow(s: &str) -> Cow<'_, str> {
    if s.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(s.to_lowercase())
    } else {
        Cow::Borrowed(s)
    }
}

/// Finds the first entry in `table` where `haystack` contains the pattern key.
#[inline]
pub fn find_substring_match<T: Copy>(haystack: &str, table: &[(&str, T)]) -> Option<T> {
    table
        .iter()
        .find_map(|&(k, v)| if haystack.contains(k) { Some(v) } else { None })
}

/// Checks whether `token` exists in `haystack` bounded by string ends or delimiters (`-`, `_`, `/`, `.`, `:`, ` `).
#[inline]
pub fn contains_delimited_token(haystack: &str, token: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(token) {
        let abs_pos = start + pos;
        let end_pos = abs_pos + token.len();

        let prev_ok = abs_pos == 0
            || matches!(
                haystack.as_bytes()[abs_pos - 1],
                b'/' | b'-' | b'_' | b'.' | b':' | b' '
            );
        let next_ok = end_pos == haystack.len()
            || matches!(
                haystack.as_bytes()[end_pos],
                b'/' | b'-' | b'_' | b'.' | b':' | b' '
            );

        if prev_ok && next_ok {
            return true;
        }
        start = abs_pos + 1;
    }
    false
}
