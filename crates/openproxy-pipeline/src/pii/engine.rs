//! Native in-process PII detection and pseudonymization engine.

use openproxy_types::config::PiiEntity;
use openproxy_types::message::OpenAIMessage;
use regex::Regex;
use std::collections::HashSet;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::ops::Range;
use std::str::FromStr;
use std::sync::LazyLock;

use super::session::PiiSession;

// ── Static Precompiled Regexes (Rust 1.80+ LazyLock) ─────────────────

static REGEX_FENCED_CODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)`````[^\n]*\n.*?`````|````[^\n]*\n.*?````|```[^\n]*\n.*?```|~~~~~[^\n]*\n.*?~~~~~|~~~~[^\n]*\n.*?~~~~|~~~[^\n]*\n.*?~~~").expect("regex compilation failed")
});

static REGEX_INLINE_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"`[^`\n]+`").expect("regex compilation failed"));

static REGEX_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?:https?|ftp|postgres(?:ql)?|mysql|mongodb(?:\+srv)?|redis|amqps?)://[^\s<>"'`]+"#,
    )
    .expect("regex compilation failed")
});

static REGEX_URI_USERINFO_PASSWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:[a-zA-Z][a-zA-Z0-9+.-]{2,16}://[a-zA-Z0-9_.~%+-]*:)([^@/\s\n\r"'\\]{3,})(@[a-zA-Z0-9_.~%+-]+)"#)
        .expect("regex compilation failed")
});

static REGEX_URL_SENSITIVE_PARAM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)[?&](?:token|api[_-]?key|secret|auth|password|passwd|access[_-]?token|refresh[_-]?token|key)=([^&#\s<>"'`\\()\[\]{}]{6,})"#)
        .expect("regex compilation failed")
});

static REGEX_DSN_PASSWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b[a-zA-Z0-9_.~%+-]+:([^@/\s\n\r"':\\]{3,})@(?:tcp|unix|[a-zA-Z0-9_.-]+(?::[0-9]+)?)"#,
    )
    .expect("regex compilation failed")
});

static REGEX_JSON_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""([^"\\]*(?:\\.[^"\\]*)*)"\s*:"#).expect("regex compilation failed")
});

static REGEX_EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}").expect("regex compilation failed")
});

static REGEX_IPV4: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])\.){3}(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])\b").expect("regex compilation failed")
});

static REGEX_IPV6: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:[0-9a-f]{1,4}:){7}[0-9a-f]{1,4}\b|(?i)\b(?:[0-9a-f]{1,4}:){1,6}:(?:[0-9a-f]{1,4}:){0,5}[0-9a-f]{1,4}\b|(?i)\b[0-9a-f]{1,4}::(?:[0-9a-f]{1,4}:){0,5}[0-9a-f]{1,4}\b|(?i)::(?:[0-9a-f]{1,4}:){0,6}[0-9a-f]{1,4}\b|(?:::1\b)").expect("regex compilation failed")
});

static REGEX_PHONE_INTL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\+(?:[1-9]\d{0,2})[ -.]?(?:\(?\d{1,4}\)?[ -.]?)?\d{2,4}[ -.]?\d{2,4}(?:[ -.]?\d{1,4})?",
    )
    .expect("regex compilation failed")
});

static REGEX_PHONE_REGIONAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:(?:\b1[-. ])?(?:\(\d{3}\)\s*|\b\d{3}[-. ]))\d{3}[-. ]\d{4}\b")
        .expect("regex compilation failed")
});

static REGEX_DATE_OR_TIME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b\d{4}[-/]\d{2}[-/]\d{2}\b|\b\d{2}:\d{2}(?::\d{2})?\b")
        .expect("regex compilation failed")
});

static REGEX_CREDIT_CARD_CANDIDATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:\d[ -]*?){13,19}\b").expect("regex compilation failed"));

static REGEX_SECRET_BEARER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bBearer\s+([a-zA-Z0-9_\-\.\+/=]{20,})").expect("regex compilation failed")
});

static REGEX_SECRET_AWS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:AKIA|ASIA|AROA)[0-9A-Z]{16}\b").expect("regex compilation failed")
});

static REGEX_SECRET_OPENAI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bsk-(?:proj-|svcacct-|none-)?[a-zA-Z0-9_-]{20,}\b|\bsk-[a-zA-Z0-9]{20,}\b")
        .expect("regex compilation failed")
});

static REGEX_SECRET_ANTHROPIC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bsk-ant-[a-zA-Z0-9_-]{20,}\b").expect("regex compilation failed")
});

static REGEX_SECRET_GITHUB: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:ghp|gho|ghu|ghs|ghr)_[a-zA-Z0-9]{36}\b|\bgithub_pat_[a-zA-Z0-9_]{82}\b")
        .expect("regex compilation failed")
});

static REGEX_SECRET_SLACK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bxox[baprs]-[0-9a-zA-Z]{10,48}\b").expect("regex compilation failed")
});

static REGEX_SECRET_GOOGLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bAIzaSy[a-zA-Z0-9_-]{33}\b").expect("regex compilation failed"));

static REGEX_SECRET_CLOUDFLARE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:cfat|cfa|cfpat)_[a-zA-Z0-9_-]{30,}\b").expect("regex compilation failed")
});

static REGEX_SECRET_TELEGRAM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b\d{8,11}:[a-zA-Z0-9_-]{35}\b").expect("regex compilation failed")
});

static REGEX_SECRET_PRIVATE_KEY_PEM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-----BEGIN (?:RSA |EC |OPENSSH |DSA |PGP )?PRIVATE KEY(?: BLOCK)?-----[\s\S]*?-----END (?:RSA |EC |OPENSSH |DSA |PGP )?PRIVATE KEY(?: BLOCK)?-----|PuTTY-User-Key-File-[0-9]:[\s\S]*?Private-Lines:\s*[0-9]+[\s\S]*?(?:\n\n|\r\n\r\n|$)")
        .expect("regex compilation failed")
});

static REGEX_SECRET_BASIC_AUTH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bBasic\s+([a-zA-Z0-9+/=]{16,})\b").expect("regex compilation failed")
});

static REGEX_SECRET_AI_CLOUD_PLATFORMS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bsk-or-v1-[a-f0-9]{64}\b|\bhf_[a-zA-Z0-9]{34,}\b|\bgsk_[a-zA-Z0-9]{52}\b|\br8_[a-zA-Z0-9]{40}\b|\b(?:sk|rk|pk|op|mcp)_(?:live|test)_[0-9a-zA-Z]{20,}\b|\b[MN][A-Za-z0-9]{23,25}\.[A-Za-z0-9_-]{6}\.[A-Za-z0-9_-]{27,}\b|\b(?:AC|SK)[a-f0-9]{32}\b").expect("regex compilation failed")
});

static REGEX_SECRET_CLI_FLAGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:[\s"'\[,]|^)-p\s*['"]?([^'"\s\n\r,\]]{4,})['"]?|(?:[\s"'\[,]|^)-u\s+[a-zA-Z0-9_.-]+:([^'"\s\n\r,\]]{4,})"#).expect("regex compilation failed")
});

static REGEX_SECRET_SSHPASS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\bsshpass\b[^\n\r`"']*?\s+-p(?:\s*['"]([^'"\n\r`\\]+)['"]|\s*([^\s'"\n\r`\\]+))"#)
        .expect("regex compilation failed")
});

static REGEX_SECRET_PLATFORM_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)["']?\b(?:[a-z0-9_]*(?:chat|user)_?id|[a-z0-9_]*chat|telegram_?id|channel_?id|sender_?id|target_?id)\b["']?[ \t]*[:=][ \t]*["']?([0-9]{6,16})["']?|(?i)\(ID:\s*["']?([0-9]{6,16})["']?\)"#,
    )
    .expect("regex compilation failed")
});

static REGEX_SECRET_PASSWORD_HASH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:^|[\s"':])((?:\$2[aby]?\$|\$\$2[aby]?\$\$|\$apr1\$|\$6\$|\$argon2[a-z]*\$)[A-Za-z0-9./$]{20,})"#)
        .expect("regex compilation failed")
});

static REGEX_IBAN_CANDIDATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Z]{2}[0-9]{2}[A-Za-z0-9]{11,30}\b").expect("regex compilation failed")
});

static REGEX_NATIONAL_ID_ES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:[0-9]{8}[A-Za-z]|[XYZxyz][0-9]{7}[A-Za-z])\b")
        .expect("regex compilation failed")
});

static REGEX_NATIONAL_ID_CL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([0-9]{7,8})-([0-9kK])\b").expect("regex compilation failed"));

static REGEX_NATIONAL_ID_US: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").expect("regex compilation failed"));

static REGEX_SECRET_LABELED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)["']?\b([a-z0-9_.-]*(?:password|passwd|secret|token|api_?key|private_?key|access_?key|contrase[nñ]a|clave|id[_-]?de[_-]?cuenta|account[_-]?id)[a-z0-9_.-]*)["']?[ \t]*[:=][ \t]*(?:"([^"\r\n]{4,})"|'([^'\r\n]{4,})'|([^\s"'\r\n,;\\()\[\]{}<>`|]{4,}))|(?i)\b(id\s+de\s+(?:cuenta|clave(?:\s+de\s+acceso)?)|token\s+de\s+(?:api|acceso)|clave\s+(?:de\s+acceso\s+)?secreta|clave\s+de\s+acceso)\s*[\r\n]+[ \t]*(?:"([^"\r\n]{4,})"|'([^'\r\n]{4,})'|([^\s"'\r\n,;\\()\[\]{}<>`|]{4,}))"#,
    )
    .expect("regex compilation failed")
});

static REGEX_SECRET_HIGH_ENTROPY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[a-zA-Z0-9_-]{32,64}\b|\b[a-zA-Z0-9+/]{30,62}={1,2}\b")
        .expect("regex compilation failed")
});

static REGEX_SECRET_JWT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\beyJ[a-zA-Z0-9_-]{10,}\.eyJ[a-zA-Z0-9_-]{10,}\.[a-zA-Z0-9_-]{10,}\b")
        .expect("regex compilation failed")
});

static REGEX_HONORIFIC_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:Mr\.|Mrs\.|Ms\.|Miss|Dr\.|Prof\.|Sir|Madam|Lord)\s+([A-Z][a-z]+(?:\s+[A-Z][a-z]+){0,2})\b").expect("regex compilation failed")
});

static REGEX_CAPITALIZED_SEQUENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([A-Z][a-z]{1,15}[ \t]+[A-Z][a-z]{1,15}(?:[ \t]+[A-Z][a-z]{1,15})?)\b")
        .expect("regex compilation failed")
});

static REGEX_CONTEXTUAL_PERSON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b(?:User|Usuario|Autor|Author|Creator|Dueño|Dueno)\b[ \t*]*[:=][ \t*]*["']?([A-Za-z]{3,15})["']?|(?i)\b(?:DM\s+with|chat\s+con|hablar\s+con|relaci[oó]n\s+con)\s+["']?([A-Za-z]{3,15})["']?|(?i)\b(?:PC|Laptop|Desktop)\s+([A-Z][a-z]{2,15})\b"#,
    )
    .expect("regex compilation failed")
});

static COMMON_FIRST_NAMES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "Miguel", "Carlos", "Alejandro", "Javier", "Fernando", "Alvaro", "Andres", "Diego",
        "Mateo", "Gabriel", "Santiago", "Manuel", "Lucas", "Rodrigo", "Gonzalo", "Ignacio",
        "Pablo", "Pedro", "Juan", "Jose", "Luis", "Maria", "Carmen", "Ana", "Laura", "Sofia",
        "Isabel", "Elena", "Marta", "Lucia", "Paula", "Sara", "Claudia", "Beatriz", "Teresa",
        "Patricia", "David", "Daniel", "Alex", "Alexander", "Michael", "John", "James", "Robert",
        "William", "Thomas", "Richard", "Charles", "Joseph", "Sarah", "Emily", "Jessica", "Emma",
        "Olivia", "Ava", "Isabella",
    ]
    .into_iter()
    .collect()
});

static REGEX_SINGLE_WORD_CAPITALIZED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([A-Z][a-z]{2,15})\b").expect("regex compilation failed")
});

// ── Shannon Entropy Calculation ──────────────────────────────────────

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

// ── Luhn Checksum Algorithm & Card Brand BIN Validation ───────────

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

// ── Stopwords for Person Name Heuristic ───────────────────────────────

fn is_stopword(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "the"
            | "this"
            | "that"
            | "there"
            | "these"
            | "those"
            | "here"
            | "where"
            | "when"
            | "what"
            | "which"
            | "who"
            | "why"
            | "how"
            | "if"
            | "then"
            | "else"
            | "because"
            | "although"
            | "since"
            | "after"
            | "before"
            | "while"
            | "under"
            | "over"
            | "above"
            | "below"
            | "between"
            | "into"
            | "through"
            | "during"
            | "without"
            | "within"
            | "from"
            | "about"
            | "against"
            | "with"
            | "and"
            | "but"
            | "or"
            | "nor"
            | "so"
            | "yet"
            | "both"
            | "either"
            | "neither"
            | "not"
            | "only"
            | "just"
            | "also"
            | "even"
            | "ever"
            | "never"
            | "always"
            | "often"
            | "sometimes"
            | "usually"
            | "really"
            | "very"
            | "quite"
            | "almost"
            | "already"
            | "still"
            | "again"
            | "once"
            | "twice"
            | "all"
            | "any"
            | "some"
            | "every"
            | "each"
            | "few"
            | "many"
            | "much"
            | "more"
            | "most"
            | "other"
            | "another"
            | "such"
            | "same"
            | "different"
            | "new"
            | "old"
            | "good"
            | "bad"
            | "high"
            | "low"
            | "big"
            | "small"
            | "large"
            | "great"
            | "little"
            | "long"
            | "short"
            | "right"
            | "left"
            | "first"
            | "last"
            | "next"
            | "early"
            | "late"
            | "best"
            | "worst"
            | "true"
            | "false"
            | "null"
            | "none"
            | "error"
            | "warning"
            | "info"
            | "debug"
            | "trace"
            | "status"
            | "message"
            | "result"
            | "request"
            | "response"
            | "data"
            | "file"
            | "path"
            | "directory"
            | "user"
            | "system"
            | "client"
            | "server"
            | "model"
            | "config"
            | "type"
            | "name"
            | "value"
            | "key"
            | "item"
            | "list"
            | "array"
            | "map"
            | "object"
            | "string"
            | "number"
            | "boolean"
            | "default"
            | "global"
            | "local"
            | "public"
            | "private"
            | "static"
            | "final"
            | "const"
            | "let"
            | "var"
            | "function"
            | "class"
            | "struct"
            | "interface"
            | "enum"
            | "impl"
            | "trait"
            | "return"
            | "break"
            | "continue"
            | "match"
            | "case"
            | "switch"
            | "select"
            | "group"
            | "order"
            | "limit"
            | "offset"
            | "join"
            | "inner"
            | "outer"
            | "full"
            | "cross"
            | "union"
            | "insert"
            | "update"
            | "delete"
            | "create"
            | "drop"
            | "alter"
            | "table"
            | "view"
            | "index"
            | "database"
            | "schema"
            | "http"
            | "https"
            | "tcp"
            | "udp"
            | "ip"
            | "dns"
            | "url"
            | "uri"
            | "api"
            | "rest"
            | "json"
            | "xml"
            | "html"
            | "css"
            | "sql"
            | "git"
            | "linux"
            | "windows"
            | "macos"
            | "android"
            | "ios"
            | "openai"
            | "anthropic"
            | "google"
            | "claude"
            | "gemini"
            | "gpt"
            | "llama"
            | "mistral"
            | "rust"
            | "python"
            | "javascript"
            | "typescript"
            | "java"
            | "ruby"
            | "swift"
            | "kotlin"
            | "docker"
            | "kubernetes"
            | "mariadb"
            | "mysql"
            | "postgres"
            | "postgresql"
            | "redis"
            | "sqlite"
            | "mongodb"
            | "aws"
            | "azure"
            | "cloudflare"
            | "january"
            | "february"
            | "march"
            | "april"
            | "may"
            | "june"
            | "july"
            | "august"
            | "september"
            | "october"
            | "november"
            | "december"
            | "monday"
            | "tuesday"
            | "wednesday"
            | "thursday"
            | "friday"
            | "saturday"
            | "sunday"
            | "please"
            | "hello"
            | "thanks"
            | "dear"
            | "regards"
            | "sincerely"
            | "hi"
            | "hey"
            | "welcome"
            | "in"
            | "on"
            | "at"
            | "to"
            | "by"
            | "as"
            | "an"
            | "is"
            | "am"
            | "are"
            | "was"
            | "were"
            | "be"
            | "been"
            | "has"
            | "have"
            | "had"
            | "do"
            | "does"
            | "did"
            | "can"
            | "could"
            | "will"
            | "would"
            | "shall"
            | "should"
            | "might"
            | "must"
            | "my"
            | "your"
            | "his"
            | "her"
            | "its"
            | "our"
            | "their"
            | "we"
            | "he"
            | "she"
            | "it"
            | "they"
            | "them"
            | "us"
            | "me"
            | "him"
    )
}

// ── Match Candidate ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub(crate) struct Candidate<'a> {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) entity: PiiEntity,
    pub(crate) matched_text: &'a str,
}

// ── PiiEngine ─────────────────────────────────────────────────────────

/// Native, deterministic PII detection and redaction engine.
#[derive(Debug, Clone)]
pub struct PiiEngine {
    entities: Vec<PiiEntity>,
}

impl PiiEngine {
    pub fn new(entities: &[PiiEntity]) -> Self {
        Self {
            entities: entities.to_vec(),
        }
    }

    pub fn from_config(config: &openproxy_types::config::PiiConfig) -> Self {
        Self::new(&config.pii_entities)
    }

    fn has_entity(&self, entity: PiiEntity) -> bool {
        self.entities.contains(&entity)
    }

    /// Identify syntax-protected spans (URLs with carveouts, JSON keys)
    /// that must not have their structural boundaries corrupted.
    pub fn find_syntax_protected_ranges(&self, text: &str) -> Vec<Range<usize>> {
        let mut ranges = Vec::new();

        // 1. URLs (https://..., ftp://..., postgres://..., etc.)
        for m in REGEX_URL.find_iter(text) {
            let url_str = m.as_str();
            let mut carveouts: Vec<Range<usize>> = Vec::new();

            // A. Password in userinfo: postgres://user:password@host
            if let Some(cap) = REGEX_URI_USERINFO_PASSWORD.captures(url_str)
                && let Some(pass_m) = cap.get(1)
            {
                carveouts.push(m.start() + pass_m.start()..m.start() + pass_m.end());
            }

            // B. Sensitive query parameters: https://host/path?token=SECRET_VALUE&...
            for cap in REGEX_URL_SENSITIVE_PARAM.captures_iter(url_str) {
                if let Some(param_val) = cap.get(1) {
                    carveouts.push(m.start() + param_val.start()..m.start() + param_val.end());
                }
            }

            if carveouts.is_empty() {
                ranges.push(m.range());
            } else {
                carveouts.sort_by_key(|r| r.start);
                let mut curr = m.start();
                for r in carveouts {
                    if r.start > curr {
                        ranges.push(curr..r.start);
                    }
                    curr = curr.max(r.end);
                }
                if curr < m.end() {
                    ranges.push(curr..m.end());
                }
            }
        }

        // 2. JSON keys ("key":)
        for cap in REGEX_JSON_KEY.captures_iter(text) {
            if let Some(key_match) = cap.get(1) {
                ranges.push(key_match.range());
            }
        }

        Self::merge_ranges(ranges)
    }

    /// Identify code blocks (fenced ``` and inline `) to avoid heuristic false-positives
    /// on code variables, types, or syntax.
    pub fn find_code_protected_ranges(&self, text: &str) -> Vec<Range<usize>> {
        let mut ranges = Vec::new();

        // 1. Fenced code blocks ```...``` and ~~~...~~~
        for m in REGEX_FENCED_CODE.find_iter(text) {
            ranges.push(m.range());
        }

        // 2. Inline code `...`
        for m in REGEX_INLINE_CODE.find_iter(text) {
            ranges.push(m.range());
        }

        Self::merge_ranges(ranges)
    }

    /// Identify all protected spans (combining syntax and code protection).
    pub fn find_protected_ranges(&self, text: &str) -> Vec<Range<usize>> {
        let mut ranges = self.find_syntax_protected_ranges(text);
        ranges.extend(self.find_code_protected_ranges(text));
        Self::merge_ranges(ranges)
    }

    fn merge_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
        if ranges.len() <= 1 {
            return ranges;
        }
        ranges.sort_by_key(|r| r.start);
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
        for r in ranges {
            if let Some(last) = merged.last_mut()
                && r.start <= last.end
            {
                last.end = last.end.max(r.end);
                continue;
            }
            merged.push(r);
        }
        merged
    }

    fn is_in_protected_range(ranges: &[Range<usize>], start: usize, end: usize) -> bool {
        let idx = ranges.partition_point(|r| r.end <= start);
        if idx < ranges.len() {
            start < ranges[idx].end && end > ranges[idx].start
        } else {
            false
        }
    }

    /// Collect all raw candidates across enabled entity types.
    pub(crate) fn collect_candidates<'a>(
        &self,
        text: &'a str,
        syntax_protected: &[Range<usize>],
        code_protected: &[Range<usize>],
    ) -> Vec<Candidate<'a>> {
        let mut candidates = Vec::new();

        // ── 1. Secrets / API Keys ──
        if self.has_entity(PiiEntity::Secret) {
            for cap in REGEX_SECRET_BEARER.captures_iter(text) {
                if let Some(m) = cap.get(1) {
                    let start = m.start();
                    let raw = m.as_str();
                    let trimmed = raw.trim_end_matches('.');
                    let end = start + trimmed.len();
                    if trimmed.len() >= 20 && !Self::is_in_protected_range(syntax_protected, start, end) {
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
                if !Self::is_in_protected_range(syntax_protected, m.start(), m.end()) {
                    candidates.push(Candidate {
                        start: m.start(),
                        end: m.end(),
                        entity: PiiEntity::Secret,
                        matched_text: m.as_str(),
                    });
                }
            }
            for m in REGEX_SECRET_OPENAI.find_iter(text) {
                if !Self::is_in_protected_range(syntax_protected, m.start(), m.end()) {
                    candidates.push(Candidate {
                        start: m.start(),
                        end: m.end(),
                        entity: PiiEntity::Secret,
                        matched_text: m.as_str(),
                    });
                }
            }
            for m in REGEX_SECRET_ANTHROPIC.find_iter(text) {
                if !Self::is_in_protected_range(syntax_protected, m.start(), m.end()) {
                    candidates.push(Candidate {
                        start: m.start(),
                        end: m.end(),
                        entity: PiiEntity::Secret,
                        matched_text: m.as_str(),
                    });
                }
            }
            for m in REGEX_SECRET_GITHUB.find_iter(text) {
                if !Self::is_in_protected_range(syntax_protected, m.start(), m.end()) {
                    candidates.push(Candidate {
                        start: m.start(),
                        end: m.end(),
                        entity: PiiEntity::Secret,
                        matched_text: m.as_str(),
                    });
                }
            }
            for m in REGEX_SECRET_SLACK.find_iter(text) {
                if !Self::is_in_protected_range(syntax_protected, m.start(), m.end()) {
                    candidates.push(Candidate {
                        start: m.start(),
                        end: m.end(),
                        entity: PiiEntity::Secret,
                        matched_text: m.as_str(),
                    });
                }
            }
            for m in REGEX_SECRET_GOOGLE.find_iter(text) {
                if !Self::is_in_protected_range(syntax_protected, m.start(), m.end()) {
                    candidates.push(Candidate {
                        start: m.start(),
                        end: m.end(),
                        entity: PiiEntity::Secret,
                        matched_text: m.as_str(),
                    });
                }
            }
            for m in REGEX_SECRET_CLOUDFLARE.find_iter(text) {
                if !Self::is_in_protected_range(syntax_protected, m.start(), m.end()) {
                    candidates.push(Candidate {
                        start: m.start(),
                        end: m.end(),
                        entity: PiiEntity::Secret,
                        matched_text: m.as_str(),
                    });
                }
            }
            for m in REGEX_SECRET_TELEGRAM.find_iter(text) {
                if !Self::is_in_protected_range(syntax_protected, m.start(), m.end()) {
                    candidates.push(Candidate {
                        start: m.start(),
                        end: m.end(),
                        entity: PiiEntity::Secret,
                        matched_text: m.as_str(),
                    });
                }
            }
            for m in REGEX_SECRET_PRIVATE_KEY_PEM.find_iter(text) {
                if !Self::is_in_protected_range(syntax_protected, m.start(), m.end()) {
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
                    if !Self::is_in_protected_range(syntax_protected, start, end) {
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
                        && !Self::is_in_protected_range(syntax_protected, start, end)
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
                if is_high_entropy_secret(s)
                    && !Self::is_in_protected_range(syntax_protected, m.start(), m.end())
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
                if !Self::is_in_protected_range(syntax_protected, m.start(), m.end()) {
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
                    if !Self::is_in_protected_range(syntax_protected, start, end) {
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
                        && !Self::is_in_protected_range(syntax_protected, start, end)
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
                    if !Self::is_in_protected_range(syntax_protected, start, end) {
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
                    if !Self::is_in_protected_range(syntax_protected, start, end) {
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
                if !Self::is_in_protected_range(syntax_protected, m.start(), m.end()) {
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
                        && !Self::is_in_protected_range(syntax_protected, start, end)
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
                        && !Self::is_in_protected_range(syntax_protected, start, end)
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
                    if !Self::is_in_protected_range(syntax_protected, start, end) {
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
                if is_valid_spanish_id(s)
                    && !Self::is_in_protected_range(syntax_protected, m.start(), m.end())
                {
                    candidates.push(Candidate {
                        start: m.start(),
                        end: m.end(),
                        entity: PiiEntity::Secret,
                        matched_text: s,
                    });
                }
            }
            for cap in REGEX_NATIONAL_ID_CL.captures_iter(text) {
                if let (Some(m_full), Some(m_digits), Some(m_verif)) =
                    (cap.get(0), cap.get(1), cap.get(2))
                {
                    let verif_char = m_verif.as_str().chars().next().unwrap_or('?');
                    if is_valid_chilean_rut(m_digits.as_str(), verif_char)
                        && !Self::is_in_protected_range(syntax_protected, m_full.start(), m_full.end())
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
                if is_valid_ssn(s) && !Self::is_in_protected_range(syntax_protected, m.start(), m.end()) {
                    candidates.push(Candidate {
                        start: m.start(),
                        end: m.end(),
                        entity: PiiEntity::Secret,
                        matched_text: s,
                    });
                }
            }
        }

        // ── 2. Email Addresses ──
        if self.has_entity(PiiEntity::Email) {
            for m in REGEX_EMAIL.find_iter(text) {
                if !Self::is_in_protected_range(syntax_protected, m.start(), m.end()) {
                    candidates.push(Candidate {
                        start: m.start(),
                        end: m.end(),
                        entity: PiiEntity::Email,
                        matched_text: m.as_str(),
                    });
                }
            }
        }

        // ── 3. Credit Card Numbers (validated with Luhn) ──
        if self.has_entity(PiiEntity::CreditCard) {
            for m in REGEX_CREDIT_CARD_CANDIDATE.find_iter(text) {
                // Reject if preceded by another hyphenated/spaced digit sequence (e.g. 9999-4532-0151-1283-0366)
                if m.start() > 0 {
                    let head = &text[..m.start()];
                    if head.ends_with('-') || head.ends_with(' ') {
                        let trimmed = head.trim_end_matches(['-', ' ']);
                        if trimmed.ends_with(|c: char| c.is_ascii_digit()) {
                            continue;
                        }
                    }
                }
                // Reject if immediately followed by another hyphenated/spaced digit sequence
                // (e.g. 20+ digit product keys or serial numbers like 4532-0151-1283-0366-1234)
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
                if luhn_check(&digits)
                    && !Self::is_in_protected_range(syntax_protected, m.start(), m.end())
                {
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
                if is_valid_iban(s) && !Self::is_in_protected_range(syntax_protected, m.start(), m.end()) {
                    candidates.push(Candidate {
                        start: m.start(),
                        end: m.end(),
                        entity: PiiEntity::CreditCard,
                        matched_text: s,
                    });
                }
            }
        }

        // ── 4. IP Addresses (IPv4 & IPv6) ──
        if self.has_entity(PiiEntity::Ip) {
            for m in REGEX_IPV4.find_iter(text) {
                let s = m.as_str();
                // Avoid semver like v1.2.3.4 or V1.2.3.4
                let is_semver = m.start() > 0
                    && (text.as_bytes()[m.start() - 1] == b'v'
                        || text.as_bytes()[m.start() - 1] == b'V');
                if is_semver {
                    continue;
                }
                // Avoid multi-dot OIDs or extended version numbers like 1.3.6.1.4.1 or 1.2.3.4.5
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
                    && !Self::is_in_protected_range(syntax_protected, m.start(), m.end())
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
                let s = m.as_str();
                if s.contains(':')
                    && Ipv6Addr::from_str(s).is_ok()
                    && !Self::is_in_protected_range(syntax_protected, m.start(), m.end())
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

        // ── 5. Phone Numbers ──
        if self.has_entity(PiiEntity::Phone) {
            for m in REGEX_PHONE_INTL.find_iter(text) {
                // Reject if preceded by alphanumeric (e.g. formula x+1-555-123-4567 or C++1-555-123-4567)
                if m.start() > 0 && text.as_bytes()[m.start() - 1].is_ascii_alphanumeric() {
                    continue;
                }
                // Reject if immediately followed by more digits
                if m.end() < text.len() && text.as_bytes()[m.end()].is_ascii_digit() {
                    continue;
                }
                // Reject if followed by separator + more digits (e.g. Serial +1-800-555-0199-99999)
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
                let digit_count = s.chars().filter(|c| c.is_ascii_digit()).count();
                if (7..=15).contains(&digit_count)
                    && !REGEX_DATE_OR_TIME.is_match(s)
                    && !Self::is_in_protected_range(syntax_protected, m.start(), m.end())
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
                // Check boundaries to avoid matching middle/prefixes of longer serial keys
                if m.start() > 0 {
                    let prev = text.as_bytes()[m.start() - 1];
                    if prev.is_ascii_alphanumeric()
                        || matches!(prev, b'+' | b'*' | b'/' | b'=' | b'%')
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
                let digit_count = s.chars().filter(|c| c.is_ascii_digit()).count();
                if (7..=15).contains(&digit_count)
                    && !REGEX_DATE_OR_TIME.is_match(s)
                    && !Self::is_in_protected_range(syntax_protected, m.start(), m.end())
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

        // ── 6. Person Names ──
        if self.has_entity(PiiEntity::Person) {
            // A. Contextual person markers (User: "miguel", DM with miguel, PC Miguel, etc.)
            for cap in REGEX_CONTEXTUAL_PERSON.captures_iter(text) {
                let m = cap.get(1).or_else(|| cap.get(2)).or_else(|| cap.get(3));
                if let Some(m) = m {
                    let s = m.as_str();
                    if !is_stopword(s)
                        && !Self::is_in_protected_range(syntax_protected, m.start(), m.end())
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
                        && !Self::is_in_protected_range(syntax_protected, m.start(), m.end())
                        && !Self::is_in_protected_range(code_protected, m.start(), m.end())
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
                        && !Self::is_in_protected_range(syntax_protected, m.start(), m.end())
                        && !Self::is_in_protected_range(code_protected, m.start(), m.end())
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

            // D. Capitalized sequence heuristic
            for cap in REGEX_CAPITALIZED_SEQUENCE.captures_iter(text) {
                if let Some(m) = cap.get(1) {
                    let s = m.as_str();
                    let words: Vec<&str> = s.split_whitespace().collect();
                    if words.len() >= 2 && words.iter().all(|w| !is_stopword(w)) {
                        // Check if preceded by sentence-ending punctuation without honorific
                        let is_sentence_start = if m.start() == 0 {
                            true
                        } else {
                            let prefix = text[..m.start()].trim_end();
                            prefix.ends_with('.')
                                || prefix.ends_with('!')
                                || prefix.ends_with('?')
                                || prefix.ends_with('\n')
                        };

                        // Avoid matching pure sentence starters like "Great Results"
                        if !is_sentence_start
                            && !Self::is_in_protected_range(syntax_protected, m.start(), m.end())
                            && !Self::is_in_protected_range(code_protected, m.start(), m.end())
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

        candidates
    }

    /// Redact detected PII in `text`, replacing them with stable per-request placeholders.
    pub fn redact_text(&self, text: &str, session: &mut PiiSession) -> String {
        if text.is_empty() || self.entities.is_empty() {
            return text.to_string();
        }

        session.seed_existing_placeholders(text);

        let syntax_protected = self.find_syntax_protected_ranges(text);
        let code_protected = self.find_code_protected_ranges(text);
        let mut candidates = self.collect_candidates(text, &syntax_protected, &code_protected);

        if candidates.is_empty() {
            return text.to_string();
        }

        // Sort by start asc, longer match first if same start
        candidates.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| b.end.cmp(&a.end)));

        // Remove overlapping candidates
        let mut non_overlapping = Vec::with_capacity(candidates.len());
        let mut last_end = 0;
        for cand in candidates {
            if cand.start >= last_end {
                last_end = cand.end;
                non_overlapping.push(cand);
            }
        }

        let mut out = String::with_capacity(text.len() + 32);
        let mut curr = 0;

        for cand in non_overlapping {
            out.push_str(&text[curr..cand.start]);
            let placeholder = session.get_or_create_placeholder(cand.entity, cand.matched_text);
            out.push_str(&placeholder);
            curr = cand.end;
        }

        out.push_str(&text[curr..]);
        out
    }

    /// Recursively redact strings inside a serde_json::Value.
    /// Structural JSON keys and function identifier names (id, type, name) are preserved.
    /// If a string is itself serialized JSON (e.g. tool_calls.arguments), it is parsed,
    /// recursively redacted at leaf strings, and re-serialized, guaranteeing valid JSON escaping.
    pub fn redact_json_value(&self, val: &mut serde_json::Value, session: &mut PiiSession) {
        match val {
            serde_json::Value::String(s) => {
                let trimmed = s.trim();
                if ((trimmed.starts_with('{') && trimmed.ends_with('}'))
                    || (trimmed.starts_with('[') && trimmed.ends_with(']')))
                    && let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(s)
                {
                    self.redact_json_value(&mut parsed, session);
                    if let Ok(serialized) = serde_json::to_string(&parsed) {
                        *s = serialized;
                        return;
                    }
                }
                let redacted = self.redact_text(s, session);
                *s = redacted;
            }
            serde_json::Value::Array(arr) => {
                for item in arr {
                    self.redact_json_value(item, session);
                }
            }
            serde_json::Value::Object(map) => {
                for (k, v) in map.iter_mut() {
                    if k == "id" || k == "type" || k == "name" {
                        continue;
                    }
                    self.redact_json_value(v, session);
                }
            }
            _ => {}
        }
    }

    /// Redact messages (system, developer, user, assistant, tool, function) and tool_calls
    /// in a slice of OpenAIMessage.
    pub fn redact_messages(
        &self,
        messages: &[OpenAIMessage],
        session: &mut PiiSession,
    ) -> Vec<OpenAIMessage> {
        messages
            .iter()
            .map(|msg| {
                let mut cloned = msg.clone();

                // Intercept content of all messages
                if let Some(ref mut content) = cloned.content {
                    self.redact_json_value(content, session);
                }

                // Intercept extra fields (e.g. reasoning_content, thought)
                for (k, v) in &mut cloned.extra {
                    if k == "reasoning_content" || k == "thought" {
                        self.redact_json_value(v, session);
                    }
                }

                // Intercept tool_calls arguments in any message
                if let Some(ref mut tool_calls) = cloned.tool_calls {
                    for tc in tool_calls.iter_mut() {
                        self.redact_json_value(tc, session);
                    }
                }

                cloned
            })
            .collect()
    }
}
