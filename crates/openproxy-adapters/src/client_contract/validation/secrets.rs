//! Credential-material heuristics applied before anything is persisted.
//!
//! A credential *name* on its own is documentation and is allowed; what is
//! rejected is a name bound to a value: `Authorization: Bearer sk-...`,
//! `session_id=abc`, `token: ...`.

/// Credential names bound to a value: `authorization:`, `session_id=`,
/// `token:`. A name alone is documentation and is allowed; the credential is
/// the value the name is bound to.
const SECRET_MARKERS: &[&str] = &[
    "authorization:",
    "proxy-authorization:",
    "www-authenticate:",
    "set-cookie:",
    "cookie:",
    "x-api-key:",
    "api-key:",
    "api_key:",
    "apikey:",
    "access_token=",
    "access_token:",
    "refresh_token=",
    "refresh_token:",
    "client_secret=",
    "client_secret:",
    "session_id=",
    "session_id:",
    "session-id=",
    "session-id:",
    "sessionid=",
    "sessionid:",
    "password=",
    "password:",
    "secret=",
    "secret:",
    "token=",
    "token:",
];

/// The same credential names as JSON keys. Payloads and catalogs are JSON, so
/// a leak there appears as `"token":"..."` — a quote sits between name and
/// colon, which the plain markers above cannot see.
///
/// Each entry is the quoted name plus its colon, so a longer key such as
/// `"authorization_header"` does not match. Whether the slot actually carries
/// a value is decided by [`json_key_binds_a_value`].
const JSON_SECRET_KEYS: &[&str] = &[
    "\"access_token\":",
    "\"refresh_token\":",
    "\"client_secret\":",
    "\"session_id\":",
    "\"sessionid\":",
    "\"session-token\":",
    "\"password\":",
    "\"secret\":",
    "\"token\":",
    "\"api_key\":",
    "\"api-key\":",
    "\"apikey\":",
    "\"session-id\":",
    "\"authorization\":",
    "\"proxy-authorization\":",
    "\"set-cookie\":",
    "\"cookie\":",
    "\"x-api-key\":",
    "\"x-auth-token\":",
];

/// Authentication schemes. A scheme marks a credential only when it is bound
/// to a base64-shaped value, so the word `basic` inside a model description is
/// not a leak while `Basic dXNlcjpwYXNz` still is.
const AUTH_SCHEMES: &[&str] = &["bearer ", "basic "];

/// Whether `bytes` carry credential material. Case-insensitive; used on
/// payloads, sources and transport bodies before anything is persisted.
///
/// A credential *name* on its own is documentation and is allowed. What is
/// rejected is a name bound to a value: `Authorization: Bearer sk-...`,
/// `session_id=abc`, `token: ...`. Header names are never hidden, renamed or
/// hashed to pass this check; the check is simply value-sensitive.
pub fn contains_secret_material(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    let lower = text.to_ascii_lowercase();
    if SECRET_MARKERS.iter().any(|marker| lower.contains(marker)) {
        return true;
    }
    if JSON_SECRET_KEYS
        .iter()
        .any(|key| json_key_binds_a_value(&text, &lower, key))
    {
        return true;
    }
    AUTH_SCHEMES
        .iter()
        .any(|scheme| scheme_binds_a_credential(&text, &lower, scheme))
}

/// Whether a JSON credential key carries a value: `"token":"abc"` does,
/// `"token":""` and `"token":null` are placeholders and do not.
fn json_key_binds_a_value(text: &str, lower: &str, key: &str) -> bool {
    let mut from = 0;
    while let Some(index) = lower[from..].find(key) {
        let after = from + index + key.len();
        let value = text[after..].trim_start();
        let binds = match value.chars().next() {
            None => false,
            // A quoted value that is not immediately closed is a real value.
            Some('"') => !value[1..].starts_with('"'),
            // `null` / `false` are placeholders, not credentials.
            Some('n' | 'f') => false,
            Some(_) => true,
        };
        if binds {
            return true;
        }
        from = after;
    }
    false
}

/// Whether `text` contains `scheme` immediately followed by a base64-shaped
/// credential: 12+ characters with two or more upper-case letters, `+`/`/`/`=`
/// padding, or a digit next to an upper-case letter.
///
/// The scheme is matched case-insensitively through `lower` (same byte length
/// as `text`, so offsets map 1:1) while the token itself is measured on the
/// original bytes: case carries the signal. Prose that merely mentions the
/// word — `basic searches`, `Bearer authentication` — carries an ordinary word
/// instead and is not a credential. Documented as a heuristic on purpose: a
/// credential *value* is always caught by [`SECRET_MARKERS`] first.
fn scheme_binds_a_credential(text: &str, lower: &str, scheme: &str) -> bool {
    let mut from = 0;
    while let Some(index) = lower[from..].find(scheme) {
        let after = from + index + scheme.len();
        let token: String = text[after..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '-' | '_'))
            .collect();
        if is_credential_shaped(&token) {
            return true;
        }
        from = after;
    }
    false
}

/// Base64 credential shape: long enough to carry a secret, and carrying at
/// least one marker that plain prose does not — padding, an alphabet special,
/// several upper-case letters, or a digit beside an upper-case letter.
fn is_credential_shaped(token: &str) -> bool {
    if token.len() < 12 {
        return false;
    }
    let upper = token.chars().filter(|c| c.is_ascii_uppercase()).count();
    let has_digit = token.chars().any(|c| c.is_ascii_digit());
    let has_special = token.chars().any(|c| matches!(c, '+' | '/' | '='));
    has_special || upper >= 2 || (has_digit && upper >= 1)
}
