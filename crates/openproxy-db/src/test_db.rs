//! Test-only helpers for spinning up an isolated SQLite database with
//! the production migrations applied. Not exported to downstream
//! crates — guarded by `#[cfg(test)]`.

use rusqlite::Connection;

/// Open a fresh in-memory SQLite database, run all migrations, and
/// return the connection. Every test gets its own DB; there is no
/// shared state between tests.
pub fn open_in_memory() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    crate::migrations::run(&mut conn).unwrap();
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
    .unwrap();
}