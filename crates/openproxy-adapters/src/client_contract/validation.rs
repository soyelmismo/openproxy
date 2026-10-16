//! Structural validation, limits and anti-downgrade for contract candidates.
//!
//! Nothing here knows a provider payload shape. The base enforces the rules any
//! contract must satisfy (size, digests, header hygiene, no secrets, stable
//! SemVer within one channel); provider-specific semantics live in the
//! [`ContractValidator`] a store is built with.

use std::cmp::Ordering;

use super::{
    Candidate, CatalogRef, ClientIdentity, ContractError, ContractId, EvidenceKind,
    IDENTITY_ORIGIN_OFFICIAL, IDENTITY_ORIGIN_OWN, Snapshot, SourceRef, canonical_bytes,
    payload_digest,
};

/// Design limits of the common base. They are envelope limits, not upstream
/// requirements: a provider may tighten them, never loosen them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContractLimits {
    /// Maximum serialized profile payload, measured after decompression.
    pub profile_bytes: usize,
    /// Maximum serialized catalog, when a payload carries one.
    pub catalog_bytes: usize,
    /// Maximum number of models inside a catalog.
    pub catalog_models: usize,
    /// Maximum raw bytes accepted from one HTTP source before parsing.
    pub source_bytes: usize,
    /// Maximum number of provenance entries in one snapshot.
    pub max_sources: usize,
    /// Maximum length of one header name or value, in bytes.
    pub max_header_bytes: usize,
    /// Maximum accepted HTTP jump count before a source is refused.
    pub max_redirects: usize,
}

impl Default for ContractLimits {
    fn default() -> Self {
        Self {
            profile_bytes: 64 * 1024,
            catalog_bytes: 4 * 1024 * 1024,
            catalog_models: 1_000,
            source_bytes: 8 * 1024 * 1024,
            max_sources: 32,
            max_header_bytes: 8 * 1024,
            max_redirects: 5,
        }
    }
}

impl ContractLimits {
    /// A conservative profile for tests and small fixtures.
    pub fn tight() -> Self {
        Self {
            profile_bytes: 2 * 1024,
            catalog_bytes: 8 * 1024,
            catalog_models: 8,
            source_bytes: 4 * 1024,
            max_sources: 4,
            max_header_bytes: 128,
            max_redirects: 1,
        }
    }
}

/// Release channel of a client version.
///
/// Only [`VersionChannel::Stable`] versions activate automatically; a
/// prerelease never promotes itself to stable by trimming its suffix, and a
/// channel change is always rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionChannel {
    Stable,
    Prerelease,
}

impl VersionChannel {
    /// Stable channel tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Prerelease => "prerelease",
        }
    }
}

/// A parsed client version: `major.minor.patch` plus its channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedVersion {
    pub channel: VersionChannel,
    pub core: [u64; 3],
    /// Normalized `major.minor.patch[-prerelease]` form, `v` prefix removed.
    pub normalized: String,
}

impl ParsedVersion {
    /// Order by numeric core only. Channel equality is checked separately by
    /// [`check_identity_transition`], so this never compares a stable against a
    /// prerelease.
    pub fn cmp_core(&self, other: &Self) -> Ordering {
        self.core.cmp(&other.core)
    }
}

/// Parse a client version of the form `[v]major.minor.patch[-prerelease]`.
///
/// A missing prerelease suffix means [`VersionChannel::Stable`]. Anything that
/// is not exactly three numeric parts is rejected: an unknown shape must not
/// activate, and `1.2` or `1.2.3.4` are not comparable SemVer.
pub fn parse_version(raw: &str) -> Result<ParsedVersion, ContractError> {
    let trimmed = raw.trim();
    let stripped = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let (core, prerelease) = match stripped.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (stripped, None),
    };
    let mut parts = [0u64; 3];
    let mut index = 0usize;
    for part in core.split('.') {
        if index == parts.len() || part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ContractError::UnparsableVersion {
                version: raw.to_string(),
            });
        }
        let parsed: u64 = part.parse().map_err(|_| ContractError::UnparsableVersion {
            version: raw.to_string(),
        })?;
        parts[index] = parsed;
        index += 1;
    }
    if index != parts.len() {
        return Err(ContractError::UnparsableVersion {
            version: raw.to_string(),
        });
    }
    let normalized = match prerelease {
        Some(prerelease) if !prerelease.is_empty() => {
            format!("{}.{}.{}-{}", parts[0], parts[1], parts[2], prerelease)
        }
        _ => format!("{}.{}.{}", parts[0], parts[1], parts[2]),
    };
    let channel = if prerelease.is_some_and(|p| !p.is_empty()) {
        VersionChannel::Prerelease
    } else {
        VersionChannel::Stable
    };
    Ok(ParsedVersion {
        channel,
        core: parts,
        normalized,
    })
}

/// Whether a version transition keeps a last-known-good snapshot, activates a
/// newer one, or finds identical content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityTransition {
    /// The candidate declares no version, or the first version ever declared.
    New,
    /// Same version string, same channel. The caller must still compare
    /// generations: identical content is `Unchanged` for the store, different
    /// content is a visible conflict.
    Same,
    /// Strictly newer within the same channel.
    Newer,
}

/// Compare the identity of a candidate against the active snapshot.
///
/// Rules: SemVer is compared inside one channel only; a channel change is
/// rejected; dropping a declared version is rejected; a version with an
/// unparsable shape is rejected.
pub fn check_identity_transition(
    previous: &ClientIdentity,
    candidate: &ClientIdentity,
) -> Result<IdentityTransition, ContractError> {
    let (previous_version, next_version) = match (&previous.version, &candidate.version) {
        (None, None | Some(_)) => return Ok(IdentityTransition::New),
        // A candidate that drops the only thing identifying the client would
        // announce a downgraded identity mid-flight. Refuse it.
        (Some(_), None) => return Err(ContractError::MissingVersion),
        (Some(previous), Some(next)) => (previous, next),
    };
    let previous = parse_version(previous_version)?;
    let next = parse_version(next_version)?;
    if previous.channel != next.channel {
        return Err(ContractError::ChannelMismatch {
            previous: previous.normalized,
            candidate: next.normalized,
        });
    }
    match next.cmp_core(&previous) {
        Ordering::Greater => Ok(IdentityTransition::Newer),
        Ordering::Less => Err(ContractError::Downgrade {
            previous: previous.normalized.clone(),
            candidate: next.normalized,
        }),
        Ordering::Equal => Ok(IdentityTransition::Same),
    }
}

/// Lowercase SHA-256 hex digest check: 64 hex characters, no `0x`, no upper
/// case, so two writers derive the same artifact name for the same bytes.
pub fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Header names that must never appear with a literal value in a contract
/// file. Their *names* may be documented; their values always come from runtime
/// credentials, so a stored value is always a leak.
const AUTH_HEADER_NAMES: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "www-authenticate",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api-key",
    "x-goog-api-key",
    "x-auth-token",
];

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

/// Validate a name/value header pair for storage or wire use.
///
/// Rejects control characters (including CR and LF), empty names, duplicate
/// names under different casing, and auth headers carrying a literal value.
pub fn validate_header_pairs(headers: &[(String, String)]) -> Result<(), ContractError> {
    let mut seen: Vec<String> = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        if name.is_empty() || name.len() > 128 || !is_header_token(name) {
            return Err(ContractError::InvalidHeader {
                name: name.clone(),
                reason: "name is not a valid header token",
            });
        }
        if value.len() > 8 * 1024 || !is_header_value(value) {
            return Err(ContractError::InvalidHeader {
                name: name.clone(),
                reason: "value contains control characters or is too long",
            });
        }
        if AUTH_HEADER_NAMES.contains(&name.to_ascii_lowercase().as_str()) {
            return Err(ContractError::InvalidHeader {
                name: name.clone(),
                reason: "credential values belong to runtime, not to a contract",
            });
        }
        if seen.iter().any(|seen| seen.eq_ignore_ascii_case(name)) {
            return Err(ContractError::DuplicateHeader { name: name.clone() });
        }
        seen.push(name.clone());
    }
    Ok(())
}

/// RFC 9110 `token`: one or more visible ASCII characters from the separator set.
fn is_header_token(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// Header value: printable ASCII plus HTAB, with no CR, LF or NUL.
fn is_header_value(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b) || b >= 0x80)
}

/// Validate one catalog reference: shape of the digest and of the format tag.
fn validate_catalog_ref(catalog: &CatalogRef) -> Result<(), ContractError> {
    if !is_sha256_hex(&catalog.digest) {
        return Err(ContractError::InvalidDigest {
            value: catalog.digest.clone(),
        });
    }
    if catalog.format.is_empty()
        || catalog.format.len() > 32
        || !catalog
            .format
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(ContractError::Rejected(format!(
            "unsupported catalog format {:?}",
            catalog.format
        )));
    }
    Ok(())
}

/// Validate the provenance entries of a candidate.
fn validate_sources(sources: &[SourceRef], limits: &ContractLimits) -> Result<(), ContractError> {
    if sources.is_empty() {
        return Err(ContractError::Rejected(
            "a contract needs at least one source reference".to_string(),
        ));
    }
    if sources.len() > limits.max_sources {
        return Err(ContractError::Rejected(format!(
            "{} sources exceed the {} entry limit",
            sources.len(),
            limits.max_sources
        )));
    }
    for source in sources {
        validate_source(source, limits)?;
    }
    Ok(())
}

/// Validate one provenance entry.
fn validate_source(source: &SourceRef, limits: &ContractLimits) -> Result<(), ContractError> {
    let url = source.url.trim();
    if url.len() > 2048 || !is_absolute_http_url(url) {
        return Err(ContractError::InvalidSourceUrl {
            reason: "not an absolute http(s) URL without credentials",
        });
    }
    if let Some(revision) = &source.revision
        && (revision.is_empty()
            || revision.len() > 128
            || !revision
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'/')))
    {
        return Err(ContractError::InvalidSourceUrl {
            reason: "revision is not a plain identifier",
        });
    }
    if !is_sha256_hex(&source.hash) {
        return Err(ContractError::InvalidDigest {
            value: source.hash.clone(),
        });
    }
    if url.len() > limits.max_header_bytes * 8 {
        return Err(ContractError::InvalidSourceUrl {
            reason: "url is unreasonably long",
        });
    }
    Ok(())
}

/// Absolute `http`/`https` URL with no userinfo, no CR/LF and no whitespace.
fn is_absolute_http_url(url: &str) -> bool {
    if url.contains(['\r', '\n', ' ', '\t']) {
        return false;
    }
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !authority.is_empty() && !authority.contains('@')
}

/// Validate the identity of a candidate.
fn validate_identity(identity: &ClientIdentity) -> Result<(), ContractError> {
    if !matches!(
        identity.origin.as_str(),
        IDENTITY_ORIGIN_OFFICIAL | IDENTITY_ORIGIN_OWN
    ) {
        return Err(ContractError::Rejected(format!(
            "unknown identity origin {:?}",
            identity.origin
        )));
    }
    if let Some(version) = &identity.version {
        parse_version(version)?;
    }
    Ok(())
}

/// Validate the compiled rule-set identity of a candidate.
fn validate_rule_set_id(rule_set_id: &str) -> Result<(), ContractError> {
    if rule_set_id.is_empty()
        || rule_set_id.len() > 128
        || !rule_set_id.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
    {
        return Err(ContractError::Rejected(
            "rule_set_id must be a short lowercase identifier".to_string(),
        ));
    }
    Ok(())
}

/// Structural validation every candidate must pass, before any disk write or
/// in-memory activation.
///
/// `payload_bytes` is the serialized payload: the caller measures the profile
/// limit after decompression, so the size checked here is the real one.
pub fn validate_candidate_shape<T: serde::Serialize>(
    candidate: &Candidate<T>,
    limits: &ContractLimits,
) -> Result<(), ContractError> {
    if candidate.extractor_version == 0 {
        return Err(ContractError::Rejected(
            "extractor_version must be non-zero".to_string(),
        ));
    }
    validate_rule_set_id(&candidate.rule_set_id)?;
    validate_identity(&candidate.client_identity)?;
    validate_sources(&candidate.sources, limits)?;
    if let Some(catalog) = &candidate.catalog_ref {
        validate_catalog_ref(catalog)?;
    }
    validate_payload_size(candidate, limits)?;
    validate_no_secrets(candidate)?;
    Ok(())
}

/// Enforce the 64 KiB profile limit and the 4 MiB / 1000-model catalog limit.
///
/// Returns the measured payload size so the store can record it.
pub fn validate_payload_size<T: serde::Serialize>(
    candidate: &Candidate<T>,
    limits: &ContractLimits,
) -> Result<usize, ContractError> {
    let bytes = canonical_bytes(&candidate.payload)?;
    if bytes.len() > limits.profile_bytes {
        return Err(ContractError::PayloadTooLarge {
            size: bytes.len(),
            limit: limits.profile_bytes,
        });
    }
    Ok(bytes.len())
}

/// Reject credential material anywhere in the candidate. Sources, the rule-set
/// id, the identity and the payload are all inspected.
pub fn validate_no_secrets<T: serde::Serialize>(
    candidate: &Candidate<T>,
) -> Result<(), ContractError> {
    for source in &candidate.sources {
        if contains_secret_material(source.url.as_bytes()) {
            return Err(ContractError::SecretMaterial {
                location: "source url",
            });
        }
        if let Some(revision) = &source.revision
            && contains_secret_material(revision.as_bytes())
        {
            return Err(ContractError::SecretMaterial {
                location: "source revision",
            });
        }
    }
    let bytes = canonical_bytes(&candidate.payload)?;
    if contains_secret_material(&bytes) {
        return Err(ContractError::SecretMaterial {
            location: "payload",
        });
    }
    Ok(())
}

/// Enforce the raw-source byte ceiling, applied before parsing so an oversized
/// body never reaches a provider parser.
pub fn validate_source_body_size(len: usize, limits: &ContractLimits) -> Result<(), ContractError> {
    if len > limits.source_bytes {
        return Err(ContractError::SourceTooLarge {
            size: len,
            limit: limits.source_bytes,
        });
    }
    Ok(())
}

/// Enforce the catalog byte ceiling for a separately stored catalog.
pub fn validate_catalog_body_size(
    len: usize,
    limits: &ContractLimits,
) -> Result<(), ContractError> {
    if len > limits.catalog_bytes {
        return Err(ContractError::CatalogBytesTooLarge {
            size: len,
            limit: limits.catalog_bytes,
        });
    }
    Ok(())
}

/// Contract-wide validation hook, implemented per provider.
///
/// The store calls this *before* touching the disk and before the anti-downgrade
/// check, so a rejected candidate never mutates state. `previous` is the active
/// snapshot, or `None` on first load.
pub trait ContractValidator<T>: Send + Sync {
    /// Identity of the compiled extraction and guard logic this validator
    /// implements. A candidate built with a different rule set is rejected.
    fn rule_set_id(&self) -> &'static str;

    /// Version of the compiled extraction and guard logic.
    fn extractor_version(&self) -> u32;

    /// Number of catalog entries carried by `candidate`, if the payload shape
    /// exposes one. Returning `None` defers the model-count limit to the
    /// provider's own [`ContractValidator::validate`] implementation.
    fn catalog_capacity(&self, _candidate: &Candidate<T>) -> Option<usize> {
        None
    }

    /// Provider-specific semantic validation, including protocol guards.
    fn validate(
        &self,
        candidate: &Candidate<T>,
        previous: Option<&Snapshot<T>>,
        limits: &ContractLimits,
    ) -> Result<(), ContractError>;
}

/// Digest of a payload, used by drivers to detect `Unchanged`.
pub fn candidate_payload_digest<T: serde::Serialize>(
    candidate: &Candidate<T>,
) -> Result<String, ContractError> {
    payload_digest(&candidate.payload)
}

/// Whether an evidence kind counts as immutable, revision-pinned provenance.
pub fn is_pinned_evidence(evidence: EvidenceKind) -> bool {
    matches!(
        evidence,
        EvidenceKind::PackageManifest | EvidenceKind::BundleHash | EvidenceKind::CapturedFixture
    )
}

/// The contract a candidate targets.
pub fn candidate_contract<T>(candidate: &Candidate<T>) -> ContractId {
    candidate.contract_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_contract::{Candidate, ClientIdentity, SourceRef};

    fn identity(version: Option<&str>) -> ClientIdentity {
        ClientIdentity {
            version: version.map(str::to_string),
            origin: IDENTITY_ORIGIN_OFFICIAL.to_string(),
        }
    }

    #[test]
    fn parse_version_accepts_three_parts_and_normalizes() {
        let parsed = parse_version("v1.2.3").unwrap();
        assert_eq!(parsed.channel, VersionChannel::Stable);
        assert_eq!(parsed.normalized, "1.2.3");
        assert_eq!(parsed.core, [1, 2, 3]);
        let prerelease = parse_version("0.162.1-beta.1").unwrap();
        assert_eq!(prerelease.channel, VersionChannel::Prerelease);
        assert_eq!(prerelease.normalized, "0.162.1-beta.1");
    }

    #[test]
    fn parse_version_rejects_unknown_shapes() {
        for raw in ["", "1", "1.2", "1.2.3.4", "v1.2.x", "1.2.-3", " 1.2.3 rc"] {
            assert!(parse_version(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn numeric_core_comparison_not_lexicographic() {
        let old = parse_version("2.1.99").unwrap();
        let new = parse_version("2.1.100").unwrap();
        assert_eq!(new.cmp_core(&old), Ordering::Greater);
    }

    #[test]
    fn transition_is_newer_for_stable_upgrades() {
        let transition =
            check_identity_transition(&identity(Some("2.1.99")), &identity(Some("2.1.100")))
                .unwrap();
        assert_eq!(transition, IdentityTransition::Newer);
    }

    #[test]
    fn transition_rejects_downgrade() {
        let error = check_identity_transition(&identity(Some("2.2.0")), &identity(Some("2.1.999")))
            .unwrap_err();
        assert!(matches!(error, ContractError::Downgrade { .. }));
    }

    #[test]
    fn transition_rejects_channel_change_in_both_directions() {
        assert!(matches!(
            check_identity_transition(&identity(Some("1.2.3")), &identity(Some("1.3.0-rc.1")))
                .unwrap_err(),
            ContractError::ChannelMismatch { .. }
        ));
        assert!(matches!(
            check_identity_transition(&identity(Some("1.3.0-rc.1")), &identity(Some("1.3.0")))
                .unwrap_err(),
            ContractError::ChannelMismatch { .. }
        ));
    }

    #[test]
    fn same_version_different_prerelease_is_not_newer() {
        let transition = check_identity_transition(
            &identity(Some("1.0.0-alpha.1")),
            &identity(Some("1.0.0-alpha.2")),
        )
        .unwrap();
        assert_eq!(transition, IdentityTransition::Same);
    }

    #[test]
    fn missing_version_on_candidate_is_rejected() {
        let error =
            check_identity_transition(&identity(Some("1.0.0")), &identity(None)).unwrap_err();
        assert!(matches!(error, ContractError::MissingVersion));
    }

    #[test]
    fn first_version_and_unversioned_are_new() {
        assert_eq!(
            check_identity_transition(&identity(None), &identity(Some("1.0.0"))).unwrap(),
            IdentityTransition::New
        );
        assert_eq!(
            check_identity_transition(&identity(None), &identity(None)).unwrap(),
            IdentityTransition::New
        );
    }

    #[test]
    fn sha256_hex_check_is_strict() {
        assert!(is_sha256_hex(&"a".repeat(64)));
        assert!(!is_sha256_hex(&"a".repeat(63)));
        assert!(!is_sha256_hex(&"A".repeat(64)));
        assert!(!is_sha256_hex(&"g".repeat(64)));
        assert!(!is_sha256_hex(&format!("0x{}", "a".repeat(64))));
    }

    #[test]
    fn header_pairs_reject_crlf_duplicates_and_credentials() {
        assert!(validate_header_pairs(&[("Accept".into(), "application/json".into())]).is_ok());
        assert!(matches!(
            validate_header_pairs(&[("X".into(), "a\r\nInjected: 1".into())]).unwrap_err(),
            ContractError::InvalidHeader { .. }
        ));
        assert!(matches!(
            validate_header_pairs(&[
                ("Accept".into(), "*/*".into()),
                ("accept".into(), "json".into())
            ])
            .unwrap_err(),
            ContractError::DuplicateHeader { .. }
        ));
        assert!(matches!(
            validate_header_pairs(&[("Authorization".into(), "Bearer sk-live-123".into())])
                .unwrap_err(),
            ContractError::InvalidHeader { .. }
        ));
        assert!(validate_header_pairs(&[("Bad Name".into(), "x".into())]).is_err());
    }

    #[test]
    fn secret_detection_catches_credentials_and_session_ids() {
        // Real credential values are rejected.
        assert!(contains_secret_material(
            b"{\"headers\":[\"Authorization: Bearer sk-123\"]}"
        ));
        assert!(contains_secret_material(b"session_id=abc123"));
        assert!(contains_secret_material(b"TOKEN=sec_2f0aff65"));
        assert!(contains_secret_material(b"Basic dXNlcjpwYXNz"));
        assert!(contains_secret_material(
            b"x-api-key: sk-live-abcdef1234567890"
        ));
        assert!(
            contains_secret_material(b"Authorization: Bearer sk-proj-Ab3Cd9EfGhIjKlMnOp"),
            "an API key bound to a bearer scheme is a credential"
        );
        assert!(
            contains_secret_material(b"WWW-Authenticate: Basic dXNlcjpwYXNz"),
            "a base64 user:pass pair is a credential"
        );

        // Documentary mentions and clean payloads are not credentials.
        assert!(
            !contains_secret_material(
                b"{\"user_agent\":\"codex-cli/1.2.3 (Linux)\",\"headers\":[\"origin\"]}"
            ),
            "a clean profile must not be flagged"
        );
        assert!(
            !contains_secret_material(b"insert_header(&mut headers, \"session-id\", &id);"),
            "a guard symbol naming the header must stay allowed"
        );
        assert!(
            !contains_secret_material(b"{\"headers\":[\"session-id\",\"thread-id\"]}"),
            "documentary header names are not secret values"
        );
        assert!(
            !contains_secret_material(
                b"{\"models\":[{\"id\":\"basic-model\",\"description\":\"basic searches\"}]}"
            ),
            "the word `basic` in prose is not an auth header"
        );
        assert!(
            !contains_secret_material(
                b"{\"docs\":\"use Bearer authentication to obtain a token\"}"
            ),
            "prose naming a scheme is not a credential"
        );
        assert!(
            !contains_secret_material(b"{\"id\":\"basic-internationalization-model\"}"),
            "a long camel-case id is not a base64 credential"
        );
    }

    #[test]
    fn source_urls_must_be_absolute_and_credential_free() {
        let limits = ContractLimits::default();
        let base = |url: &str| SourceRef {
            url: url.to_string(),
            revision: Some("rev".to_string()),
            hash: "a".repeat(64),
            evidence: EvidenceKind::PackageManifest,
        };
        assert!(validate_sources(&[base("https://registry.npmjs.org/x")], &limits).is_ok());
        assert!(validate_sources(&[base("registry.npmjs.org/x")], &limits).is_err());
        assert!(validate_sources(&[base("https://u:p@registry.npmjs.org/x")], &limits).is_err());
        assert!(validate_sources(&[base("https://x/y\r\nZ: 1")], &limits).is_err());
        assert!(validate_sources(&[base("file:///etc/passwd")], &limits).is_err());
    }

    #[test]
    fn revision_must_be_a_plain_identifier() {
        let limits = ContractLimits::default();
        let base = |revision: Option<&str>| SourceRef {
            url: "https://example.test/x.json".to_string(),
            revision: revision.map(str::to_string),
            hash: "b".repeat(64),
            evidence: EvidenceKind::BundleHash,
        };
        assert!(validate_sources(&[base(Some("2f05c0a71f59"))], &limits).is_ok());
        assert!(validate_sources(&[base(Some("v1.0.0"))], &limits).is_ok());
        assert!(validate_sources(&[base(Some("tag with space"))], &limits).is_err());
        assert!(validate_sources(&[base(None)], &limits).is_ok());
    }

    #[test]
    fn default_limits_match_the_specification() {
        let limits = ContractLimits::default();
        assert_eq!(limits.profile_bytes, 64 * 1024);
        assert_eq!(limits.catalog_bytes, 4 * 1024 * 1024);
        assert_eq!(limits.catalog_models, 1_000);
    }

    #[test]
    fn unversioned_and_versioned_identities_take_every_arm() {
        // Every (previous, candidate) version combination must be handled;
        // this is the regression for the non-exhaustive match.
        assert_eq!(
            check_identity_transition(&identity(None), &identity(None)).unwrap(),
            IdentityTransition::New
        );
        assert_eq!(
            check_identity_transition(&identity(None), &identity(Some("1.0.0"))).unwrap(),
            IdentityTransition::New
        );
        assert!(matches!(
            check_identity_transition(&identity(Some("1.0.0")), &identity(None)).unwrap_err(),
            ContractError::MissingVersion
        ));
        assert_eq!(
            check_identity_transition(&identity(Some("1.0.0")), &identity(Some("1.0.0"))).unwrap(),
            IdentityTransition::Same
        );
        assert_eq!(
            check_identity_transition(&identity(Some("1.0.0")), &identity(Some("1.0.1"))).unwrap(),
            IdentityTransition::Newer
        );
    }

    #[test]
    fn catalog_body_ceiling_is_its_own_error() {
        let limits = ContractLimits::tight();
        assert!(validate_catalog_body_size(limits.catalog_bytes, &limits).is_ok());
        assert!(matches!(
            validate_catalog_body_size(limits.catalog_bytes + 1, &limits).unwrap_err(),
            ContractError::CatalogBytesTooLarge { .. }
        ));
    }

    #[test]
    fn payload_over_profile_limit_is_rejected() {
        let limits = ContractLimits::tight();
        let big = "x".repeat(limits.profile_bytes + 1);
        let candidate = Candidate::<String>::without_catalog(
            ContractId::Codex,
            identity(Some("1.0.0")),
            "codex-rules",
            1,
            vec![SourceRef::pinned(
                "https://example.test/x.json",
                "rev",
                "c".repeat(64),
                EvidenceKind::PackageManifest,
            )],
            big,
        );
        assert!(matches!(
            validate_payload_size(&candidate, &limits).unwrap_err(),
            ContractError::PayloadTooLarge { .. }
        ));
    }
}
