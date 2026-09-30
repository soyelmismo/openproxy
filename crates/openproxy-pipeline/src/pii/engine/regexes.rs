//! Static precompiled regexes and name dictionaries for PII detection.

use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

pub static REGEX_FENCED_CODE: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r"(?s)`````[^\n]*\n.*?`````|````[^\n]*\n.*?````|```[^\n]*\n.*?```|~~~~~[^\n]*\n.*?~~~~~|~~~~[^\n]*\n.*?~~~~|~~~[^\n]*\n.*?~~~"
    )
});

pub static REGEX_INLINE_CODE: LazyLock<Regex> =
    LazyLock::new(|| openproxy_types::static_regex!(r"`[^`\n]+`"));

pub static REGEX_URL: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r#"(?:https?|ftp|postgres(?:ql)?|mysql|mongodb(?:\+srv)?|redis|amqps?)://[^\s<>"'`]+"#,
    )
});

pub static REGEX_DATA_URI: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r"(?i)data:(?:[a-zA-Z0-9+.-]+/[a-zA-Z0-9+.-]+)?(?:;[a-zA-Z0-9+.-]+=[a-zA-Z0-9+.-]+)*;base64,[A-Za-z0-9+/=\r\n]+",
    )
});

pub static REGEX_URI_USERINFO_PASSWORD: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r#"(?:[a-zA-Z][a-zA-Z0-9+.-]{2,16}://[a-zA-Z0-9_.~%+-]*:)([^@/\s\n\r"'\\]{3,})(@[a-zA-Z0-9_.~%+-]+)"#
    )
});

pub static REGEX_URL_SENSITIVE_PARAM: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r#"(?i)[?&](?:token|api[_-]?key|secret|auth|password|passwd|access[_-]?token|refresh[_-]?token|key)=([^&#\s<>"'`\\()\[\]{}]{6,})"#
    )
});

pub static REGEX_DSN_PASSWORD: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r#"(?i)\b[a-zA-Z0-9_.~%+-]+:([^@/\s\n\r"':\\]{3,})@(?:tcp|unix|[a-zA-Z0-9_.-]+(?::[0-9]+)?)"#,
    )
});

pub static REGEX_JSON_KEY: LazyLock<Regex> =
    LazyLock::new(|| openproxy_types::static_regex!(r#""([^"\\]*(?:\\.[^"\\]*)*)"\s*:"#));

pub static REGEX_EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    // Security: include common Unicode homoglyph variants of the separators
    // (`@` U+0040 vs FULLWIDTH @ U+FF20; `.` U+002E vs FULLWIDTH FULL STOP
    // U+FF0E) and allow any Unicode letter / decimal digit in local-part
    // and domain. Zero-width characters (U+200B/C/D, U+FEFF) are matched
    // around the separators so an input like `alice\u200B@example.com`
    // does not slip past the regex. NFKC-equivalent variants are caught by
    // the alternation; pure-script Cyrillic / Greek homoglyphs in the
    // domain are caught by `\p{L}`.
    openproxy_types::static_regex!(
        r"[\p{L}\p{N}._%+-]+[\u200B\u200C\u200D\uFEFF]*(?:@|\uFF20)[\u200B\u200C\u200D\uFEFF]*[\p{L}\p{N}.-]+[\u200B\u200C\u200D\uFEFF]*(?:\.|\uFF0E)[\p{L}]{2,}"
    )
});

pub static REGEX_IPV4: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r"\b(?:(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])\.){3}(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])\b"
    )
});

pub static REGEX_IPV6: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r"(?i)\b(?:[0-9a-f]{1,4}:){7}[0-9a-f]{1,4}\b|(?i)\b(?:[0-9a-f]{1,4}:){1,6}:(?:[0-9a-f]{1,4}:){0,5}[0-9a-f]{1,4}\b|(?i)\b[0-9a-f]{1,4}::(?:[0-9a-f]{1,4}:){0,5}[0-9a-f]{1,4}\b|(?i)::(?:[0-9a-f]{1,4}:){0,6}[0-9a-f]{1,4}\b|(?:::1\b)"
    )
});

pub static REGEX_PHONE_INTL: LazyLock<Regex> = LazyLock::new(|| {
    // Security: use `\p{Nd}` (any Unicode decimal digit) instead of `\d`
    // (ASCII-only) so FULLWIDTH DIGITS U+FF11..U+FF19 are matched. Include
    // the Unicode hyphen/dash family U+2010..U+2015 + U+2212 as alternatives
    // to ASCII `-`, so `+1\u2010800\u2010555\u20100199` (HYPHEN U+2010) is
    // detected instead of leaking verbatim to the upstream provider. The
    // leading country-code digit class also accepts fullwidth 1-9
    // (U+FF11..U+FF19) so an all-fullwidth number like
    // `+\uFF11\uFF12\uFF13-\uFF15\uFF15\uFF15-\uFF10\uFF11\uFF19\uFF19`
    // is matched end-to-end.
    openproxy_types::static_regex!(
        r"\+(?:[1-9\u{FF11}-\u{FF19}]\p{Nd}{0,2})[ \-.\u2010-\u2015\u2212]?(?:\(?\p{Nd}{1,4}\)?[ \-.\u2010-\u2015\u2212]?)?\p{Nd}{2,4}[ \-.\u2010-\u2015\u2212]?\p{Nd}{2,4}(?:[ \-.\u2010-\u2015\u2212]?\p{Nd}{1,4})?"
    )
});

pub static REGEX_PHONE_REGIONAL: LazyLock<Regex> = LazyLock::new(|| {
    // Security: same `\p{Nd}` + Unicode hyphen extension as REGEX_PHONE_INTL.
    openproxy_types::static_regex!(
        r"(?:(?:\b1[-. \u2010-\u2015\u2212])?(?:\(\p{Nd}{3}\)\s*|\b\p{Nd}{3}[-. \u2010-\u2015\u2212]))\p{Nd}{3}[-. \u2010-\u2015\u2212]\p{Nd}{4}\b"
    )
});

pub static REGEX_DATE_OR_TIME: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(r"\b\d{4}[-/]\d{2}[-/]\d{2}\b|\b\d{2}:\d{2}(?::\d{2})?\b")
});

pub static REGEX_CREDIT_CARD_CANDIDATE: LazyLock<Regex> =
    LazyLock::new(|| openproxy_types::static_regex!(r"\b(?:\d[ -]*?){13,19}\b"));

pub static REGEX_SECRET_BEARER: LazyLock<Regex> =
    LazyLock::new(|| openproxy_types::static_regex!(r"\bBearer\s+([a-zA-Z0-9_\-\.\+/=]{20,})"));

pub static REGEX_SECRET_AWS: LazyLock<Regex> =
    LazyLock::new(|| openproxy_types::static_regex!(r"\b(?:AKIA|ASIA|AROA)[0-9A-Z]{16}\b"));

pub static REGEX_SECRET_OPENAI: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r"\bsk-(?:proj-|svcacct-|none-)?[a-zA-Z0-9_-]{20,}\b|\bsk-[a-zA-Z0-9]{20,}\b"
    )
});

pub static REGEX_SECRET_ANTHROPIC: LazyLock<Regex> =
    LazyLock::new(|| openproxy_types::static_regex!(r"\bsk-ant-[a-zA-Z0-9_-]{20,}\b"));

pub static REGEX_SECRET_GITHUB: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r"\b(?:ghp|gho|ghu|ghs|ghr)_[a-zA-Z0-9]{36}\b|\bgithub_pat_[a-zA-Z0-9_]{82}\b"
    )
});

pub static REGEX_SECRET_SLACK: LazyLock<Regex> =
    LazyLock::new(|| openproxy_types::static_regex!(r"\bxox[baprs]-[0-9a-zA-Z]{10,48}\b"));

pub static REGEX_SECRET_GOOGLE: LazyLock<Regex> =
    LazyLock::new(|| openproxy_types::static_regex!(r"\bAIzaSy[a-zA-Z0-9_-]{33}\b"));

pub static REGEX_SECRET_CLOUDFLARE: LazyLock<Regex> =
    LazyLock::new(|| openproxy_types::static_regex!(r"\b(?:cfat|cfa|cfpat)_[a-zA-Z0-9_-]{30,}\b"));

pub static REGEX_SECRET_TELEGRAM: LazyLock<Regex> =
    LazyLock::new(|| openproxy_types::static_regex!(r"\b\d{8,11}:[a-zA-Z0-9_-]{35}\b"));

pub static REGEX_SECRET_PRIVATE_KEY_PEM: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r"-----BEGIN (?:RSA |EC |OPENSSH |DSA |PGP )?PRIVATE KEY(?: BLOCK)?-----[\s\S]*?-----END (?:RSA |EC |OPENSSH |DSA |PGP )?PRIVATE KEY(?: BLOCK)?-----|PuTTY-User-Key-File-[0-9]:[\s\S]*?Private-Lines:\s*[0-9]+[\s\S]*?(?:\n\n|\r\n\r\n|$)"
    )
});

pub static REGEX_SECRET_BASIC_AUTH: LazyLock<Regex> =
    LazyLock::new(|| openproxy_types::static_regex!(r"\bBasic\s+([a-zA-Z0-9+/=]{16,})\b"));

pub static REGEX_SECRET_AI_CLOUD_PLATFORMS: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r"\bsk-or-v1-[a-f0-9]{64}\b|\bhf_[a-zA-Z0-9]{34,}\b|\bgsk_[a-zA-Z0-9]{52}\b|\br8_[a-zA-Z0-9]{40}\b|\b(?:sk|rk|pk|op|mcp)_(?:live|test)_[0-9a-zA-Z]{20,}\b|\b[MN][A-Za-z0-9]{23,25}\.[A-Za-z0-9_-]{6}\.[A-Za-z0-9_-]{27,}\b|\b(?:AC|SK)[a-f0-9]{32}\b"
    )
});

pub static REGEX_SECRET_CLI_FLAGS: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r#"(?:[\s"'\[,]|^)-p\s*['"]?([^'"\s\n\r,\]]{4,})['"]?|(?:[\s"'\[,]|^)-u\s+[a-zA-Z0-9_.-]+:([^'"\s\n\r,\]]{4,})"#
    )
});

pub static REGEX_SECRET_SSHPASS: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r#"\bsshpass\b[^\n\r`"']*?\s+-p(?:\s*['"]([^'"\n\r`\\]+)['"]|\s*([^\s'"\n\r`\\]+))"#
    )
});

pub static REGEX_SECRET_PLATFORM_ID: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r#"(?i)["']?\b(?:[a-z0-9_]*(?:chat|user)_?id|[a-z0-9_]*chat|telegram_?id|channel_?id|sender_?id|target_?id)\b["']?[ \t]*[:=][ \t]*["']?([0-9]{6,16})["']?|(?i)\(ID:\s*["']?([0-9]{6,16})["']?\)"#,
    )
});

pub static REGEX_SECRET_PASSWORD_HASH: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r#"(?:^|[\s"':])((?:\$2[aby]?\$|\$\$2[aby]?\$\$|\$apr1\$|\$6\$|\$argon2[a-z]*\$)[A-Za-z0-9./$]{20,})"#
    )
});

pub static REGEX_IBAN_CANDIDATE: LazyLock<Regex> =
    LazyLock::new(|| openproxy_types::static_regex!(r"\b[A-Z]{2}[0-9]{2}[A-Za-z0-9]{11,30}\b"));

pub static REGEX_NATIONAL_ID_ES: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(r"\b(?:[0-9]{8}[A-Za-z]|[XYZxyz][0-9]{7}[A-Za-z])\b")
});

pub static REGEX_NATIONAL_ID_CL: LazyLock<Regex> =
    LazyLock::new(|| openproxy_types::static_regex!(r"\b([0-9]{7,8})-([0-9kK])\b"));

pub static REGEX_NATIONAL_ID_US: LazyLock<Regex> =
    LazyLock::new(|| openproxy_types::static_regex!(r"\b\d{3}-\d{2}-\d{4}\b"));

pub static REGEX_SECRET_LABELED: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r#"(?i)["']?\b([a-z0-9_.-]*(?:password|passwd|secret|token|api_?key|private_?key|access_?key|contrase[nñ]a|clave|id[_-]?de[_-]?cuenta|account[_-]?id)[a-z0-9_.-]*)["']?[ \t]*[:=][ \t]*(?:"([^"\r\n]{4,})"|'([^'\r\n]{4,})'|([^\s"'\r\n,;\\()\[\]{}<>`|]{4,}))|(?i)\b(id\s+de\s+(?:cuenta|clave(?:\s+de\s+acceso)?)|token\s+de\s+(?:api|acceso)|clave\s+(?:de\s+acceso\s+)?secreta|clave\s+de\s+acceso)\s*[\r\n]+[ \t]*(?:"([^"\r\n]{4,})"|'([^'\r\n]{4,})'|([^\s"'\r\n,;\\()\[\]{}<>`|]{4,}))"#,
    )
});

pub static REGEX_SECRET_HIGH_ENTROPY: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(r"\b[a-zA-Z0-9_-]{32,64}\b|\b[a-zA-Z0-9+/]{30,62}={1,2}\b")
});

pub static REGEX_SECRET_JWT: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r"\beyJ[a-zA-Z0-9_-]{10,}\.eyJ[a-zA-Z0-9_-]{10,}\.[a-zA-Z0-9_-]{10,}\b"
    )
});

pub static REGEX_HONORIFIC_NAME: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r"\b(?:Mr\.|Mrs\.|Ms\.|Miss|Dr\.|Prof\.|Sir|Madam|Lord)\s+([A-Z][a-z]+(?:\s+[A-Z][a-z]+){0,2})\b"
    )
});

pub static REGEX_CAPITALIZED_SEQUENCE: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r"\b([A-Z][a-z]{1,15}[ \t]+[A-Z][a-z]{1,15}(?:[ \t]+[A-Z][a-z]{1,15})?)\b"
    )
});

pub static REGEX_CONTEXTUAL_PERSON: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r#"(?i)\b(?:User|Usuario|Autor|Author|Creator|Dueño|Dueno)\b[ \t*]*[:=][ \t*]*["']?([A-Za-z]{3,15})["']?|(?i)\b(?:DM\s+with|chat\s+con|hablar\s+con|relaci[oó]n\s+con)\s+["']?([A-Za-z]{3,15})["']?|(?i)\b(?:PC|Laptop|Desktop)\s+([A-Z][a-z]{2,15})\b"#,
    )
});

pub static COMMON_FIRST_NAMES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "Miguel",
        "Carlos",
        "Alejandro",
        "Javier",
        "Fernando",
        "Alvaro",
        "Andres",
        "Diego",
        "Mateo",
        "Gabriel",
        "Santiago",
        "Manuel",
        "Lucas",
        "Rodrigo",
        "Gonzalo",
        "Ignacio",
        "Pablo",
        "Pedro",
        "Juan",
        "Jose",
        "Luis",
        "Maria",
        "Carmen",
        "Ana",
        "Laura",
        "Sofia",
        "Isabel",
        "Elena",
        "Marta",
        "Lucia",
        "Paula",
        "Sara",
        "Claudia",
        "Beatriz",
        "Teresa",
        "Patricia",
        "David",
        "Daniel",
        "Alex",
        "Alexander",
        "Michael",
        "John",
        "James",
        "Robert",
        "William",
        "Thomas",
        "Richard",
        "Charles",
        "Joseph",
        "Sarah",
        "Emily",
        "Jessica",
        "Emma",
        "Olivia",
        "Ava",
        "Isabella",
    ]
    .into_iter()
    .collect()
});

pub static REGEX_SINGLE_WORD_CAPITALIZED: LazyLock<Regex> =
    LazyLock::new(|| openproxy_types::static_regex!(r"\b([A-Z][a-z]{2,15})\b"));
