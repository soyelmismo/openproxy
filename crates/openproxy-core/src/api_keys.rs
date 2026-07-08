//! API key CRUD. Keys are hashed with SHA-256 (high-entropy tokens, not
//! passwords). The plaintext is returned ONCE on creation/regeneration
//! and never stored.
//!
//! The wire shape of an `ApiKey` is the metadata *only* — `key_hash`
//! and `created_at` are exposed because the dashboard needs them, but
//! the plaintext never reappears after the create/regenerate call.
//!
//! Backwards compatibility: a row created by the 000001 migration
//! (just `id, key_hash, label, created_at`) will return with the
//! metadata fields defaulted by the 000015 migration
//! (`key_prefix=NULL`, `scopes=["chat"]`, `is_active=1`, etc.).

use crate::error::{CoreError, Result};
use crate::ids::ApiKeyId;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

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
    pub is_active: bool,
    pub revoked_at: Option<String>,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    pub created_at: String,
    pub created_by: Option<String>,
}

/// Input to [`create`]. All fields except `scopes` are optional; an
/// empty `scopes` vector is rejected by the caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateApiKeyInput {
    pub label: Option<String>,
    pub scopes: Vec<String>,
    pub allowed_models: Option<Vec<String>>,
    pub allowed_combos: Option<Vec<i64>>,
    pub expires_at: Option<String>,
}

/// Number of seconds of "stale-ness" we tolerate before re-stamping
/// `last_used_at`. A high-traffic key would otherwise rewrite the
/// column on every request, which both costs an UPDATE per call and
/// makes the column useless as a "when was this key last used"
/// indicator (it would always be "just now"). 5 minutes is a
/// reasonable compromise: visible to humans, cheap to maintain.
const LAST_USED_THROTTLE_SECS: i64 = 300;

/// Generate a new API key plaintext.
///
/// Format: `op_live_<32 random base62 chars>`.
///
/// The total length is 40 characters: the literal `op_live_` (8
/// chars) plus 32 random alphanumeric characters. 32 chars from a
/// 62-symbol alphabet is roughly 190 bits of entropy, well above
/// the threshold for safe offline-unguessable keys.
pub fn generate_plaintext() -> String {
    use rand::RngExt;
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    let suffix: String = (0..32)
        .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
        .collect();
    format!("op_live_{}", suffix)
}

/// Hash a plaintext API key. Uses SHA-256 hex-encoded, matching the
/// shape stored in `api_keys.key_hash`. SHA-256 is appropriate here
/// because the plaintext already has ~190 bits of entropy — we are
/// not protecting against a weak password, we are protecting against
/// DB dumps. Argon2 / bcrypt would be theatre at this entropy level
/// and would slow every chat-completion request for no gain.
pub fn hash_key(plaintext: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(plaintext.as_bytes());
    hex::encode(hasher.finalize())
}

/// LOW fix (#15): parse an `expires_at` string into a UTC timestamp
/// and return whether `now >= expires_at`.
///
/// The previous implementation compared the stored string
/// lexicographically against `now.format("%Y-%m-%d %H:%M:%S")`. That
/// works ONLY if the stored value uses the exact same zero-padded
/// format with a SPACE between the date and the time. The codebase
/// actually writes `%Y-%m-%dT%H:%M:%SZ` (RFC3339), where `T` (ASCII
/// 84) sorts AFTER the space (ASCII 32). The consequence: the
/// lexicographic check treated `2026-01-01T00:00:00Z` as greater
/// than any `2026-...` `now` string, so EVERY key with an
/// `expires_at` was effectively considered never-expiring.
///
/// This helper parses both sides through `chrono` and compares
/// timestamps directly. The wire format is unchanged
/// (RFC3339-ish strings in the column); the fix is in the reader.
///
/// Returns `Ok(None)` when `expires_at` is `None` (no expiry
/// configured). Returns `Err(_)` when the stored string cannot be
/// parsed as a timestamp — the caller should treat parse failure
/// as "expired" because we can't reason about a value we can't
/// understand.
pub fn is_expired(expires_at: Option<&str>, now: DateTime<Utc>) -> Result<bool> {
    let Some(s) = expires_at else {
        return Ok(false);
    };
    let dt = DateTime::parse_from_rfc3339(s)
        .map_err(|e| CoreError::Database {
            message: format!("invalid expires_at {:?}: {}", s, e),
            source: None,
        })?
        .with_timezone(&Utc);
    Ok(now >= dt)
}

/// Create a new API key. Returns the persisted row plus the plaintext
/// (shown to the user once and never re-derivable).
pub fn create(
    conn: &Connection,
    input: CreateApiKeyInput,
    created_by: &str,
) -> Result<(ApiKey, String)> {
    if input.scopes.is_empty() {
        return Err(CoreError::Validation(
            "scopes must contain at least one entry".into(),
        ));
    }
    let plaintext = generate_plaintext();
    let key_hash = hash_key(&plaintext);
    let key_prefix: String = plaintext.chars().take(12).collect();
    let scopes_json = serde_json::to_string(&input.scopes)
        .map_err(|e| CoreError::Parse(format!("serialize scopes: {e}")))?;
    let allowed_models_json = match &input.allowed_models {
        Some(v) => Some(
            serde_json::to_string(v)
                .map_err(|e| CoreError::Parse(format!("serialize allowed_models: {e}")))?,
        ),
        None => None,
    };
    let allowed_combos_json = match &input.allowed_combos {
        Some(v) => Some(
            serde_json::to_string(v)
                .map_err(|e| CoreError::Parse(format!("serialize allowed_combos: {e}")))?,
        ),
        None => None,
    };

    conn.execute(
        "INSERT INTO api_keys \
            (key_hash, key_prefix, label, scopes_json, allowed_models_json, \
             allowed_combos_json, expires_at, created_by) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            key_hash,
            key_prefix,
            input.label,
            scopes_json,
            allowed_models_json,
            allowed_combos_json,
            input.expires_at,
            created_by,
        ],
    )
    .map_err(crate::error::map_db_error)?;

    let id = ApiKeyId(conn.last_insert_rowid());
    let row = get_by_id(conn, id)?
        .ok_or_else(|| CoreError::Internal("failed to load newly-inserted api key".into()))?;
    Ok((row, plaintext))
}

/// Look up a single API key by id. Returns `Ok(None)` when absent.
pub fn get_by_id(conn: &Connection, id: ApiKeyId) -> Result<Option<ApiKey>> {
    let row = conn
        .query_row(
            "SELECT id, key_hash, key_prefix, label, scopes_json, \
                    allowed_models_json, allowed_combos_json, is_active, \
                    revoked_at, expires_at, last_used_at, created_at, created_by \
             FROM api_keys WHERE id = ?1",
            params![id.0],
            row_to_api_key,
        )
        .optional()
        .map_err(|e| CoreError::Database {
            message: format!("get api_key {}: {e}", id.0),
            source: Some(Box::new(e)),
        })?;
    Ok(row)
}

/// Look up by hash. Used by the chat handler on every authenticated
/// request, so it's a hot path: a single indexed lookup against
/// `key_hash` (UNIQUE).
pub fn get_by_hash(conn: &Connection, key_hash: &str) -> Result<Option<ApiKey>> {
    let row = conn
        .query_row(
            "SELECT id, key_hash, key_prefix, label, scopes_json, \
                    allowed_models_json, allowed_combos_json, is_active, \
                    revoked_at, expires_at, last_used_at, created_at, created_by \
             FROM api_keys WHERE key_hash = ?1",
            params![key_hash],
            row_to_api_key,
        )
        .optional()
        .map_err(crate::error::map_db_error)?;
    Ok(row)
}

/// Count the number of *active* API keys. Used by the chat
/// endpoint's anonymous-access gate: when at least one active key
/// is configured, the operator has explicitly opted into per-key
/// auth, so anonymous traffic must be rejected. When zero active
/// keys exist (typical local-dev / first-run state), the chat
/// endpoint stays open for backwards compatibility.
pub fn count_active(conn: &Connection) -> Result<u64> {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM api_keys WHERE is_active = 1",
            [],
            |row| row.get(0),
        )
        .map_err(crate::error::map_db_error)?;
    Ok(n.max(0) as u64)
}

/// List every API key, newest first.
pub fn list(conn: &Connection) -> Result<Vec<ApiKey>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, key_hash, key_prefix, label, scopes_json, \
                    allowed_models_json, allowed_combos_json, is_active, \
                    revoked_at, expires_at, last_used_at, created_at, created_by \
             FROM api_keys ORDER BY id DESC",
        )
        .map_err(crate::error::map_db_error)?;
    let rows = stmt
        .query_map([], row_to_api_key)
        .map_err(crate::error::map_db_error)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(crate::error::map_db_error)?);
    }
    Ok(out)
}

/// Soft-revoke: mark the key as inactive and stamp `revoked_at`.
/// The row is preserved for audit, but no future request will be
/// accepted with it.
pub fn revoke(conn: &Connection, id: ApiKeyId) -> Result<()> {
    let affected = conn
        .execute(
            "UPDATE api_keys \
             SET is_active = 0, \
                 revoked_at = COALESCE(revoked_at, datetime('now')) \
             WHERE id = ?1",
            params![id.0],
        )
        .map_err(|e| CoreError::Database {
            message: format!("revoke api_key {}: {e}", id.0),
            source: Some(Box::new(e)),
        })?;
    if affected == 0 {
        return Err(CoreError::Internal(format!("api_key {} not found", id.0)));
    }
    Ok(())
}

/// Hard delete. The `usage.api_key_id` FK is `ON DELETE SET NULL`,
/// so historical usage rows survive — the link is dropped and the
/// key disappears from the dashboard.
pub fn hard_delete(conn: &Connection, id: ApiKeyId) -> Result<()> {
    conn.execute("DELETE FROM api_keys WHERE id = ?1", params![id.0])
        .map_err(|e| CoreError::Database {
            message: format!("delete api_key {}: {e}", id.0),
            source: Some(Box::new(e)),
        })?;
    Ok(())
}

/// Issue a new plaintext and re-hash the row. The id and metadata
/// are preserved; only `key_hash`, `key_prefix`, and the revocation
/// state change. The previous plaintext is invalidated immediately.
pub fn regenerate(conn: &Connection, id: ApiKeyId) -> Result<(ApiKey, String)> {
    let plaintext = generate_plaintext();
    let key_hash = hash_key(&plaintext);
    let key_prefix: String = plaintext.chars().take(12).collect();

    let affected = conn
        .execute(
            "UPDATE api_keys \
             SET key_hash = ?1, key_prefix = ?2, \
                 is_active = 1, revoked_at = NULL, last_used_at = NULL \
             WHERE id = ?3",
            params![key_hash, key_prefix, id.0],
        )
        .map_err(|e| CoreError::Database {
            message: format!("regenerate api_key {}: {e}", id.0),
            source: Some(Box::new(e)),
        })?;
    if affected == 0 {
        return Err(CoreError::Internal(format!("api_key {} not found", id.0)));
    }

    let row = get_by_id(conn, id)?
        .ok_or_else(|| CoreError::Internal("regenerated api_key vanished".into()))?;
    Ok((row, plaintext))
}

/// Stamp `last_used_at` on the row. Throttled: we only UPDATE if
/// the existing `last_used_at` is older than
/// [`LAST_USED_THROTTLE_SECS`] (or NULL). This keeps the column
/// useful as a "when was this key last seen" signal without a
/// write-amplification problem on busy keys.
pub fn touch_last_used(conn: &Connection, id: ApiKeyId) -> Result<()> {
    let affected = conn
        .execute(
            "UPDATE api_keys SET last_used_at = datetime('now') \
             WHERE id = ?1 \
               AND (last_used_at IS NULL \
                    OR (julianday('now') - julianday(last_used_at)) * 86400 > ?2)",
            params![id.0, LAST_USED_THROTTLE_SECS],
        )
        .map_err(|e| CoreError::Database {
            message: format!("touch_last_used api_key {}: {e}", id.0),
            source: Some(Box::new(e)),
        })?;
    // affected=0 means either the id is gone (treat as no-op) or
    // the throttle kicked in. Both are fine, so we don't surface
    // the rowcount.
    let _ = affected;
    Ok(())
}

/// Partial update. The handler-side encoding uses `Option<Option<T>>`
/// to distinguish "leave alone" (outer `None`) from "clear to NULL"
/// (inner `None`); we flatten that here so the call site stays
/// readable.
///
/// `is_active = Some(false)` *also* stamps `revoked_at` (matching
/// the soft-revoke semantics) so a dashboard "disable" toggle and
/// the explicit revoke endpoint produce the same audit row.
#[derive(Default)]
pub struct UpdateParams<'a> {
    pub label: Option<&'a str>,
    pub scopes: Option<&'a [String]>,
    pub allowed_models: Option<Option<&'a [String]>>,
    pub allowed_combos: Option<Option<&'a [i64]>>,
    pub is_active: Option<bool>,
    pub expires_at: Option<Option<&'a str>>,
}

pub fn update(conn: &Connection, id: ApiKeyId, params: UpdateParams<'_>) -> Result<()> {
    if let Some(s) = params.scopes
        && s.is_empty()
    {
        return Err(CoreError::Validation(
            "scopes must contain at least one entry".into(),
        ));
    }
    let scopes_json: Option<String> = params
        .scopes
        .map(|s| {
            serde_json::to_string(s).map_err(|e| CoreError::Parse(format!("serialize scopes: {e}")))
        })
        .transpose()?;
    let allowed_models_json: Option<Option<String>> = params
        .allowed_models
        .map(|inner| {
            inner
                .map(|v| {
                    serde_json::to_string(v)
                        .map_err(|e| CoreError::Parse(format!("serialize allowed_models: {e}")))
                })
                .transpose()
        })
        .transpose()?;
    let allowed_combos_json: Option<Option<String>> = params
        .allowed_combos
        .map(|inner| {
            inner
                .map(|v| {
                    serde_json::to_string(v)
                        .map_err(|e| CoreError::Parse(format!("serialize allowed_combos: {e}")))
                })
                .transpose()
        })
        .transpose()?;
    let expires_at_str: Option<Option<&str>> = params.expires_at;

    // Build the dynamic SET clause. We only touch columns that the
    // caller actually provided, so a no-op PATCH round-trips through
    // the SQL with zero writes.
    let mut sets: Vec<&'static str> = Vec::new();
    let mut bound: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(label_value) = params.label {
        sets.push("label = ?");
        bound.push(Box::new(label_value.to_string()));
    }
    if let Some(s) = &scopes_json {
        sets.push("scopes_json = ?");
        bound.push(Box::new(s.clone()));
    }
    if let Some(om) = &allowed_models_json {
        sets.push("allowed_models_json = ?");
        bound.push(Box::new(om.clone()));
    }
    if let Some(oc) = &allowed_combos_json {
        sets.push("allowed_combos_json = ?");
        bound.push(Box::new(oc.clone()));
    }
    if let Some(active) = params.is_active {
        sets.push("is_active = ?");
        bound.push(Box::new(active as i64));
        if !active {
            // Audit-stamp the revoke time when the user disables a key
            // from the dashboard. Mirror the soft-revoke path so the
            // audit row is consistent regardless of which endpoint
            // the operator used.
            sets.push("revoked_at = COALESCE(revoked_at, datetime('now'))");
        } else {
            // Re-enabling a previously revoked key clears the audit
            // stamp. Re-revoking later will re-stamp it.
            sets.push("revoked_at = NULL");
        }
    }
    if let Some(oe) = expires_at_str {
        sets.push("expires_at = ?");
        bound.push(Box::new(oe.map(|s| s.to_string())));
    }

    if sets.is_empty() {
        // Nothing to do. Still verify the row exists so the caller
        // gets a clear error for a missing id.
        let present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM api_keys WHERE id = ?1",
                params![id.0],
                |r| r.get(0),
            )
            .map_err(|e| CoreError::Database {
                message: format!("count api_key {}: {e}", id.0),
                source: Some(Box::new(e)),
            })?;
        if present == 0 {
            return Err(CoreError::Internal(format!("api_key {} not found", id.0)));
        }
        return Ok(());
    }

    let sql = format!("UPDATE api_keys SET {} WHERE id = ?", sets.join(", "));
    bound.push(Box::new(id.0));
    let param_refs: Vec<&dyn rusqlite::ToSql> = bound
        .iter()
        .map(|b| b.as_ref() as &dyn rusqlite::ToSql)
        .collect();

    let affected = conn
        .execute(&sql, rusqlite::params_from_iter(param_refs))
        .map_err(|e| CoreError::Database {
            message: format!("update api_key {}: {e}", id.0),
            source: Some(Box::new(e)),
        })?;
    if affected == 0 {
        return Err(CoreError::Internal(format!("api_key {} not found", id.0)));
    }
    Ok(())
}

/// Sum of usage rows for a single API key. Used by the per-key
/// dashboard view. Returns a flat row of metrics; cheaper than
/// running the full `usage::summary` machinery when we just want
/// the headline numbers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSummary {
    pub total_rows: u64,
    pub unique_requests: u64,
    pub errors: u64,
    pub total_cost_usd: f64,
    pub last_used_at: Option<String>,
}

pub fn usage_summary(conn: &Connection, id: ApiKeyId) -> Result<UsageSummary> {
    // total_rows + unique_requests + errors + cost, scoped to this key.
    let row = conn
        .query_row(
            "SELECT \
                 COUNT(*), \
                 COUNT(DISTINCT request_id), \
                 SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END), \
                 COALESCE(SUM(cost_usd), 0.0) \
             FROM usage WHERE api_key_id = ?1",
            params![id.0],
            |r| {
                let total: i64 = r.get(0)?;
                let unique: i64 = r.get(1)?;
                let errors: Option<i64> = r.get(2)?;
                let cost: f64 = r.get(3)?;
                Ok((total, unique, errors.unwrap_or(0), cost))
            },
        )
        .map_err(|e| CoreError::Database {
            message: format!("usage_summary for api_key {}: {e}", id.0),
            source: Some(Box::new(e)),
        })?;
    let last_used_at: Option<String> = conn
        .query_row(
            "SELECT last_used_at FROM api_keys WHERE id = ?1",
            params![id.0],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| CoreError::Database {
            message: format!("select last_used_at for api_key {}: {e}", id.0),
            source: Some(Box::new(e)),
        })?
        .flatten(); // outer None (row gone) → None; inner None (column NULL) → None.

    Ok(UsageSummary {
        total_rows: row.0.max(0) as u64,
        unique_requests: row.1.max(0) as u64,
        errors: row.2.max(0) as u64,
        total_cost_usd: row.3,
        last_used_at,
    })
}

// ---------------------------------------------------------------------------
// Row mapper
// ---------------------------------------------------------------------------

/// Map a single SELECT row into an `ApiKey`. Shared by `get_by_id`,
/// `get_by_hash`, and `list`.
///
/// The column order MUST match every SELECT in this module; the
/// easiest way to keep them aligned is to use the same string in
/// all three queries (see the literal above).
fn row_to_api_key(row: &Row<'_>) -> rusqlite::Result<ApiKey> {
    let id: i64 = row.get(0)?;
    let key_hash: String = row.get(1)?;
    let key_prefix: Option<String> = row.get(2)?;
    let label: Option<String> = row.get(3)?;
    let scopes_json: String = row.get(4)?;
    let allowed_models_json: Option<String> = row.get(5)?;
    let allowed_combos_json: Option<String> = row.get(6)?;
    let is_active: i64 = row.get(7)?;
    let revoked_at: Option<String> = row.get(8)?;
    let expires_at: Option<String> = row.get(9)?;
    let last_used_at: Option<String> = row.get(10)?;
    let created_at: String = row.get(11)?;
    let created_by: Option<String> = row.get(12)?;

    let scopes: Vec<String> = serde_json::from_str(&scopes_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(SimpleErr(format!("decode scopes_json: {e}"))),
        )
    })?;
    let allowed_models = match allowed_models_json {
        Some(s) if !s.is_empty() => Some(serde_json::from_str(&s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                Box::new(SimpleErr(format!("decode allowed_models_json: {e}"))),
            )
        })?),
        _ => None,
    };
    let allowed_combos = match allowed_combos_json {
        Some(s) if !s.is_empty() => Some(serde_json::from_str(&s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Text,
                Box::new(SimpleErr(format!("decode allowed_combos_json: {e}"))),
            )
        })?),
        _ => None,
    };

    Ok(ApiKey {
        id: ApiKeyId(id),
        key_hash,
        key_prefix,
        label,
        scopes,
        allowed_models,
        allowed_combos,
        is_active: is_active != 0,
        revoked_at,
        expires_at,
        last_used_at,
        created_at,
        created_by,
    })
}

#[derive(Debug)]
struct SimpleErr(String);
impl std::fmt::Display for SimpleErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SimpleErr {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use std::path::PathBuf;

    fn fresh_pool() -> (Connection, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("openproxy-apikeys-test-{}-{}-{}", pid, nanos, n));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("apikeys.db");
        let mut conn = Connection::open(&path).expect("open");
        migrations::run(&mut conn).expect("migrate");
        (conn, path)
    }

    fn make_input(label: &str) -> CreateApiKeyInput {
        CreateApiKeyInput {
            label: Some(label.to_string()),
            scopes: vec!["chat".to_string()],
            allowed_models: None,
            allowed_combos: None,
            expires_at: None,
        }
    }

    #[test]
    fn hash_key_is_deterministic() {
        let h1 = hash_key("op_live_abc");
        let h2 = hash_key("op_live_abc");
        assert_eq!(h1, h2);
        // SHA-256 hex is 64 chars.
        assert_eq!(h1.len(), 64);
        // Different inputs → different hashes.
        assert_ne!(h1, hash_key("op_live_abd"));
    }

    #[test]
    fn generate_plaintext_has_correct_format() {
        let p = generate_plaintext();
        assert!(p.starts_with("op_live_"), "got: {p}");
        // "op_live_" = 8 chars + 32 random = 40 total.
        assert_eq!(p.len(), 40);
        // Two calls produce different outputs (32 chars of randomness).
        assert_ne!(p, generate_plaintext());
    }

    // ---- LOW fix (#15): expires_at must be parsed, not lex-compared.
    // The bug: stored value uses `%Y-%m-%dT%H:%M:%SZ` and the old check
    // compared against `now.format("%Y-%m-%d %H:%M:%S")`. Because `T`
    // (0x54) sorts AFTER space (0x20), the lexicographic check treated
    // every T-formatted future date as "not yet expired". These tests
    // pin down the new parser-based semantics.

    #[test]
    fn is_expired_returns_false_when_none() {
        let now = chrono::Utc::now();
        assert!(!is_expired(None, now).expect("ok"));
    }

    #[test]
    fn is_expired_returns_true_when_now_is_after_rfc3339() {
        // Stored in the exact format the codebase writes today.
        let stored = "2020-01-01T00:00:00Z";
        let now = "2025-06-18T12:00:00Z"
            .parse::<chrono::DateTime<Utc>>()
            .unwrap();
        assert!(
            is_expired(Some(stored), now).expect("ok"),
            "2020 timestamp with a 2025 now must be expired"
        );
    }

    #[test]
    fn is_expired_returns_false_when_future_rfc3339() {
        let stored = "2099-12-31T23:59:59Z";
        let now = chrono::Utc::now();
        assert!(!is_expired(Some(stored), now).expect("ok"));
    }

    #[test]
    fn is_expired_treats_malformed_as_error() {
        let now = chrono::Utc::now();
        let err = is_expired(Some("not-a-date"), now).expect_err("parse error");
        assert!(matches!(err, CoreError::Database { .. }));
    }

    #[test]
    fn is_expired_handles_offset_timezone() {
        // RFC3339 with a non-Z timezone must be normalised to UTC
        // before comparison. 2025-06-18T15:00:00+02:00 == 13:00 UTC.
        let stored = "2025-06-18T15:00:00+02:00";
        let now_just_before = "2025-06-18T12:59:59Z"
            .parse::<chrono::DateTime<Utc>>()
            .unwrap();
        let now_just_after = "2025-06-18T13:00:01Z"
            .parse::<chrono::DateTime<Utc>>()
            .unwrap();

        assert!(!is_expired(Some(stored), now_just_before).expect("ok"));
        assert!(is_expired(Some(stored), now_just_after).expect("ok"));
    }

    #[test]
    fn create_returns_plaintext_once() {
        let (conn, _p) = fresh_pool();
        let (key, plaintext) = create(&conn, make_input("test"), "admin").expect("create");
        assert!(plaintext.starts_with("op_live_"));
        // The hash is stored; the plaintext is not.
        assert_eq!(key.key_hash, hash_key(&plaintext));
        // First 12 chars of the plaintext round-trip into key_prefix.
        assert_eq!(key.key_prefix.as_deref(), Some(&plaintext[..12]));
        assert_eq!(key.scopes, vec!["chat".to_string()]);
        assert!(key.is_active);
        assert!(key.revoked_at.is_none());
        assert!(key.last_used_at.is_none());
        assert_eq!(key.created_by.as_deref(), Some("admin"));
    }

    #[test]
    fn create_rejects_empty_scopes() {
        let (conn, _p) = fresh_pool();
        let input = CreateApiKeyInput {
            label: None,
            scopes: vec![],
            allowed_models: None,
            allowed_combos: None,
            expires_at: None,
        };
        let err = create(&conn, input, "admin").expect_err("empty scopes");
        assert!(matches!(err, CoreError::Validation(_)));
    }

    #[test]
    fn get_by_hash_roundtrip() {
        let (conn, _p) = fresh_pool();
        let (key, plaintext) = create(&conn, make_input("rt"), "admin").expect("create");

        let by_hash = get_by_hash(&conn, &hash_key(&plaintext)).expect("by hash");
        assert!(by_hash.is_some());
        let by_hash = by_hash.unwrap();
        assert_eq!(by_hash.id, key.id);
        assert_eq!(by_hash.label.as_deref(), Some("rt"));

        // Wrong hash → None.
        let miss = get_by_hash(&conn, "deadbeef").expect("miss");
        assert!(miss.is_none());

        // get_by_id round-trip.
        let by_id = get_by_id(&conn, key.id).expect("by id").expect("present");
        assert_eq!(by_id.id, key.id);
    }

    #[test]
    fn list_returns_all_keys_newest_first() {
        let (conn, _p) = fresh_pool();
        let (k1, _) = create(&conn, make_input("a"), "admin").expect("a");
        let (k2, _) = create(&conn, make_input("b"), "admin").expect("b");
        let (k3, _) = create(&conn, make_input("c"), "admin").expect("c");

        let all = list(&conn).expect("list");
        assert_eq!(all.len(), 3);
        // Newest first → k3, k2, k1.
        assert_eq!(all[0].id, k3.id);
        assert_eq!(all[1].id, k2.id);
        assert_eq!(all[2].id, k1.id);
    }

    #[test]
    fn revoke_deactivates() {
        let (conn, _p) = fresh_pool();
        let (key, _) = create(&conn, make_input("rev"), "admin").expect("create");

        revoke(&conn, key.id).expect("revoke");
        let after = get_by_id(&conn, key.id).expect("get").expect("present");
        assert!(!after.is_active, "is_active flipped to 0");
        assert!(after.revoked_at.is_some(), "revoked_at stamped");

        // Idempotent: a second revoke preserves the original timestamp.
        let first_ts = after.revoked_at.clone().unwrap();
        revoke(&conn, key.id).expect("revoke 2");
        let after2 = get_by_id(&conn, key.id).expect("get 2").expect("present");
        assert_eq!(after2.revoked_at.as_deref(), Some(first_ts.as_str()));
    }

    #[test]
    fn hard_delete_removes_row() {
        let (conn, _p) = fresh_pool();
        let (key, _) = create(&conn, make_input("del"), "admin").expect("create");
        hard_delete(&conn, key.id).expect("delete");
        assert!(get_by_id(&conn, key.id).expect("get").is_none());
        // Idempotent.
        hard_delete(&conn, key.id).expect("delete again");
    }

    #[test]
    fn regenerate_changes_hash_and_keeps_id() {
        let (conn, _p) = fresh_pool();
        let (key, plaintext) = create(&conn, make_input("regen"), "admin").expect("create");
        let old_hash = key.key_hash.clone();
        let old_prefix = key.key_prefix.clone();

        let (regen, new_plaintext) = regenerate(&conn, key.id).expect("regenerate");
        assert_ne!(new_plaintext, plaintext, "new plaintext differs");
        assert_ne!(regen.key_hash, old_hash, "hash changed");
        assert_ne!(regen.key_prefix, old_prefix, "prefix changed");
        assert_eq!(regen.id, key.id, "id preserved");
        assert!(regen.is_active, "regenerate re-activates");
        assert!(regen.revoked_at.is_none(), "regenerate clears revoked_at");

        // The old plaintext no longer maps to a row.
        let miss = get_by_hash(&conn, &hash_key(&plaintext)).expect("miss");
        assert!(miss.is_none(), "old plaintext is invalidated");

        // The new plaintext does.
        let hit = get_by_hash(&conn, &hash_key(&new_plaintext))
            .expect("hit")
            .expect("present");
        assert_eq!(hit.id, key.id);
    }

    #[test]
    fn update_patches_subset() {
        let (conn, _p) = fresh_pool();
        let (key, _) = create(&conn, make_input("u"), "admin").expect("create");

        // Update only label.
        update(
            &conn,
            key.id,
            UpdateParams {
                label: Some("renamed"),
                ..Default::default()
            },
        )
        .expect("update label");
        let after = get_by_id(&conn, key.id).expect("get").expect("present");
        assert_eq!(after.label.as_deref(), Some("renamed"));
        assert_eq!(after.scopes, vec!["chat".to_string()], "scopes unchanged");

        // Update scopes to ["manage", "read"].
        update(
            &conn,
            key.id,
            UpdateParams {
                scopes: Some(&["manage".to_string(), "read".to_string()]),
                ..Default::default()
            },
        )
        .expect("update scopes");
        let after = get_by_id(&conn, key.id).expect("get").expect("present");
        assert_eq!(after.scopes, vec!["manage".to_string(), "read".to_string()]);

        // Update allowed_models via Some(Some(&slice)).
        update(
            &conn,
            key.id,
            UpdateParams {
                allowed_models: Some(Some(&["openai/gpt-4o".to_string()])),
                ..Default::default()
            },
        )
        .expect("update allowed_models");
        let after = get_by_id(&conn, key.id).expect("get").expect("present");
        assert_eq!(
            after.allowed_models,
            Some(vec!["openai/gpt-4o".to_string()])
        );

        // Clear allowed_models via Some(None).
        update(
            &conn,
            key.id,
            UpdateParams {
                label: None,
                scopes: None,
                allowed_models: Some(None),
                allowed_combos: None,
                is_active: None,
                expires_at: None,
            },
        )
        .expect("clear allowed_models");
        let after = get_by_id(&conn, key.id).expect("get").expect("present");
        assert!(after.allowed_models.is_none(), "allowed_models cleared");

        // Reject empty scopes.
        let err = update(
            &conn,
            key.id,
            UpdateParams {
                label: None,
                scopes: Some(&[]),
                allowed_models: None,
                allowed_combos: None,
                is_active: None,
                expires_at: None,
            },
        )
        .expect_err("empty scopes");
        assert!(matches!(err, CoreError::Validation(_)));

        // Missing id → Internal.
        let err = update(
            &conn,
            ApiKeyId(9999),
            UpdateParams {
                label: Some("x"),
                ..Default::default()
            },
        )
        .expect_err("missing");
        assert!(matches!(err, CoreError::Internal(_)));
    }

    #[test]
    fn update_disable_stamps_revoked_at() {
        let (conn, _p) = fresh_pool();
        let (key, _) = create(&conn, make_input("dis"), "admin").expect("create");

        update(
            &conn,
            key.id,
            UpdateParams {
                label: None,
                scopes: None,
                allowed_models: None,
                allowed_combos: None,
                is_active: Some(false),
                expires_at: None,
            },
        )
        .expect("disable");
        let after = get_by_id(&conn, key.id).expect("get").expect("present");
        assert!(!after.is_active);
        assert!(after.revoked_at.is_some());

        // Re-enable clears revoked_at.
        update(
            &conn,
            key.id,
            UpdateParams {
                label: None,
                scopes: None,
                allowed_models: None,
                allowed_combos: None,
                is_active: Some(true),
                expires_at: None,
            },
        )
        .expect("enable");
        let after = get_by_id(&conn, key.id).expect("get").expect("present");
        assert!(after.is_active);
        assert!(after.revoked_at.is_none());
    }

    #[test]
    fn touch_last_used_throttles() {
        let (conn, _p) = fresh_pool();
        let (key, _) = create(&conn, make_input("throttle"), "admin").expect("create");

        // First touch stamps.
        touch_last_used(&conn, key.id).expect("touch 1");
        let after1 = get_by_id(&conn, key.id).expect("get").expect("present");
        let ts1 = after1.last_used_at.clone().expect("stamp 1");

        // Second touch immediately is throttled: column unchanged.
        touch_last_used(&conn, key.id).expect("touch 2");
        let after2 = get_by_id(&conn, key.id).expect("get").expect("present");
        let ts2 = after2.last_used_at.clone().expect("stamp 2");
        assert_eq!(ts1, ts2, "throttled: same timestamp");

        // Manually move last_used_at to > 5 min ago → next touch wins.
        conn.execute(
            "UPDATE api_keys SET last_used_at = '2020-01-01 00:00:00' WHERE id = ?1",
            params![key.id.0],
        )
        .expect("manual backdate");
        touch_last_used(&conn, key.id).expect("touch 3");
        let after3 = get_by_id(&conn, key.id).expect("get").expect("present");
        assert_ne!(after3.last_used_at.as_deref(), Some("2020-01-01 00:00:00"));
    }

    #[test]
    fn legacy_row_gets_metadata_defaults() {
        // Pre-000015 row: only id, key_hash, label, created_at populated.
        // The migration's column defaults must surface in get_by_id.
        let (conn, _p) = fresh_pool();
        conn.execute(
            "INSERT INTO api_keys (key_hash, label) VALUES (?1, ?2)",
            params!["abc123hash", "legacy"],
        )
        .expect("insert legacy");

        let all = list(&conn).expect("list");
        assert_eq!(all.len(), 1);
        let k = &all[0];
        // Defaults from the migration.
        assert_eq!(k.scopes, vec!["chat".to_string()]);
        assert!(k.is_active, "is_active defaults to 1");
        assert!(k.key_prefix.is_none(), "no prefix for legacy");
        assert!(k.allowed_models.is_none());
        assert!(k.allowed_combos.is_none());
        assert!(k.revoked_at.is_none());
        assert!(k.expires_at.is_none());
        assert!(k.last_used_at.is_none());
    }

    // ---- MEDIUM fix: count_active drives the anonymous-access gate
    // in `chat::authenticate`. A fresh DB has zero → anonymous OK
    // (local-dev). After the operator creates the first key the
    // count is ≥ 1 → chat endpoint requires a key. Revoked keys
    // must NOT count (they cannot authenticate either).

    #[test]
    fn count_active_is_zero_on_fresh_db() {
        let (conn, _p) = fresh_pool();
        assert_eq!(count_active(&conn).expect("count"), 0);
    }

    #[test]
    fn count_active_returns_one_after_create() {
        let (conn, _p) = fresh_pool();
        create(&conn, make_input("a"), "admin").expect("create");
        assert_eq!(count_active(&conn).expect("count"), 1);
    }

    #[test]
    fn count_active_excludes_revoked_keys() {
        let (conn, _p) = fresh_pool();
        let (k1, _) = create(&conn, make_input("a"), "admin").expect("create");
        create(&conn, make_input("b"), "admin").expect("create");
        // Revoke k1. count_active must drop back to 1.
        revoke(&conn, k1.id).expect("revoke");
        assert_eq!(count_active(&conn).expect("count"), 1);
    }
}
