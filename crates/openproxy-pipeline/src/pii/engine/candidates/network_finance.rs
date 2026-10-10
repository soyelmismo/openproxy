//! Network, contact, and financial candidate extraction (Email, IP, CreditCard, Phone).

use openproxy_types::config::PiiEntity;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::ops::Range;
use std::str::FromStr;

use super::super::protected::is_in_protected_range;
use super::super::regexes::{
    REGEX_CREDIT_CARD_CANDIDATE, REGEX_DATE_OR_TIME, REGEX_EMAIL, REGEX_IBAN_CANDIDATE, REGEX_IPV4,
    REGEX_IPV6, REGEX_PHONE_INTL, REGEX_PHONE_REGIONAL,
};
use super::super::validators::{is_valid_iban, luhn_check};
use super::Candidate;

pub(crate) fn collect_network_finance_candidates<'a>(
    entities: &[PiiEntity],
    text: &'a str,
    syntax_protected: &[Range<usize>],
    candidates: &mut Vec<Candidate<'a>>,
) {
    if entities.contains(&PiiEntity::Email) {
        for m in REGEX_EMAIL.find_iter(text) {
            if !is_in_protected_range(syntax_protected, m.start(), m.end()) {
                candidates.push(Candidate {
                    start: m.start(),
                    end: m.end(),
                    entity: PiiEntity::Email,
                    matched_text: m.as_str(),
                });
            }
        }
    }

    if entities.contains(&PiiEntity::CreditCard) {
        for m in REGEX_CREDIT_CARD_CANDIDATE.find_iter(text) {
            // Preceded by another hyphenated or spaced digit run.
            if m.start() > 0 {
                let head = &text[..m.start()];
                if head.ends_with('-') || head.ends_with(' ') {
                    let trimmed = head.trim_end_matches(['-', ' ']);
                    if trimmed.ends_with(|c: char| c.is_ascii_digit()) {
                        continue;
                    }
                }
            }
            // Followed by another digit run: 20+ digit product keys and
            // serials such as 4532-0151-1283-0366-1234.
            if m.end() < text.len() {
                let tail = &text[m.end()..];
                if tail.starts_with('-') || tail.starts_with(' ') {
                    let trimmed = tail.trim_start_matches(['-', ' ']);
                    if trimmed.starts_with(|c: char| c.is_ascii_digit()) {
                        continue;
                    }
                }
            }
            let s = m.as_str();
            let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
            if luhn_check(&digits) && !is_in_protected_range(syntax_protected, m.start(), m.end()) {
                candidates.push(Candidate {
                    start: m.start(),
                    end: m.end(),
                    entity: PiiEntity::CreditCard,
                    matched_text: s,
                });
            }
        }
        for m in REGEX_IBAN_CANDIDATE.find_iter(text) {
            let s = m.as_str();
            if is_valid_iban(s) && !is_in_protected_range(syntax_protected, m.start(), m.end()) {
                candidates.push(Candidate {
                    start: m.start(),
                    end: m.end(),
                    entity: PiiEntity::CreditCard,
                    matched_text: s,
                });
            }
        }
    }

    if entities.contains(&PiiEntity::Ip) {
        for m in REGEX_IPV4.find_iter(text) {
            let s = m.as_str();
            let is_semver = m.start() > 0
                && (text.as_bytes()[m.start() - 1] == b'v'
                    || text.as_bytes()[m.start() - 1] == b'V');
            if is_semver {
                continue;
            }
            if m.start() > 1
                && text.as_bytes()[m.start() - 1] == b'.'
                && text.as_bytes()[m.start() - 2].is_ascii_digit()
            {
                continue;
            }
            if m.end() + 1 < text.len()
                && text.as_bytes()[m.end()] == b'.'
                && text.as_bytes()[m.end() + 1].is_ascii_digit()
            {
                continue;
            }

            if Ipv4Addr::from_str(s).is_ok()
                && !is_in_protected_range(syntax_protected, m.start(), m.end())
            {
                candidates.push(Candidate {
                    start: m.start(),
                    end: m.end(),
                    entity: PiiEntity::Ip,
                    matched_text: s,
                });
            }
        }

        for m in REGEX_IPV6.find_iter(text) {
            if m.start() > 0 {
                let prev = text.as_bytes()[m.start() - 1];
                if prev == b':' || prev.is_ascii_hexdigit() {
                    continue;
                }
            }
            if m.end() < text.len() {
                let next = text.as_bytes()[m.end()];
                if next == b':' || next.is_ascii_hexdigit() {
                    continue;
                }
            }
            let s = m.as_str();
            if s == "::" {
                let attached = text[..m.start()]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
                    || text[m.end()..]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_alphanumeric() || c == '_');
                let previous_word = text[..m.start()]
                    .trim_end_matches(|c: char| c.is_whitespace() || c == ':' || c == '=')
                    .rsplit(|c: char| !c.is_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or("");
                let network_context = [
                    "ip",
                    "ipv6",
                    "host",
                    "address",
                    "bind",
                    "listen",
                    "listening",
                    "interface",
                    "gateway",
                    "ip_address",
                    "bind_address",
                ]
                .iter()
                .any(|label| previous_word.eq_ignore_ascii_case(label));
                if attached || (text.trim() != "::" && !network_context) {
                    continue;
                }
            }
            if s.contains(':')
                && Ipv6Addr::from_str(s).is_ok()
                && !is_in_protected_range(syntax_protected, m.start(), m.end())
            {
                candidates.push(Candidate {
                    start: m.start(),
                    end: m.end(),
                    entity: PiiEntity::Ip,
                    matched_text: s,
                });
            }
        }
    }

    if entities.contains(&PiiEntity::Phone) {
        for m in REGEX_PHONE_INTL.find_iter(text) {
            if m.start() > 0 && text.as_bytes()[m.start() - 1].is_ascii_alphanumeric() {
                continue;
            }
            if m.end() < text.len() && text.as_bytes()[m.end()].is_ascii_digit() {
                continue;
            }
            if m.end() < text.len() {
                let tail = &text[m.end()..];
                if tail.starts_with('-') || tail.starts_with('.') || tail.starts_with(' ') {
                    let trimmed = tail.trim_start_matches(['-', '.', ' ']);
                    if trimmed.starts_with(|c: char| c.is_ascii_digit()) {
                        continue;
                    }
                }
            }

            let s = m.as_str();
            let digit_count = s
                .chars()
                .filter(|c| c.is_ascii_digit() || ('\u{FF10}'..='\u{FF19}').contains(c))
                .count();
            if (7..=15).contains(&digit_count)
                && !REGEX_DATE_OR_TIME.is_match(s)
                && !is_in_protected_range(syntax_protected, m.start(), m.end())
            {
                candidates.push(Candidate {
                    start: m.start(),
                    end: m.end(),
                    entity: PiiEntity::Phone,
                    matched_text: s,
                });
            }
        }

        for m in REGEX_PHONE_REGIONAL.find_iter(text) {
            if m.start() > 0 {
                let prev = text.as_bytes()[m.start() - 1];
                if prev.is_ascii_alphanumeric() || matches!(prev, b'+' | b'*' | b'/' | b'=' | b'%')
                {
                    continue;
                }
                let head = &text[..m.start()];
                if head.ends_with('-') || head.ends_with('.') || head.ends_with(' ') {
                    let trimmed = head.trim_end_matches(['-', '.', ' ']);
                    if trimmed.ends_with(|c: char| c.is_ascii_digit()) {
                        continue;
                    }
                }
            }
            if m.end() < text.len() {
                let tail = &text[m.end()..];
                if tail.starts_with('-') || tail.starts_with('.') || tail.starts_with(' ') {
                    let trimmed = tail.trim_start_matches(['-', '.', ' ']);
                    if trimmed.starts_with(|c: char| c.is_ascii_digit()) {
                        continue;
                    }
                }
            }

            let s = m.as_str();
            let digit_count = s
                .chars()
                .filter(|c| c.is_ascii_digit() || ('\u{FF10}'..='\u{FF19}').contains(c))
                .count();
            if (7..=15).contains(&digit_count)
                && !REGEX_DATE_OR_TIME.is_match(s)
                && !is_in_protected_range(syntax_protected, m.start(), m.end())
            {
                candidates.push(Candidate {
                    start: m.start(),
                    end: m.end(),
                    entity: PiiEntity::Phone,
                    matched_text: s,
                });
            }
        }
    }
}
