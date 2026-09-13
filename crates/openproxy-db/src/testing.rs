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
    let mut conn = Connection::open_in_memory().expect("open in-memory db");
    crate::migrations::run(&mut conn).expect("run migrations");
    conn
}

/// Seed the `providers` row required by the
/// `accounts.provider_id NOT NULL REFERENCES providers(id)` FK
/// constraint declared in `migrations/000019_add_oauth_support.sql`.
/// Tests that `INSERT INTO accounts` MUST call this first.
pub fn seed_antigravity_provider(conn: &Connection) {
    conn.execute(
        "INSERT INTO providers (id, name, base_url, auth_type, format) \
         VALUES ('antigravity', 'Antigravity', 'https://example.com', \
                 'oauth', 'openai')",
        [],
    )
    .expect("seed antigravity provider");
}
