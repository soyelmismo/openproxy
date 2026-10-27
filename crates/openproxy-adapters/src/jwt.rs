//! Unverified JWT claim reading for portal-issued OAuth access tokens.
//!
//! RFC 6749 §5.1 makes `expires_in` optional, and several portals (Nous Portal
//! among them) answer `/api/oauth/token` with only `access_token` and
//! `token_type`: the real lifetime lives in the token's own `exp` claim.
//!
//! That distinction is not cosmetic. An account row whose `expires_at` is NULL
//! is treated as "fresh forever" by both the OAuth refresh scheduler and the
//! pipeline's pre-dispatch refresh decision, so it never renews: the access
//! token lapses and the next request is a 401 that marks the account unhealthy.
//! Deriving the deadline from `exp` is what keeps the renewal paths able to see
//! the token at all.
//!
//! Signature verification is the transport's job (TLS to the portal, plus the
//! upstream's own auth check). These helpers only read claims for attribution
//! and expiry, never to authenticate.

use base64::Engine as _;

/// Decodes the payload segment of a JWT into JSON.
///
/// Accepts both unpadded and padded base64url, since portals differ. Returns
/// `None` for anything that is not a three-segment JWT with a JSON payload, so
/// opaque tokens fall through to their declared `expires_in` instead of erroring.
fn payload_json(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    if payload.is_empty() {
        return None;
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

/// A string claim from an unverified JWT payload.
///
/// Empty strings read as absent: portals sometimes emit `""` for an unset claim,
/// and callers use `Option` to mean "not present".
pub fn claim_str(token: &str, claim: &str) -> Option<String> {
    let value = payload_json(token)?.get(claim)?.as_str()?.to_string();
    if value.is_empty() { None } else { Some(value) }
}

/// A numeric claim from an unverified JWT payload.
///
/// `exp` and `iat` are NumericDate per RFC 7519 §2, but some issuers emit them
/// as strings; both spellings are accepted.
pub fn claim_i64(token: &str, claim: &str) -> Option<i64> {
    let value = payload_json(token)?.get(claim)?.clone();
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|v| i64::try_from(v).ok()))
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
}

/// The `exp` claim as a UTC timestamp.
pub fn exp_timestamp(token: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp(claim_i64(token, "exp")?, 0)
}

/// The `exp` claim formatted the way `accounts.expires_at` is stored.
pub fn exp_rfc3339(token: &str) -> Option<String> {
    Some(
        exp_timestamp(token)?
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
    )
}

/// Deadline for a freshly issued access token.
///
/// The `exp` claim wins over `expires_in` when both exist: `expires_in` is a
/// duration counted from when the response was read, so it drifts by however
/// long the exchange took and by the caller's scheduling, while `exp` is an
/// absolute instant the issuer computed. `expires_in` remains the fallback for
/// opaque tokens, which is the only deadline those can offer.
///
/// Returns `None` only when the token carries neither, which callers must treat
/// as "unknown deadline" rather than "no deadline".
pub fn expires_at_for_token(expires_in: Option<u64>, access_token: &str) -> Option<String> {
    exp_rfc3339(access_token).or_else(|| {
        expires_in.map(|secs| {
            (chrono::Utc::now()
                + chrono::Duration::seconds(i64::try_from(secs).unwrap_or(i64::MAX)))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(claims: &serde_json::Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(claims).expect("claims serialize"));
        format!("{header}.{payload}.c2ln")
    }

    #[test]
    fn reads_string_and_numeric_claims() {
        let token = jwt(&serde_json::json!({"sub": "user-1", "exp": 1_792_998_934_i64}));
        assert_eq!(claim_str(&token, "sub").as_deref(), Some("user-1"));
        assert_eq!(claim_i64(&token, "exp"), Some(1_792_998_934));
    }

    #[test]
    fn numeric_claims_given_as_strings_still_parse() {
        let token = jwt(&serde_json::json!({"exp": "1792998934"}));
        assert_eq!(claim_i64(&token, "exp"), Some(1_792_998_934));
    }

    #[test]
    fn empty_string_claim_is_absent() {
        let token = jwt(&serde_json::json!({"sub": ""}));
        assert_eq!(claim_str(&token, "sub"), None);
    }

    #[test]
    fn opaque_tokens_yield_no_claims() {
        assert_eq!(claim_i64("rt_ljcqkopaque", "exp"), None);
        assert_eq!(claim_str("not-a-jwt", "sub"), None);
        assert_eq!(claim_str("", "sub"), None);
        assert_eq!(exp_timestamp("sk-nous-abc.def"), None);
    }

    #[test]
    fn padded_base64url_payload_is_accepted() {
        // A payload whose base64url encoding carries `=` padding must not be
        // rejected just because the issuer padded it.
        let claims = serde_json::json!({"sub": "padded", "exp": 1_700_000_000_i64});
        let mut body = serde_json::to_vec(&claims).expect("claims serialize");
        let payload = loop {
            let encoded = base64::engine::general_purpose::URL_SAFE.encode(&body);
            if encoded.contains('=') {
                break encoded;
            }
            // Trailing whitespace keeps the JSON valid while changing the
            // encoding length, so the fixture stays padded across formatters.
            body.push(b' ');
        };
        let token = format!("h.{payload}.c2ln");
        assert_eq!(claim_str(&token, "sub").as_deref(), Some("padded"));
    }

    #[test]
    fn exp_claim_is_the_deadline() {
        let token = jwt(&serde_json::json!({"exp": 1_792_998_934_i64}));
        assert_eq!(exp_rfc3339(&token).as_deref(), Some("2026-10-26T07:15:34Z"));
    }

    #[test]
    fn jwt_exp_wins_over_expires_in() {
        let token = jwt(&serde_json::json!({"exp": 1_792_998_934_i64}));
        // A wildly wrong expires_in must not override the issuer's own instant.
        assert_eq!(
            expires_at_for_token(Some(1), &token).as_deref(),
            Some("2026-10-26T07:15:34Z")
        );
    }

    #[test]
    fn opaque_token_falls_back_to_declared_lifetime() {
        let before = chrono::Utc::now();
        let expires_at = expires_at_for_token(Some(3600), "opaque-access-token").expect("deadline");
        let parsed = chrono::DateTime::parse_from_rfc3339(&expires_at).expect("rfc3339");
        let delta = parsed.with_timezone(&chrono::Utc) - before;
        assert!(
            delta.num_seconds() >= 3599 && delta.num_seconds() <= 3601,
            "expected ~1h, got {delta}"
        );
    }

    #[test]
    fn token_without_expiry_at_all_reports_unknown() {
        assert_eq!(expires_at_for_token(None, "opaque-access-token"), None);
    }
}
