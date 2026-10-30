use std::cmp::Ordering;

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
        check_identity_transition(&identity(Some("2.1.99")), &identity(Some("2.1.100"))).unwrap();
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
    let error = check_identity_transition(&identity(Some("1.0.0")), &identity(None)).unwrap_err();
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
        !contains_secret_material(b"{\"docs\":\"use Bearer authentication to obtain a token\"}"),
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
