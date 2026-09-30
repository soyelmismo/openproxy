//! Regression tests for the Unicode homoglyph / fullwidth / zero-width
//! bypasses of the PII regex engine.
//!
//! Security: before the fix, every PII regex was ASCII-only, so any non-ASCII
//! substitution (FULLWIDTH @ U+FF20, FULLWIDTH DIGITS U+FF11..U+FF19,
//! HYPHEN U+2010, ZERO WIDTH SPACE U+200B, Cyrillic 'е' U+0435 in domain)
//! defeated detection and the raw PII was forwarded to the upstream LLM
//! provider verbatim. These tests confirm the extended regexes catch each
//! variant that was previously leaking.

use openproxy_pipeline::pii::engine::regexes::{
    REGEX_EMAIL, REGEX_PHONE_INTL, REGEX_PHONE_REGIONAL,
};

#[test]
fn email_fullwidth_at_is_detected() {
    // FULLWIDTH COMMERCIAL AT U+FF20
    let input = "alice\u{FF20}example.com";
    let m = REGEX_EMAIL.find(input).expect("FULLWIDTH @ must match");
    assert_eq!(m.as_str(), input);
}

#[test]
fn email_fullwidth_dot_in_tld_is_detected() {
    // FULLWIDTH FULL STOP U+FF0E
    let input = "alice@example\u{FF0E}com";
    let m = REGEX_EMAIL.find(input).expect("FULLWIDTH . must match");
    assert_eq!(m.as_str(), input);
}

#[test]
fn email_zero_width_space_before_at_is_detected() {
    // ZERO WIDTH SPACE U+200B
    let input = "alice\u{200B}@example.com";
    let m = REGEX_EMAIL.find(input).expect("ZWS before @ must match");
    assert_eq!(m.as_str(), input);
}

#[test]
fn email_cyrillic_homoglyph_in_domain_is_detected() {
    // Cyrillic 'е' U+0435 instead of Latin 'e' U+0065
    let input = "test@\u{0435}xample.com";
    let m = REGEX_EMAIL.find(input).expect("Cyrillic 'е' in domain must match");
    assert_eq!(m.as_str(), input);
}

#[test]
fn phone_intl_fullwidth_digits_are_detected() {
    // FULLWIDTH DIGITS U+FF11..U+FF19
    let input = "+\u{FF11}\u{FF12}\u{FF13}-\u{FF15}\u{FF15}\u{FF15}-\u{FF10}\u{FF11}\u{FF19}\u{FF19}";
    let m = REGEX_PHONE_INTL.find(input).expect("FULLWIDTH digits must match");
    assert_eq!(m.as_str(), input);
}

#[test]
fn phone_intl_unicode_hyphen_is_detected() {
    // HYPHEN U+2010 instead of ASCII HYPHEN-MINUS U+002D
    let input = "+1\u{2010}800\u{2010}555\u{2010}0199";
    let m = REGEX_PHONE_INTL.find(input).expect("U+2010 hyphen must match");
    assert_eq!(m.as_str(), input);
}

#[test]
fn phone_regional_unicode_hyphen_is_detected() {
    // EN DASH U+2013 instead of ASCII HYPHEN-MINUS
    let input = "1\u{2013}800\u{2013}555\u{2013}0199";
    let m = REGEX_PHONE_REGIONAL
        .find(input)
        .expect("U+2013 en-dash must match");
    assert_eq!(m.as_str(), input);
}
