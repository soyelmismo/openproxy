use crate::error::{CoreError, Result};
use crate::ids::ApiKeyId;
use crate::validation::Validatable;
use chrono::{DateTime, Utc};
use openproxy_types::UpdateField;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// All optional fields on a single `api_keys` row, decoded for the
/// admin / dashboard path. The `key_hash` is exposed because the
/// dashboard's debug view wants to copy it; the plaintext is *never*
/// reconstructed from this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: ApiKeyId,
    /// Full SHA-256 hash. Only serialized when the caller opts in via
    /// `?include_hash=true` — see `ApiKeySafe` for the default shape.
    #[serde(skip_serializing)]
    pub key_hash: String,
    /// First 12 characters of the plaintext, e.g. `"op_live_abc"`.
    /// Useful for the dashboard's "show me the last 4 chars of my key"
    /// affordance without leaking the secret.
    pub key_prefix: Option<String>,
    pub label: Option<String>,
    /// Decoded `scopes_json` column. The schema defaults to
    /// `["chat"]` for rows created before migration 000015.
    pub scopes: Vec<String>,
    /// Decoded `allowed_models_json` column. `None` = no restriction
    /// (= "all models allowed"). An empty vec would mean "deny all"
    /// and is treated as no-allowlist, not as a denial.
    pub allowed_models: Option<Vec<String>>,
    /// Decoded `allowed_combos_json` column. Same semantics as
    /// `allowed_models`.
    pub allowed_combos: Option<Vec<i64>>,
    /// Decoded `blacklisted_providers_json` column. `None` = no restriction.
    pub blacklisted_providers: Option<Vec<String>>,
    /// Decoded `blacklisted_models_json` column. `None` = no restriction.
    pub blacklisted_models: Option<Vec<String>>,
    pub is_active: bool,
    pub revoked_at: Option<String>,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    pub created_at: String,
    pub created_by: Option<String>,
}

/// Input to [`create`]. All fields except `scopes` are optional; an
/// empty `scopes` vector is rejected by the caller.
impl Validatable for CreateApiKeyInput {
    fn validate(&self) -> Result<()> {
        if self.scopes.is_empty() {
            return Err(CoreError::Validation(
                "scopes must contain at least one entry".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateApiKeyInput {
    pub label: Option<String>,
    pub scopes: Vec<String>,
    pub allowed_models: Option<Vec<String>>,
    pub allowed_combos: Option<Vec<i64>>,
    pub blacklisted_providers: Option<Vec<String>>,
    pub blacklisted_models: Option<Vec<String>>,
    pub expires_at: Option<String>,
}

/// Partial update parameters.
#[derive(Default, Clone, Copy)]
pub struct UpdateParams<'a> {
    pub label: Option<&'a str>,
    pub scopes: Option<&'a [String]>,
    pub allowed_models: UpdateField<&'a [String]>,
    pub allowed_combos: UpdateField<&'a [i64]>,
    pub blacklisted_providers: UpdateField<&'a [String]>,
    pub blacklisted_models: UpdateField<&'a [String]>,
    pub is_active: Option<bool>,
    pub expires_at: UpdateField<&'a str>,
}

/// Sum of usage rows for a single API key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSummary {
    pub total_rows: u64,
    pub unique_requests: u64,
    pub errors: u64,
    pub total_cost_usd: f64,
    pub last_used_at: Option<String>,
}

/// Number of seconds of "stale-ness" we tolerate before re-stamping `last_used_at`.
pub const LAST_USED_THROTTLE_SECS: i64 = 300;

/// Generate a new API key plaintext.
pub fn generate_plaintext() -> String {
    use rand::RngExt;
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    let suffix: String = (0..32)
        .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
        .collect();
    format!("op_live_{suffix}")
}

/// Hash a plaintext API key with SHA-256.
pub fn hash_key(plaintext: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(plaintext.as_bytes());
    hex::encode(hasher.finalize())
}

/// Parse an `expires_at` string into a UTC timestamp and return whether `now >= expires_at`.
pub fn is_expired(expires_at: Option<&str>, now: DateTime<Utc>) -> Result<bool> {
    let Some(s) = expires_at else {
        return Ok(false);
    };
    let dt = openproxy_types::timestamp::parse_timestamp(s)
        .map_err(|e| CoreError::Database {
            message: format!("invalid expires_at {s:?}: {e}"),
            source: None,
        })?
        .with_timezone(&Utc);
    Ok(now >= dt)
}
