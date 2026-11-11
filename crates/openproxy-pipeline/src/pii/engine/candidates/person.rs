//! Person name candidate extraction.

use openproxy_types::config::PiiEntity;
use std::ops::Range;

use super::super::protected::is_in_protected_range;
use super::super::regexes::{
    COMMON_FIRST_NAMES, REGEX_CAPITALIZED_SEQUENCE, REGEX_CONTEXTUAL_PERSON, REGEX_HONORIFIC_NAME,
    REGEX_SINGLE_WORD_CAPITALIZED,
};
use super::super::stopwords::is_stopword;
use super::Candidate;

/// Values captured behind a conversational label (`User:`, `Name:`, `DM with`)
/// must still look like a person. Lowercase identifiers behind a `name=` field
/// (`gpthon`, `serde_json`) are technical values, not people: redacting them
/// puts a synthetic name where the model expects a project or a field and it
/// starts reasoning about the mismatch.
fn is_plausible_person_value(s: &str) -> bool {
    let Some(first) = s.split_whitespace().next() else {
        return false;
    };
    if COMMON_FIRST_NAMES
        .iter()
        .any(|name| name.eq_ignore_ascii_case(first))
    {
        return true;
    }
    // Proper-noun shape: every word starts uppercase, so `Carlos Sanchez`
    // passes while `gpthon` or `serde_json` do not.
    s.split_whitespace()
        .all(|w| w.chars().next().is_some_and(|c| c.is_uppercase()))
}

pub(crate) fn collect_person_candidates<'a>(
    text: &'a str,
    syntax_protected: &[Range<usize>],
    code_protected: &[Range<usize>],
    candidates: &mut Vec<Candidate<'a>>,
) {
    // Where this collector's candidates start in the shared vector, so the
    // identifier filter below never touches another collector's matches.
    let first_person_candidate = candidates.len();

    // A. Contextual person markers (User: "miguel", DM with miguel, PC Miguel, etc.)
    for cap in REGEX_CONTEXTUAL_PERSON.captures_iter(text) {
        let m = cap.get(1).or_else(|| cap.get(2)).or_else(|| cap.get(3));
        if let Some(m) = m {
            let s = m.as_str();
            if !is_stopword(s)
                && is_plausible_person_value(s)
                && !is_in_protected_range(syntax_protected, m.start(), m.end())
            {
                candidates.push(Candidate {
                    start: m.start(),
                    end: m.end(),
                    entity: PiiEntity::Person,
                    matched_text: s,
                });
            }
        }
    }

    // B. Common first names (standalone capitalized words like Miguel, Carlos, etc.)
    for cap in REGEX_SINGLE_WORD_CAPITALIZED.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let s = m.as_str();
            if COMMON_FIRST_NAMES.contains(s)
                && !is_in_protected_range(syntax_protected, m.start(), m.end())
                && !is_in_protected_range(code_protected, m.start(), m.end())
            {
                candidates.push(Candidate {
                    start: m.start(),
                    end: m.end(),
                    entity: PiiEntity::Person,
                    matched_text: s,
                });
            }
        }
    }

    // C. Honorific names
    for cap in REGEX_HONORIFIC_NAME.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let s = m.as_str();
            let words: Vec<&str> = s.split_whitespace().collect();
            if words.iter().all(|w| !is_stopword(w))
                && !is_in_protected_range(syntax_protected, m.start(), m.end())
                && !is_in_protected_range(code_protected, m.start(), m.end())
            {
                candidates.push(Candidate {
                    start: m.start(),
                    end: m.end(),
                    entity: PiiEntity::Person,
                    matched_text: s,
                });
            }
        }
    }

    // D. Capitalized sequence heuristic (requires first word to be a known given name)
    for cap in REGEX_CAPITALIZED_SEQUENCE.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let s = m.as_str();
            let words: Vec<&str> = s.split_whitespace().collect();
            if words.len() >= 2
                && COMMON_FIRST_NAMES.contains(words[0])
                && words.iter().all(|w| !is_stopword(w))
            {
                // Check if preceded by markdown list/heading markers or brackets
                let is_heading_or_bracket = if m.start() == 0 {
                    false
                } else {
                    let prefix = text[..m.start()].trim_end();
                    prefix.ends_with('•')
                        || prefix.ends_with('-')
                        || prefix.ends_with('*')
                        || prefix.ends_with('[')
                        || prefix.ends_with('#')
                        || prefix.ends_with(':')
                };

                if !is_heading_or_bracket
                    && !is_in_protected_range(syntax_protected, m.start(), m.end())
                    && !is_in_protected_range(code_protected, m.start(), m.end())
                {
                    candidates.push(Candidate {
                        start: m.start(),
                        end: m.end(),
                        entity: PiiEntity::Person,
                        matched_text: s,
                    });
                }
            }
        }
    }

    // A person name glued into an identifier is a technical token, not a
    // mention: a hostname like `jdoe-laptop` is not a person. Masking it
    // strands the name inside the token and the restore then rebuilds it
    // with the wrong case (`Jdoe-laptop`), a fidelity loss the client sees.
    // Only the candidates this function produced are filtered: the vector is
    // shared with the other collectors, so a blanket retain would drop their
    // matches too (the IP in `root@10.0.0.1` loses to an `@` separator).
    let own = candidates.split_off(first_person_candidate);
    for c in own {
        if !is_glued_to_identifier(text, c.start, c.end) {
            candidates.push(c);
        }
    }
}

/// Separator bytes that join words into one technical identifier: hostnames,
/// crate paths, package names.
const IDENTIFIER_SEPARATORS: [u8; 4] = *b"-_/\\";

fn is_glued_to_identifier(text: &str, start: usize, end: usize) -> bool {
    let bytes = text.as_bytes();
    let prev = start > 0 && IDENTIFIER_SEPARATORS.contains(&bytes[start - 1]);
    let next = end < bytes.len() && IDENTIFIER_SEPARATORS.contains(&bytes[end]);
    prev || next
}
