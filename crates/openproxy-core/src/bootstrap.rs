//! Bootstrap API key: if the database is empty when the server starts,
//! seed a single admin key with `["manage", "chat"]` scope and print
//! the plaintext to the logs (and stderr, for visibility). The
//! operator copies it out of the boot logs and uses it as their
//! first API key.
//!
//! The behaviour is intentionally one-shot: subsequent starts see
//! existing keys and do nothing. Re-running the bootstrap path on a
//! populated DB is a no-op so the operator can safely restart the
//! server.
//!
//! Disable by leaving the `api_keys` table non-empty at boot (the
//! normal case after the first run).

use crate::api_keys::{self, CreateApiKeyInput};
use crate::error::Result;
use crate::ids::ApiKeyId;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// Public, HTTP-friendly view of a bootstrap-key row. Returned to
/// the admin handler so the UI can render the plaintext in a copy-
/// able modal without a second DB round-trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapResult {
    pub id: ApiKeyId,
    pub plaintext: String,
    pub key_prefix: Option<String>,
}

/// If `api_keys` is empty, insert a single bootstrap key with
/// `["manage", "chat"]` scope. The plaintext is returned to the
/// caller and printed to logs (WARN level) so the operator can save
/// it. A non-empty table is a no-op.
pub fn ensure_bootstrap_key(conn: &Connection, label: &str) -> Result<Option<BootstrapResult>> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM api_keys", [], |r| r.get(0))
        .map_err(|e| crate::error::CoreError::Database {
            message: format!("count api_keys: {e}"),
            source: Some(Box::new(e)),
        })?;
    if count > 0 {
        return Ok(None);
    }

    let (key, plaintext) = api_keys::create(
        conn,
        CreateApiKeyInput {
            label: Some(label.to_string()),
            scopes: vec!["manage".to_string(), "chat".to_string()],
            allowed_models: None,
            allowed_combos: None,
            expires_at: None,
        },
        "system",
    )?;

    // WARN, not INFO, because the operator must take action
    // (copy the key). A regular INFO would scroll past too easily
    // in a default `RUST_LOG=info` deployment.
    //
    // SECURITY: do NOT include `plaintext` as a structured field —
    // log aggregators (Datadog, CloudWatch, Loki) index structured
    // fields and store them indefinitely, making exfiltration trivial.
    // The plaintext is only sent to stderr (eprintln!) below, which
    // goes to the console/journalctl but not to the structured log
    // pipeline.
    tracing::warn!(
        key_id = key.id.0,
        prefix = ?key.key_prefix,
        "Bootstrap API key created. Check stderr (journalctl) for the plaintext — it is not stored in plaintext anywhere.",
    );
    // Also surface on stderr: containerized deployments often have
    // log collection that swallows WARN from the application layer,
    // and the operator scanning `journalctl -u openproxy-core` will
    // see this without filter setup.
    eprintln!(
        "openproxy bootstrap key (id={}): {}\n  ^- save this NOW, it is not stored in plaintext anywhere",
        key.id.0, plaintext
    );

    Ok(Some(BootstrapResult {
        id: key.id,
        plaintext,
        key_prefix: key.key_prefix,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    fn fresh_conn() -> (Connection, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("openproxy-bootstrap-test-{}-{}-{}", pid, nanos, n));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("bootstrap.db");
        let mut conn = Connection::open(&path).expect("open");
        openproxy_db::migrations::run(&mut conn).expect("migrate");
        (conn, path)
    }

    #[test]
    fn bootstrap_creates_first_key() {
        let (conn, _p) = fresh_conn();
        let result = ensure_bootstrap_key(&conn, "bootstrap").expect("bootstrap");
        let r = result.expect("non-empty result on empty table");
        assert!(r.plaintext.starts_with("op_live_"));
        // Scope includes both manage + chat so the operator can hit
        // admin and chat endpoints with the same key.
        let key = api_keys::get_by_id(&conn, r.id)
            .expect("get")
            .expect("present");
        assert!(key.scopes.contains(&"manage".to_string()));
        assert!(key.scopes.contains(&"chat".to_string()));
    }

    #[test]
    fn bootstrap_is_noop_when_keys_exist() {
        let (conn, _p) = fresh_conn();
        // Pre-populate.
        let (_existing, _) = api_keys::create(
            &conn,
            CreateApiKeyInput {
                label: Some("pre".into()),
                scopes: vec!["chat".into()],
                allowed_models: None,
                allowed_combos: None,
                expires_at: None,
            },
            "admin",
        )
        .expect("seed");

        let r = ensure_bootstrap_key(&conn, "bootstrap").expect("bootstrap");
        assert!(r.is_none(), "no-op on populated table");
    }
}
