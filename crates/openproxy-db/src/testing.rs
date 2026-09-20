//! Test-only and testing utilities for isolated SQLite databases,
//! temporary directories with RAII cleanup, and seed helpers.

use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// RAII temporary directory that guarantees recursive deletion on drop.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create a new uniquely named directory under [`std::env::temp_dir`].
    pub fn new(prefix: &str) -> std::io::Result<Self> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!("{prefix}-{pid}-{nanos}-{n}"));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// The absolute path to this temporary directory.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if self.path.exists() && std::fs::remove_dir_all(&self.path).is_err() {
            std::thread::sleep(std::time::Duration::from_millis(10));
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Open a fresh in-memory SQLite database, run all migrations, and
/// return the connection. Every test gets its own DB; there is no
/// shared state between tests and zero disk I/O.
pub fn open_in_memory() -> Connection {
    let mut conn = match Connection::open_in_memory() {
        Ok(c) => c,
        Err(e) => panic!("open in-memory db: {e}"),
    };
    if let Err(e) = crate::migrations::run(&mut conn) {
        panic!("run migrations: {e}");
    }
    conn
}

/// Seed the `providers` row required by the
/// `accounts.provider_id NOT NULL REFERENCES providers(id)` FK
/// constraint declared in `migrations/000019_add_oauth_support.sql`.
/// Tests that `INSERT INTO accounts` MUST call this first.
pub fn seed_antigravity_provider(conn: &Connection) {
    if let Err(e) = conn.execute(
        "INSERT INTO providers (id, name, base_url, auth_type, format) \
         VALUES ('antigravity', 'Antigravity', 'https://example.com', \
                 'oauth', 'openai')",
        [],
    ) {
        panic!("seed antigravity provider: {e}");
    }
}

/// Create a fresh isolated test pool with all migrations applied.
/// Returns both the pool and the path to its SQLite database file.
pub fn fresh_pool() -> (crate::conn::DbPool, PathBuf) {
    fresh_pool_with_prefix("openproxy-test")
}

/// Create a fresh isolated test pool with a custom prefix.
pub fn fresh_pool_with_prefix(prefix: &str) -> (crate::conn::DbPool, PathBuf) {
    let pool = match crate::conn::DbPool::test_pool_with_prefix(prefix) {
        Ok(p) => p,
        Err(e) => panic!("open test db pool: {e}"),
    };
    let path = pool.path().to_path_buf();
    (pool, path)
}

/// Create a fresh isolated test pool and return only the [`crate::conn::DbPool`].
pub fn fresh_pool_only() -> crate::conn::DbPool {
    fresh_pool().0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fresh_pool_creates_isolated_db_with_migrations() {
        let (pool, path) = fresh_pool();
        assert!(path.exists());
        let writer = pool.writer();
        let count: i64 = writer
            .query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0))
            .expect("query schema_migrations");
        assert!(count > 0, "migrations should be applied");
    }

    #[test]
    fn test_fresh_pool_with_prefix() {
        let (_pool, path) = fresh_pool_with_prefix("custom-prefix");
        assert!(path.exists());
        assert!(path.to_string_lossy().contains("custom-prefix"));
    }

    #[test]
    fn test_fresh_pool_only() {
        let pool = fresh_pool_only();
        assert!(pool.path().exists());
    }
}
