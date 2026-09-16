//! Secret and API key candidate extraction.

use openproxy_types::config::PiiEntity;
use std::ops::Range;

use super::super::protected::is_in_protected_range;
use super::super::regexes::{
    REGEX_DSN_PASSWORD, REGEX_NATIONAL_ID_CL, REGEX_NATIONAL_ID_ES, REGEX_NATIONAL_ID_US,
    REGEX_SECRET_AI_CLOUD_PLATFORMS, REGEX_SECRET_ANTHROPIC, REGEX_SECRET_AWS,
    REGEX_SECRET_BASIC_AUTH, REGEX_SECRET_BEARER, REGEX_SECRET_CLI_FLAGS, REGEX_SECRET_CLOUDFLARE,
    REGEX_SECRET_GITHUB, REGEX_SECRET_GOOGLE, REGEX_SECRET_HIGH_ENTROPY, REGEX_SECRET_JWT,
    REGEX_SECRET_LABELED, REGEX_SECRET_OPENAI, REGEX_SECRET_PASSWORD_HASH,
    REGEX_SECRET_PLATFORM_ID, REGEX_SECRET_PRIVATE_KEY_PEM, REGEX_SECRET_SLACK,
    REGEX_SECRET_SSHPASS, REGEX_SECRET_TELEGRAM, REGEX_URI_USERINFO_PASSWORD,
    REGEX_URL_SENSITIVE_PARAM,
};
use super::super::validators::{
    is_high_entropy_secret, is_valid_chilean_rut, is_valid_secret_value, is_valid_spanish_id,
    is_valid_ssn,
};
use super::Candidate;

pub(crate) fn collect_secret_candidates<'a>(
    text: &'a str,
    syntax_protected: &[Range<usize>],
    candidates: &mut Vec<Candidate<'a>>,
) {
    for cap in REGEX_SECRET_BEARER.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let start = m.start();
            let raw = m.as_str();
            let trimmed = raw.trim_end_matches('.');
            let end = start + trimmed.len();
            if trimmed.len() >= 20 && !is_in_protected_range(syntax_protected, start, end) {
                candidates.push(Candidate {
                    start,
                    end,
                    entity: PiiEntity::Secret,
                    matched_text: trimmed,
                });
            }
        }
    }
    for m in REGEX_SECRET_AWS.find_iter(text) {
        if !is_in_protected_range(syntax_protected, m.start(), m.end()) {
            candidates.push(Candidate {
                start: m.start(),
                end: m.end(),
                entity: PiiEntity::Secret,
                matched_text: m.as_str(),
            });
        }
    }
    for m in REGEX_SECRET_OPENAI.find_iter(text) {
        if !is_in_protected_range(syntax_protected, m.start(), m.end()) {
            candidates.push(Candidate {
                start: m.start(),
                end: m.end(),
                entity: PiiEntity::Secret,
                matched_text: m.as_str(),
            });
        }
    }
    for m in REGEX_SECRET_ANTHROPIC.find_iter(text) {
        if !is_in_protected_range(syntax_protected, m.start(), m.end()) {
            candidates.push(Candidate {
                start: m.start(),
                end: m.end(),
                entity: PiiEntity::Secret,
                matched_text: m.as_str(),
            });
        }
    }
    for m in REGEX_SECRET_GITHUB.find_iter(text) {
        if !is_in_protected_range(syntax_protected, m.start(), m.end()) {
            candidates.push(Candidate {
                start: m.start(),
                end: m.end(),
                entity: PiiEntity::Secret,
                matched_text: m.as_str(),
            });
        }
    }
    for m in REGEX_SECRET_SLACK.find_iter(text) {
        if !is_in_protected_range(syntax_protected, m.start(), m.end()) {
            candidates.push(Candidate {
                start: m.start(),
                end: m.end(),
                entity: PiiEntity::Secret,
                matched_text: m.as_str(),
            });
        }
    }
    for m in REGEX_SECRET_GOOGLE.find_iter(text) {
        if !is_in_protected_range(syntax_protected, m.start(), m.end()) {
            candidates.push(Candidate {
                start: m.start(),
                end: m.end(),
                entity: PiiEntity::Secret,
                matched_text: m.as_str(),
            });
        }
    }
    for m in REGEX_SECRET_CLOUDFLARE.find_iter(text) {
        if !is_in_protected_range(syntax_protected, m.start(), m.end()) {
            candidates.push(Candidate {
                start: m.start(),
                end: m.end(),
                entity: PiiEntity::Secret,
                matched_text: m.as_str(),
            });
        }
    }
    for m in REGEX_SECRET_TELEGRAM.find_iter(text) {
        if !is_in_protected_range(syntax_protected, m.start(), m.end()) {
            candidates.push(Candidate {
                start: m.start(),
                end: m.end(),
                entity: PiiEntity::Secret,
                matched_text: m.as_str(),
            });
        }
    }
    for m in REGEX_SECRET_PRIVATE_KEY_PEM.find_iter(text) {
        if !is_in_protected_range(syntax_protected, m.start(), m.end()) {
            candidates.push(Candidate {
                start: m.start(),
                end: m.end(),
                entity: PiiEntity::Secret,
                matched_text: m.as_str(),
            });
        }
    }
    for cap in REGEX_SECRET_LABELED.captures_iter(text) {
        let Some(key_m) = cap.get(1).or_else(|| cap.get(5)) else {
            continue;
        };
        let Some(val_m) = cap
            .get(2)
            .or_else(|| cap.get(3))
            .or_else(|| cap.get(4))
            .or_else(|| cap.get(6))
            .or_else(|| cap.get(7))
            .or_else(|| cap.get(8))
        else {
            continue;
        };
        let key_str = key_m.as_str();
        let val_raw = val_m.as_str();
        let trimmed_val = val_raw.trim_end_matches(['\\', ';', ',', '.', ')', ']', '}', '"', '\'']);

        // If unquoted and followed by more text on the same line, check if it's natural language prose
        if (cap.get(4).is_some() || cap.get(8).is_some()) && val_m.end() < text.len() {
            let line_tail = text[val_m.end()..].lines().next().unwrap_or("");
            let trimmed_tail = line_tail.trim();
            if !trimmed_tail.is_empty() && !trimmed_tail.starts_with('#') {
                let is_pure_alpha = trimmed_val.chars().all(|c| c.is_alphabetic());
                if is_pure_alpha && !is_high_entropy_secret(trimmed_val) {
                    continue;
                }
            }
        }

        if trimmed_val.len() >= 4 && is_valid_secret_value(key_str, trimmed_val) {
            let start = val_m.start();
            let end = start + trimmed_val.len();
            if !is_in_protected_range(syntax_protected, start, end) {
                candidates.push(Candidate {
                    start,
                    end,
                    entity: PiiEntity::Secret,
                    matched_text: trimmed_val,
                });
            }
        }
    }
    for cap in REGEX_URL_SENSITIVE_PARAM.captures_iter(text) {
        if let Some(val_m) = cap.get(1) {
            let val = val_m.as_str();
            let trimmed_val = val.trim_end_matches(['\\', ';', ',', '.', ')', ']', '}', '"', '\'']);
            let start = val_m.start();
            let end = start + trimmed_val.len();
            if trimmed_val.len() >= 6
                && is_valid_secret_value("token", trimmed_val)
                && !is_in_protected_range(syntax_protected, start, end)
            {
                candidates.push(Candidate {
                    start,
                    end,
                    entity: PiiEntity::Secret,
                    matched_text: trimmed_val,
                });
            }
        }
    }
    for m in REGEX_SECRET_HIGH_ENTROPY.find_iter(text) {
        // If immediately preceded by '-' after whitespace, quote, bracket, comma or start of line,
        // it is a CLI flag like -pSECRET... or -u... Let REGEX_SECRET_CLI_FLAGS handle it.
        if m.start() > 0 && text.as_bytes()[m.start() - 1] == b'-' {
            let before_dash = if m.start() >= 2 {
                let b = text.as_bytes()[m.start() - 2];
                b.is_ascii_whitespace() || matches!(b, b'"' | b'\'' | b'[' | b',' | b'-')
            } else {
                true
            };
            if before_dash {
                continue;
            }
        }
        let s = m.as_str();
        if is_high_entropy_secret(s) && !is_in_protected_range(syntax_protected, m.start(), m.end())
        {
            candidates.push(Candidate {
                start: m.start(),
                end: m.end(),
                entity: PiiEntity::Secret,
                matched_text: s,
            });
        }
    }
    for m in REGEX_SECRET_JWT.find_iter(text) {
        if !is_in_protected_range(syntax_protected, m.start(), m.end()) {
            candidates.push(Candidate {
                start: m.start(),
                end: m.end(),
                entity: PiiEntity::Secret,
                matched_text: m.as_str(),
            });
        }
    }
    for cap in REGEX_URI_USERINFO_PASSWORD.captures_iter(text) {
        if let Some(pass_m) = cap.get(1) {
            let start = pass_m.start();
            let end = pass_m.end();
            let pass_str = pass_m.as_str();
            if !is_in_protected_range(syntax_protected, start, end) {
                candidates.push(Candidate {
                    start,
                    end,
                    entity: PiiEntity::Secret,
                    matched_text: pass_str,
                });
            }
        }
    }
    for cap in REGEX_DSN_PASSWORD.captures_iter(text) {
        if let Some(pass_m) = cap.get(1) {
            let start = pass_m.start();
            let end = pass_m.end();
            let pass_str = pass_m.as_str();
            if is_valid_secret_value("password", pass_str)
                && !is_in_protected_range(syntax_protected, start, end)
            {
                candidates.push(Candidate {
                    start,
                    end,
                    entity: PiiEntity::Secret,
                    matched_text: pass_str,
                });
            }
        }
    }
    for cap in REGEX_SECRET_PASSWORD_HASH.captures_iter(text) {
        if let Some(hash_m) = cap.get(1) {
            let start = hash_m.start();
            let end = hash_m.end();
            let hash_str = hash_m.as_str();
            if !is_in_protected_range(syntax_protected, start, end) {
                candidates.push(Candidate {
                    start,
                    end,
                    entity: PiiEntity::Secret,
                    matched_text: hash_str,
                });
            }
        }
    }
    for cap in REGEX_SECRET_BASIC_AUTH.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let start = m.start();
            let end = m.end();
            if !is_in_protected_range(syntax_protected, start, end) {
                candidates.push(Candidate {
                    start,
                    end,
                    entity: PiiEntity::Secret,
                    matched_text: m.as_str(),
                });
            }
        }
    }
    for m in REGEX_SECRET_AI_CLOUD_PLATFORMS.find_iter(text) {
        if !is_in_protected_range(syntax_protected, m.start(), m.end()) {
            candidates.push(Candidate {
                start: m.start(),
                end: m.end(),
                entity: PiiEntity::Secret,
                matched_text: m.as_str(),
            });
        }
    }
    for cap in REGEX_SECRET_CLI_FLAGS.captures_iter(text) {
        let m = cap.get(1).or_else(|| cap.get(2));
        if let Some(m) = m {
            let start = m.start();
            let end = m.end();
            let val = m.as_str();
            if is_valid_secret_value("-p", val)
                && !is_in_protected_range(syntax_protected, start, end)
            {
                candidates.push(Candidate {
                    start,
                    end,
                    entity: PiiEntity::Secret,
                    matched_text: val,
                });
            }
        }
    }
    for cap in REGEX_SECRET_SSHPASS.captures_iter(text) {
        let m = cap.get(1).or_else(|| cap.get(2));
        if let Some(m) = m {
            let start = m.start();
            let end = m.end();
            let val = m.as_str();
            if !val.is_empty()
                && is_valid_secret_value("-p_sshpass", val)
                && !is_in_protected_range(syntax_protected, start, end)
            {
                candidates.push(Candidate {
                    start,
                    end,
                    entity: PiiEntity::Secret,
                    matched_text: val,
                });
            }
        }
    }
    for cap in REGEX_SECRET_PLATFORM_ID.captures_iter(text) {
        let m = cap.get(1).or_else(|| cap.get(2));
        if let Some(m) = m {
            let start = m.start();
            let end = m.end();
            let val = m.as_str();
            if !is_in_protected_range(syntax_protected, start, end) {
                candidates.push(Candidate {
                    start,
                    end,
                    entity: PiiEntity::Secret,
                    matched_text: val,
                });
            }
        }
    }
    for m in REGEX_NATIONAL_ID_ES.find_iter(text) {
        let s = m.as_str();
        if is_valid_spanish_id(s) && !is_in_protected_range(syntax_protected, m.start(), m.end()) {
            candidates.push(Candidate {
                start: m.start(),
                end: m.end(),
                entity: PiiEntity::Secret,
                matched_text: s,
            });
        }
    }
    for cap in REGEX_NATIONAL_ID_CL.captures_iter(text) {
        if let (Some(m_full), Some(m_digits), Some(m_verif)) = (cap.get(0), cap.get(1), cap.get(2))
        {
            let verif_char = m_verif.as_str().chars().next().unwrap_or('?');
            if is_valid_chilean_rut(m_digits.as_str(), verif_char)
                && !is_in_protected_range(syntax_protected, m_full.start(), m_full.end())
            {
                candidates.push(Candidate {
                    start: m_full.start(),
                    end: m_full.end(),
                    entity: PiiEntity::Secret,
                    matched_text: m_full.as_str(),
                });
            }
        }
    }
    for m in REGEX_NATIONAL_ID_US.find_iter(text) {
        let s = m.as_str();
        if is_valid_ssn(s) && !is_in_protected_range(syntax_protected, m.start(), m.end()) {
            candidates.push(Candidate {
                start: m.start(),
                end: m.end(),
                entity: PiiEntity::Secret,
                matched_text: s,
            });
        }
    }
}
