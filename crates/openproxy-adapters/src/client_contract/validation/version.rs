//! Client-version parsing and the anti-downgrade identity transition.
//!
//! SemVer is compared inside one channel only: a prerelease never promotes
//! itself to stable by trimming its suffix, and a channel change is always
//! rejected.

use std::cmp::Ordering;

use super::super::{ClientIdentity, ContractError};

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
