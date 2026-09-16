//! Cryptographic and algorithmic validation routines for candidate tokens.

use super::stopwords::is_stopword;

/// Calculates Shannon entropy of a byte string in bits per byte (0.0 to 8.0).
pub fn shannon_entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    let mut total = 0;
    for b in s.bytes() {
        counts[b as usize] += 1;
        total += 1;
    }
    let mut entropy = 0.0;
    let total_f = total as f64;
    for &c in &counts {
        if c > 0 {
            let p = (c as f64) / total_f;
            entropy -= p * p.log2();
        }
    }
    entropy
}

/// Validates whether an isolated candidate token possesses cryptographic entropy.
pub fn is_high_entropy_secret(candidate: &str) -> bool {
    let len = candidate.len();
    if !(32..=64).contains(&len) {
        return false;
    }
    let has_letters = candidate.bytes().any(|b| b.is_ascii_alphabetic());
    let has_digits = candidate.bytes().any(|b| b.is_ascii_digit());
    if !has_letters || !has_digits {
        return false;
    }
    if candidate.contains("__") {
        return false;
    }
    let entropy = shannon_entropy(candidate);
    let is_hex = candidate.bytes().all(|b| b.is_ascii_hexdigit());
    if is_hex {
        entropy >= 3.3
    } else {
        entropy >= 3.8
    }
}

/// Filters candidate values matched by generic secret labels.
pub fn is_valid_secret_value(key: &str, val: &str) -> bool {
    let lower_key = key.to_ascii_lowercase();
    if lower_key.ends_with("_user")
        || lower_key.ends_with("_username")
        || lower_key.ends_with("_name")
        || lower_key.ends_with("_file")
        || lower_key.ends_with("_path")
        || lower_key.ends_with("_url")
    {
        return false;
    }
    if val.starts_with('<') && val.ends_with('>') {
        return false;
    }
    if (val.starts_with('{') && val.ends_with('}'))
        || (val.starts_with("${") && val.ends_with('}'))
        || (val.starts_with("{{") && val.ends_with("}}"))
        || (val.starts_with('%') && val.ends_with('%'))
        || (val.starts_with('[') && val.ends_with(']'))
        || (val.starts_with('(') && val.ends_with(')'))
        || val.contains('{')
        || val.contains('}')
        || val.contains('\\')
    {
        return false;
    }
    if val.starts_with("${") {
        return false;
    }
    if let Some(stripped) = val.strip_prefix('$') {
        let unescaped = stripped.strip_prefix('$').unwrap_or(stripped);
        let is_shell_var = !unescaped.is_empty()
            && unescaped
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_');
        if is_shell_var {
            return false;
        }
    }
    let lower_val = val.to_ascii_lowercase();
    if is_stopword(&lower_val) {
        return false;
    }
    if matches!(
        lower_val.as_str(),
        "true"
            | "false"
            | "null"
            | "none"
            | "default"
            | "undefined"
            | "yes"
            | "no"
            | "enabled"
            | "disabled"
            | "optional"
            | "required"
            | "str"
            | "int"
            | "bool"
            | "float"
            | "bytes"
            | "list"
            | "dict"
            | "set"
            | "tuple"
            | "any"
            | "object"
            | "string"
            | "number"
            | "boolean"
            | "array"
            | "char"
            | "void"
            | "unknown"
            | "never"
            | "symbol"
            | "cámbialo"
            | "cambialo"
            | "cambiar"
            | "change"
            | "changeme"
            | "change_me"
            | "change-me"
            | "example"
            | "ejemplo"
            | "sample"
            | "template"
            | "placeholder"
            | "secret"
            | "password"
            | "pass"
            | "contraseña"
            | "contrasena"
            | "clave"
    ) {
        return false;
    }
    if val.starts_with('/') || val.starts_with("./") {
        return false;
    }
    if let Some((_, ext)) = val.rsplit_once('.')
        && matches!(
            ext.to_ascii_lowercase().as_str(),
            "txt" | "json" | "yml" | "yaml" | "conf" | "toml" | "log" | "sh"
        )
    {
        return false;
    }
    if key != "-p_sshpass" && val.len() < 10 && val.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    true
}

/// Validates Spanish DNI / NIE with the official Modulo 23 control letter algorithm.
pub fn is_valid_spanish_id(s: &str) -> bool {
    if s.len() != 9 {
        return false;
    }
    let bytes = s.as_bytes();
    let num_str = match bytes[0] {
        b'X' | b'x' => format!("0{}", &s[1..8]),
        b'Y' | b'y' => format!("1{}", &s[1..8]),
        b'Z' | b'z' => format!("2{}", &s[1..8]),
        c if c.is_ascii_digit() => s[..8].to_string(),
        _ => return false,
    };
    let Ok(num) = num_str.parse::<usize>() else {
        return false;
    };
    const TABLE: &[u8] = b"TRWAGMYFPDXBNJZSQVHLCKE";
    let expected = TABLE[num % 23];
    bytes[8].to_ascii_uppercase() == expected
}

/// Validates Chilean RUT with the standard Modulo 11 check digit algorithm.
pub fn is_valid_chilean_rut(digits_str: &str, verifier: char) -> bool {
    if !(7..=8).contains(&digits_str.len()) {
        return false;
    }
    let mut sum = 0;
    let mut multiplier = 2;
    for c in digits_str.chars().rev() {
        let Some(d) = c.to_digit(10) else {
            return false;
        };
        sum += d * multiplier;
        multiplier = if multiplier == 7 { 2 } else { multiplier + 1 };
    }
    let remainder = 11 - (sum % 11);
    let expected = match remainder {
        11 => '0',
        10 => 'K',
        n => match char::from_digit(n, 10) {
            Some(ch) => ch,
            None => return false,
        },
    };
    verifier.to_ascii_uppercase() == expected
}

/// Validates US Social Security Number (SSN) structural rules.
pub fn is_valid_ssn(s: &str) -> bool {
    if s.len() != 11 {
        return false;
    }
    let mut parts = s.split('-');
    let Some(area_str) = parts.next() else {
        return false;
    };
    let Some(group_str) = parts.next() else {
        return false;
    };
    let Some(serial_str) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    let (Ok(area), Ok(group), Ok(serial)) = (
        area_str.parse::<u16>(),
        group_str.parse::<u16>(),
        serial_str.parse::<u16>(),
    ) else {
        return false;
    };
    // Area cannot be 000, 666, or >= 900
    if area == 0 || area == 666 || area >= 900 {
        return false;
    }
    // Group cannot be 00
    if group == 0 {
        return false;
    }
    // Serial cannot be 0000
    if serial == 0 {
        return false;
    }
    true
}

/// Validates an IBAN using the ISO 7064 Modulo 97-10 algorithm.
pub fn is_valid_iban(iban: &str) -> bool {
    let len = iban.len();
    if !(15..=34).contains(&len) {
        return false;
    }
    let bytes = iban.as_bytes();
    if !bytes[0].is_ascii_alphabetic() || !bytes[1].is_ascii_alphabetic() {
        return false;
    }
    if !bytes[2].is_ascii_digit() || !bytes[3].is_ascii_digit() {
        return false;
    }

    // Rearrange: move first 4 characters (country + check digits) to the end
    let rearranged = [&bytes[4..], &bytes[..4]].concat();
    let mut remainder: u32 = 0;
    for &b in &rearranged {
        if b.is_ascii_digit() {
            remainder = (remainder * 10 + u32::from(b - b'0')) % 97;
        } else if b.is_ascii_alphabetic() {
            let val = u32::from(b.to_ascii_uppercase() - b'A' + 10);
            remainder = (remainder * 100 + val) % 97;
        } else {
            return false;
        }
    }
    remainder == 1
}

/// Validates that candidate card digits match an ISO/IEC 7812 Issuer Identification Number (IIN/BIN).
fn has_card_iin_prefix(d: &str) -> bool {
    let bytes = d.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    // Visa: starts with 4
    if bytes[0] == b'4' {
        return true;
    }
    // Mastercard: 51..=55, Amex: 34 or 37, Diners: 36 or 38, Discover: 65
    if bytes.len() >= 2 {
        let p2 = (bytes[0].wrapping_sub(b'0') as u16) * 10 + (bytes[1].wrapping_sub(b'0') as u16);
        if (51..=55).contains(&p2) || p2 == 34 || p2 == 37 || p2 == 36 || p2 == 38 || p2 == 65 {
            return true;
        }
    }
    // Diners: 300..=305, Discover: 644..=649
    if bytes.len() >= 3 {
        let p3 = (bytes[0].wrapping_sub(b'0') as u16) * 100
            + (bytes[1].wrapping_sub(b'0') as u16) * 10
            + (bytes[2].wrapping_sub(b'0') as u16);
        if (300..=305).contains(&p3) || (644..=649).contains(&p3) {
            return true;
        }
    }
    // Mastercard: 2221..=2720, Discover: 6011, JCB: 3528..=3589
    if bytes.len() >= 4 {
        let p4 = (bytes[0].wrapping_sub(b'0') as u16) * 1000
            + (bytes[1].wrapping_sub(b'0') as u16) * 100
            + (bytes[2].wrapping_sub(b'0') as u16) * 10
            + (bytes[3].wrapping_sub(b'0') as u16);
        if (2221..=2720).contains(&p4) || p4 == 6011 || (3528..=3589).contains(&p4) {
            return true;
        }
    }
    false
}

/// Validates credit card numbers with the standard Luhn checksum algorithm
/// combined with ISO/IEC 7812 major brand prefix verification (Visa, Mastercard, Amex, Discover, etc.).
/// Returns false for non-numeric, wrong length (< 13 or > 19), unrecognized BINs, or failing checksum.
pub fn luhn_check(digits_only: &str) -> bool {
    let len = digits_only.len();
    if !(13..=19).contains(&len) {
        return false;
    }
    // Reject all-zero sequences like 0000000000000000
    if digits_only.chars().all(|c| c == '0') {
        return false;
    }
    // Require valid card network IIN/BIN prefix to avoid false-positive masking on snowflake IDs
    if !has_card_iin_prefix(digits_only) {
        return false;
    }

    let mut sum = 0;
    let mut alternate = false;
    for c in digits_only.chars().rev() {
        let Some(d) = c.to_digit(10) else {
            return false;
        };
        let mut val = d;
        if alternate {
            val *= 2;
            if val > 9 {
                val -= 9;
            }
        }
        sum += val;
        alternate = !alternate;
    }
    sum % 10 == 0
}
