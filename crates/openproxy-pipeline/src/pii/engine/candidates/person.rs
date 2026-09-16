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

pub(crate) fn collect_person_candidates<'a>(
    text: &'a str,
    syntax_protected: &[Range<usize>],
    code_protected: &[Range<usize>],
    candidates: &mut Vec<Candidate<'a>>,
) {
    // A. Contextual person markers (User: "miguel", DM with miguel, PC Miguel, etc.)
    for cap in REGEX_CONTEXTUAL_PERSON.captures_iter(text) {
        let m = cap.get(1).or_else(|| cap.get(2)).or_else(|| cap.get(3));
        if let Some(m) = m {
            let s = m.as_str();
            if !is_stopword(s) && !is_in_protected_range(syntax_protected, m.start(), m.end()) {
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
}
