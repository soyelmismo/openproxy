//! Structural validation, limits and anti-downgrade for contract candidates.
//!
//! Nothing here knows a provider payload shape. The base enforces the rules any
//! contract must satisfy (size, digests, header hygiene, no secrets, stable
//! SemVer within one channel); provider-specific semantics live in the
//! [`ContractValidator`] a store is built with.

use super::{
    Candidate, CatalogRef, ClientIdentity, ContractError, ContractId, EvidenceKind,
    IDENTITY_ORIGIN_OFFICIAL, IDENTITY_ORIGIN_OWN, Snapshot, SourceRef, canonical_bytes,
    payload_digest,
};

mod secrets;
mod version;

pub use secrets::contains_secret_material;
pub use version::{
    IdentityTransition, ParsedVersion, VersionChannel, check_identity_transition, parse_version,
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
mod tests;
