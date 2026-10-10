// Nous Research (Nous Portal).
//
// The Portal rejects chat completions unless the OpenAI-compatible `tags` array
// carries a `user=` entry identifying the caller. Where that identity comes from
// depends on the credential kind:
//
//   * OAuth accounts send a JWT bearer whose `sub` claim IS the Portal user id,
//     so the Portal can derive it on its own -- but only when it can parse the
//     token. We extract `sub` (falling back to `email`) explicitly so the tag is
//     stable and correct for *each* account in a multi-account pool.
//   * API-key accounts (`sk-nous-...`) carry no user identity at all, and the
//     Portal answers 400 "missing tags" / "missing user tag". Those need a
//     synthetic but deterministic `user=` tag, keyed on the account label.
//
// Both branches keep `product=`/`client=` attribution and the sticky
// `session_id` routing key so prompt caches stay warm.

declare_openai_adapter!(
    /// Adapter for <https://inference-api.nousresearch.com>.
    ///
    /// Nous Research speaks OpenAI-compatible `/v1/chat/completions` with
    /// Bearer auth. Free-tier models include Hermes-4-405B and Hermes-4-70B.
    NousResearchAdapter,
    id: "nous-research",
    name: "Nous Research",
    base_url: "https://inference-api.nousresearch.com/v1",
    custom_impl: {
        fn wrap_request_body(
            &self,
            body: bytes::Bytes,
            target_format: TargetFormat,
            _model: &ModelId,
            resolved_target: &openproxy_types::context::ResolvedTarget,
        ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
            if target_format != TargetFormat::Openai {
                return Ok(body);
            }

            // OAuth accounts keep their JWT in `custom_meta.access_token` (the
            // pipeline leaves `api_key` empty for those). API-key accounts have
            // `custom_meta == None` and the raw key in `api_key`.
            let oauth_token = resolved_target
                .custom_meta
                .as_ref()
                .map(|m| m.access_token.as_str());
            let user_tag = nous_user_tag(
                oauth_token,
                resolved_target.api_key.as_str(),
                resolved_target.api_key_label.as_deref(),
            );
            let sticky_key = nous_sticky_key(
                oauth_token,
                resolved_target.api_key_label.as_deref(),
            );

            crate::adapters::traits::patch_json_request_body(body, |obj| {
                let mut tags = match obj.remove("tags") {
                    Some(serde_json::Value::Array(existing)) => existing,
                    _ => Vec::new(),
                };

                // Keep attribution first; the identity tag goes last.
                for tag in ["product=hermes-agent", "client=hermes-client"] {
                    if !tags.iter().any(|t| t.as_str() == Some(tag)) {
                        tags.push(serde_json::Value::String(tag.to_string()));
                    }
                }
                // Only add the identity tag when the caller did not supply a
                // `user=` of its own (Hermes sends its own attribution set).
                if !tags
                    .iter()
                    .any(|t| t.as_str().is_some_and(|s| s.starts_with("user=")))
                {
                    tags.push(serde_json::Value::String(user_tag));
                }

                obj.insert("tags".to_string(), serde_json::Value::Array(tags));

                if let Some(key) = sticky_key {
                    obj.entry("session_id")
                        .or_insert_with(|| serde_json::Value::String(key));
                }
            })
        }
    }
);

/// Builds the mandatory `user=<id>` tag.
///
/// OAuth: the Portal user id read from the JWT (`sub`, falling back to `email`),
/// so each account in the pool reports its own identity.
/// API key: a deterministic synthetic id derived from the account label, since a
/// `sk-nous-...` key carries no identity for the Portal to derive one from.
fn nous_user_tag(oauth_token: Option<&str>, api_key: &str, account_label: Option<&str>) -> String {
    if let Some(sub) = oauth_token.and_then(jwt_user) {
        return format!("user={sub}");
    }
    format!("user={}", synthetic_api_key_user(api_key, account_label))
}

/// Sticky routing key. Mirrors what the Portal sees as the stable account
/// identity, so repeated requests from one account land on the same upstream
/// instance and keep the prompt cache warm.
fn nous_sticky_key(oauth_token: Option<&str>, account_label: Option<&str>) -> Option<String> {
    oauth_token
        .and_then(jwt_user)
        .or_else(|| account_label.map(|l| l.to_string()))
}

/// Extracts a named claim from an unverified JWT payload.
///
/// Signature verification is the upstream TLS/auth layer's job; here we only need
/// the attribution claims. Returns `None` for non-JWT or malformed tokens.
pub fn jwt_claim_named(token: &str, claim: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::Engine::decode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        payload,
    )
    .or_else(|_| {
        base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD_NO_PAD,
            payload,
        )
    })
    .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let claim = value.get(claim)?.as_str()?;
    if claim.is_empty() {
        return None;
    }
    Some(claim.to_string())
}

/// The Portal's user id for an OAuth token: `sub`, falling back to `email`.
pub fn jwt_user(token: &str) -> Option<String> {
    jwt_claim_named(token, "sub").or_else(|| jwt_claim_named(token, "email"))
}

/// Convenience alias for the email claim, used to label OAuth accounts.
pub fn jwt_email(token: &str) -> Option<String> {
    jwt_claim_named(token, "email")
}

/// Deterministic stand-in identity for API-key accounts.
///
/// Prefers the account label (stable across restarts and shared by every request
/// routed to that account), and falls back to a short digest of the key so two
/// different API keys never collapse onto the same Portal user.
fn synthetic_api_key_user(api_key: &str, account_label: Option<&str>) -> String {
    if let Some(label) = account_label.map(str::trim).filter(|l| !l.is_empty()) {
        let sanitized: String = label
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        return format!("openproxy-key-{sanitized}");
    }

    // No label: derive a short, stable id from the key itself.
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut hasher, api_key.as_bytes());
    let digest = sha2::Digest::finalize(hasher);
    let short: String = digest
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("openproxy-key-{short}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn make_jwt(claims: &serde_json::Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(claims).unwrap());
        format!("{header}.{payload}.c2ln")
    }

    #[test]
    fn jwt_sub_is_extracted_as_user_tag() {
        let token = make_jwt(&serde_json::json!({"sub": "cmot8lfvz0000jb065nisf0t1"}));
        assert_eq!(
            jwt_claim_named(&token, "sub").as_deref(),
            Some("cmot8lfvz0000jb065nisf0t1")
        );
        let tag = nous_user_tag(Some(&token), "", Some("hermes"));
        assert_eq!(tag, "user=cmot8lfvz0000jb065nisf0t1");
    }

    #[test]
    fn jwt_without_sub_falls_back_to_email() {
        let token = make_jwt(&serde_json::json!({"email": "a@b.co"}));
        let tag = nous_user_tag(Some(&token), "", Some("hermes"));
        assert_eq!(tag, "user=a@b.co");
    }

    #[test]
    fn distinct_oauth_accounts_get_distinct_user_tags() {
        let one = make_jwt(&serde_json::json!({"sub": "user-one"}));
        let two = make_jwt(&serde_json::json!({"sub": "user-two"}));
        assert_ne!(
            nous_user_tag(Some(&one), "", Some("acct")),
            nous_user_tag(Some(&two), "", Some("acct"))
        );
    }

    #[test]
    fn api_key_uses_label_based_user_tag() {
        assert_eq!(
            synthetic_api_key_user("sk-nous-abc", Some("hermes-main")),
            "openproxy-key-hermes-main"
        );
        let tag = nous_user_tag(None, "sk-nous-abc", Some("hermes-main"));
        assert_eq!(tag, "user=openproxy-key-hermes-main");
    }

    #[test]
    fn api_key_without_label_derives_from_key() {
        let a = synthetic_api_key_user("sk-nous-aaa", None);
        let b = synthetic_api_key_user("sk-nous-bbb", None);
        assert_ne!(a, b);
        assert!(a.starts_with("openproxy-key-"));
    }

    #[test]
    fn malformed_token_yields_no_claim() {
        assert_eq!(jwt_claim_named("not-a-jwt", "sub"), None);
        assert_eq!(jwt_claim_named("", "sub"), None);
        assert_eq!(jwt_user("not-a-jwt"), None);
    }

    // --- wrap_request_body end to end -------------------------------------

    fn target(api_key: &str, label: Option<&str>, oauth: Option<&str>) -> openproxy_types::context::ResolvedTarget {
        openproxy_types::context::ResolvedTarget {
            target: openproxy_types::combos::ComboTarget::default(),
            model: openproxy_types::models::Model::default(),
            api_key: api_key.to_string(),
            api_key_label: label.map(str::to_string),
            custom_meta: oauth.map(|access_token| {
                openproxy_types::context::CustomProviderMeta {
                    access_token: access_token.to_string(),
                    maybe_refresh: None,
                    kiro_region: None,
                    kiro_profile_arn: None,
                    antigravity_project: None,
                    antigravity_metadata: None,
                    codex_workspace_id: None,
                    claude_account_uuid: None,
                    claude_metadata: None,
                }
            }),
        }
    }

    fn tags_of(body: &[u8]) -> Vec<String> {
        let val: serde_json::Value = serde_json::from_slice(body).unwrap();
        val["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn oauth_account_sends_its_own_jwt_sub_as_user_tag() {
        let adapter = NousResearchAdapter::new();
        let token = make_jwt(&serde_json::json!({"sub": "user-abc"}));
        let body = adapter
            .wrap_request_body(
                bytes::Bytes::from_static(br#"{"model":"m","messages":[]}"#),
                TargetFormat::Openai,
                &openproxy_types::ModelId::new("m"),
                &target("", Some("hermes"), Some(&token)),
            )
            .unwrap();
        let tags = tags_of(&body);
        assert!(tags.iter().any(|t| t == "user=user-abc"), "{tags:?}");
        assert!(tags.iter().any(|t| t == "product=hermes-agent"));
    }

    #[test]
    fn api_key_account_gets_a_synthetic_user_tag() {
        let adapter = NousResearchAdapter::new();
        let body = adapter
            .wrap_request_body(
                bytes::Bytes::from_static(br#"{"model":"m","messages":[]}"#),
                TargetFormat::Openai,
                &openproxy_types::ModelId::new("m"),
                &target("sk-nous-xyz", Some("hermes-main"), None),
            )
            .unwrap();
        let tags = tags_of(&body);
        assert!(
            tags.iter().any(|t| t == "user=openproxy-key-hermes-main"),
            "{tags:?}"
        );
    }

    #[test]
    fn caller_supplied_user_tag_is_not_overwritten() {
        let adapter = NousResearchAdapter::new();
        let body = adapter
            .wrap_request_body(
                bytes::Bytes::from_static(br#"{"model":"m","tags":["user=mine"]}"#),
                TargetFormat::Openai,
                &openproxy_types::ModelId::new("m"),
                &target("sk-nous-xyz", Some("hermes-main"), None),
            )
            .unwrap();
        let tags = tags_of(&body);
        assert!(tags.iter().any(|t| t == "user=mine"), "{tags:?}");
        assert_eq!(
            tags.iter().filter(|t| t.starts_with("user=")).count(),
            1,
            "no duplicate user tag"
        );
    }

    #[test]
    fn non_openai_target_format_is_left_untouched() {
        let adapter = NousResearchAdapter::new();
        let original = br#"{"model":"m","messages":[]}"#;
        let body = adapter
            .wrap_request_body(
                bytes::Bytes::from_static(original),
                TargetFormat::Anthropic,
                &openproxy_types::ModelId::new("m"),
                &target("sk-nous-xyz", Some("hermes-main"), None),
            )
            .unwrap();
        assert_eq!(&body[..], &original[..]);
    }
}
