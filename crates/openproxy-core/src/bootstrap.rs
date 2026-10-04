//! Bootstrap API key: if the database is empty when the server starts,
//! seed a single admin key with `["manage", "chat"]` scope and hand the
//! plaintext to the operator through a **root-only file** next to the
//! database (or the path in `OPENPROXY_BOOTSTRAP_KEY_FILE`). Only the key
//! id, prefix and the file path are logged.
//!
//! The plaintext is deliberately NOT written to stdout/stderr: under
//! systemd (`StandardError=journal`) and Docker (`docker logs`) both
//! streams land in the same indexed, long-retained log store as the
//! structured log pipeline, so "just stderr" was never a safe channel.
//!
//! One-shot: a populated `api_keys` table is a no-op, so restarts are safe.
//!
//! Disable by leaving the `api_keys` table non-empty at boot (the
//! normal case after the first run).

use crate::api_keys::{self, CreateApiKeyInput};
use crate::error::Result;
use crate::ids::ApiKeyId;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Public, HTTP-friendly view of a bootstrap-key row. Returned to
/// the admin handler so the UI can render the plaintext in a copy-
/// able modal without a second DB round-trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapResult {
    pub id: ApiKeyId,
    pub plaintext: String,
    pub key_prefix: Option<String>,
}

/// Environment variable to explicitly opt-in to logging the bootstrap key plaintext to stdout/stderr.
///
/// SECURITY: This should only be used in ephemeral container environments (e.g. Koyeb Free,
/// Railway, Fly.io without persistent storage) where the 0600 drop file cannot be accessed.
pub const LOG_BOOTSTRAP_KEY_ENV: &str = "OPENPROXY_LOG_BOOTSTRAP_KEY";

/// Check if the operator explicitly opted in to logging the bootstrap administrative API key.
pub fn is_bootstrap_key_log_enabled() -> bool {
    std::env::var(LOG_BOOTSTRAP_KEY_ENV).is_ok_and(|v| parse_bool_env(&v))
}

/// Helper function to parse truthy boolean environment values.
pub fn parse_bool_env(val: &str) -> bool {
    matches!(
        val.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
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
            source: Some(std::sync::Arc::new(e)),
        })?;
    if count > 0 {
        return Ok(None);
    }

    let (key, plaintext) = api_keys::create(
        conn,
        CreateApiKeyInput {
            label: Some(label.to_string()),
            scopes: vec!["manage".to_string(), "chat".to_string()],
            ..Default::default()
        },
        "system",
    )?;

    // SECURITY: by default, the plaintext never reaches any log stream; only the 0600 file
    // path is logged, at WARN because the operator must act.
    // Explicit opt-in via OPENPROXY_LOG_BOOTSTRAP_KEY is supported for ephemeral/distroless environments.
    if is_bootstrap_key_log_enabled() {
        tracing::warn!(
            "============================== SECURITY WARNING ==============================\n\
             OPENPROXY_LOG_BOOTSTRAP_KEY is enabled. The bootstrap administrative API key\n\
             will be printed to logs. Ensure log access is strictly restricted.\n\
             =============================================================================="
        );
        tracing::warn!(
            key_id = key.id.0,
            prefix = ?key.key_prefix,
            api_key = %plaintext,
            "Bootstrap API key created: {}",
            plaintext
        );
    }

    match write_bootstrap_key_file(conn, &plaintext) {
        Ok(Some(path)) => tracing::warn!(
            key_id = key.id.0,
            prefix = ?key.key_prefix,
            path = %path.display(),
            "Bootstrap API key created. Plaintext written to a root-only (0600) file — \
             read it, store it in your secret manager, then delete the file.",
        ),
        Ok(None) => tracing::warn!(
            key_id = key.id.0,
            prefix = ?key.key_prefix,
            "Bootstrap API key created but the database has no on-disk path and \
             OPENPROXY_BOOTSTRAP_KEY_FILE is unset; the plaintext was not persisted. \
             Set OPENPROXY_BOOTSTRAP_KEY_FILE (or OPENPROXY_LOG_BOOTSTRAP_KEY=1) and restart with an empty api_keys table.",
        ),
        Err(e) => tracing::error!(
            key_id = key.id.0,
            prefix = ?key.key_prefix,
            error = %e,
            "Bootstrap API key created but writing the plaintext file failed. \
             Delete the row from api_keys (or set OPENPROXY_BOOTSTRAP_KEY_FILE to a \
             writable path, or OPENPROXY_LOG_BOOTSTRAP_KEY=1) and restart to re-issue.",
        ),
    }

    Ok(Some(BootstrapResult {
        id: key.id,
        plaintext,
        key_prefix: key.key_prefix,
    }))
}

/// Name of the plaintext drop file when `OPENPROXY_BOOTSTRAP_KEY_FILE`
/// is unset: a sibling of the SQLite database.
pub const BOOTSTRAP_KEY_FILENAME: &str = "bootstrap-api-key.txt";

/// Resolve where the plaintext should be written: the env override, or
/// `<db dir>/bootstrap-api-key.txt` for an on-disk database. `None` for
/// in-memory databases without an override.
fn bootstrap_key_path(conn: &Connection) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("OPENPROXY_BOOTSTRAP_KEY_FILE").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let db_path = conn.path().filter(|p| !p.is_empty() && *p != ":memory:")?;
    let parent = Path::new(db_path).parent()?;
    Some(parent.join(BOOTSTRAP_KEY_FILENAME))
}

/// Create the drop file with mode 0600 (owner read/write only) and write
/// the plaintext. Fails closed if the file already exists so a stale
/// file is never silently overwritten (and so we never truncate a path an
/// attacker pre-created as a symlink).
fn write_bootstrap_key_file(
    conn: &Connection,
    plaintext: &str,
) -> std::io::Result<Option<PathBuf>> {
    let Some(path) = bootstrap_key_path(conn) else {
        return Ok(None);
    };
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&path)?;
    writeln!(
        f,
        "# openproxy bootstrap API key (scopes: manage, chat)\n\
         # Store it in your secret manager and DELETE this file.\n\
         {plaintext}"
    )?;
    f.sync_all()?;
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    fn fresh_conn() -> (Connection, PathBuf) {
        let conn = openproxy_db::testing::open_in_memory();
        (conn, PathBuf::from(":memory:"))
    }

    #[test]
    fn bootstrap_writes_plaintext_to_root_only_file_next_to_db() {
        let dir = openproxy_db::testing::TempDir::new("openproxy-bootstrap").expect("tmp");
        let db_path = dir.path().join("data.db");
        let mut conn = Connection::open(&db_path).expect("open db");
        openproxy_db::migrations::run(&mut conn).expect("migrations");

        let r = ensure_bootstrap_key(&conn, "bootstrap")
            .expect("bootstrap")
            .expect("created");

        let file = dir.path().join(BOOTSTRAP_KEY_FILENAME);
        let contents = std::fs::read_to_string(&file).expect("drop file exists");
        assert!(contents.lines().any(|l| l == r.plaintext), "{contents}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "drop file must be owner-only");
        }
    }

    #[test]
    fn bootstrap_creates_first_key() {
        let (conn, _p) = fresh_conn();
        let result = ensure_bootstrap_key(&conn, "bootstrap").expect("bootstrap");
        let r = result.expect("non-empty result on empty table");
        assert!(r.plaintext.starts_with("op_live_"));
        // manage + chat so one key reaches both endpoint families.
        let key = api_keys::get_by_id(&conn, r.id)
            .expect("get")
            .expect("present");
        assert!(key.scopes.contains(&"manage".to_string()));
        assert!(key.scopes.contains(&"chat".to_string()));
    }

    #[test]
    fn bootstrap_is_noop_when_keys_exist() {
        let (conn, _p) = fresh_conn();
        let (_existing, _) = api_keys::create(
            &conn,
            CreateApiKeyInput {
                label: Some("pre".into()),
                scopes: vec!["chat".into()],
                ..Default::default()
            },
            "admin",
        )
        .expect("seed");

        let r = ensure_bootstrap_key(&conn, "bootstrap").expect("bootstrap");
        assert!(r.is_none(), "no-op on populated table");
    }

    #[test]
    fn test_parse_bool_env() {
        assert!(parse_bool_env("1"));
        assert!(parse_bool_env("true"));
        assert!(parse_bool_env("TRUE"));
        assert!(parse_bool_env("True"));
        assert!(parse_bool_env("yes"));
        assert!(parse_bool_env("YES"));
        assert!(parse_bool_env("on"));
        assert!(parse_bool_env("ON"));
        assert!(parse_bool_env("  true  "));

        assert!(!parse_bool_env("0"));
        assert!(!parse_bool_env("false"));
        assert!(!parse_bool_env("no"));
        assert!(!parse_bool_env("off"));
        assert!(!parse_bool_env(""));
        assert!(!parse_bool_env("random_string"));
    }

    #[test]
    fn test_bootstrap_log_flag_default() {
        // By default, if the env var is not set, logging is disabled
        if std::env::var_os(LOG_BOOTSTRAP_KEY_ENV).is_none() {
            assert!(!is_bootstrap_key_log_enabled());
        }
    }
}
