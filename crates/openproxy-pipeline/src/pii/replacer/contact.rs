//! Conservative token boundaries for contact-shaped placeholders.

use super::is_token_char;

#[derive(Clone, Copy)]
pub(super) enum ContactKind {
    Email,
    Ipv4,
    Ipv6,
}

pub(super) fn contact_kind(pattern: &str) -> Option<ContactKind> {
    if pattern.contains('@') {
        Some(ContactKind::Email)
    } else if pattern.parse::<std::net::Ipv4Addr>().is_ok() {
        Some(ContactKind::Ipv4)
    } else if pattern.parse::<std::net::Ipv6Addr>().is_ok() {
        Some(ContactKind::Ipv6)
    } else {
        None
    }
}

pub(super) fn extends_contact_left(kind: ContactKind, c: char) -> bool {
    match kind {
        ContactKind::Email => ".+%-'@".contains(c),
        ContactKind::Ipv4 => ".+%".contains(c),
        ContactKind::Ipv6 => c == ':',
    }
}

/// Some(true): larger token. Some(false): delimiter. None: need next chunk.
/// A trailing dot is punctuation unless followed by a domain/IP segment.
/// At a chunk edge the next character is unknown, so defer the match.
pub(super) fn extends_contact_right(kind: ContactKind, tail: &str, is_eof: bool) -> Option<bool> {
    let mut chars = tail.chars();
    let Some(first) = chars.next() else {
        return Some(false);
    };
    match kind {
        ContactKind::Email if "-+%@".contains(first) => Some(true),
        ContactKind::Ipv6 => Some(first == ':' || first == '%'),
        _ if first == '.' => match chars.next() {
            Some(next) => Some(is_token_char(next) || next == '-'),
            None if is_eof => Some(false),
            None => None,
        },
        _ => Some(false),
    }
}
