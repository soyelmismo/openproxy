use super::*;
use rusqlite::Connection;
use std::sync::Arc;

mod crud;
mod scrapers;

fn scraped(source: &str, host: &str, port: u16, r#type: &str) -> ScrapedProxy {
    ScrapedProxy {
        source: source.into(),
        host: host.into(),
        port,
        r#type: r#type.into(),
        country_code: None,
        username: None,
        password: None,
        priority: 0,
    }
}

fn scraped_with_country(
    source: &str,
    host: &str,
    port: u16,
    r#type: &str,
    cc: &str,
) -> ScrapedProxy {
    let mut s = scraped(source, host, port, r#type);
    s.country_code = Some(cc.into());
    s
}

fn fresh_pool(name: &str) -> (openproxy_db::testing::TempDir, Arc<openproxy_db::DbPool>) {
    let tmp = openproxy_db::testing::TempDir::new(name).expect("tempdir");
    let db_path = tmp.path().join(format!("{name}.db"));
    let pool = Arc::new(openproxy_db::DbPool::open(&db_path).expect("open db pool"));
    {
        let mut conn = pool.writer();
        openproxy_db::migrations::run(&mut conn).expect("migrations");
    }
    (tmp, pool)
}

fn setup_test_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE free_proxies (
          id TEXT PRIMARY KEY,
          source TEXT NOT NULL,
          host TEXT NOT NULL,
          port INTEGER NOT NULL,
          type TEXT NOT NULL DEFAULT 'http',
          country_code TEXT,
          status TEXT NOT NULL DEFAULT 'unknown',
          latency_ms INTEGER,
          last_validated TEXT,
          created_at TEXT NOT NULL DEFAULT (datetime('now')),
          updated_at TEXT NOT NULL DEFAULT (datetime('now')),
          username TEXT,
          password TEXT,
          priority INTEGER DEFAULT 0,
          UNIQUE(host, port)
        );
        CREATE TABLE proxy_sources (
          id TEXT PRIMARY KEY,
          name TEXT NOT NULL,
          url TEXT NOT NULL,
          priority INTEGER NOT NULL DEFAULT 0,
          active INTEGER NOT NULL DEFAULT 1,
          is_builtin INTEGER NOT NULL DEFAULT 0,
          created_at TEXT NOT NULL DEFAULT (datetime('now')),
          updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE providers (
          id TEXT PRIMARY KEY,
          name TEXT NOT NULL,
          base_url TEXT NOT NULL,
          auth_type TEXT NOT NULL,
          format TEXT NOT NULL,
          extra_headers_json TEXT,
          auto_activate_keyword TEXT,
          use_proxies INTEGER DEFAULT 0,
          current_proxy_id TEXT,
          proxy_rotation_errors TEXT DEFAULT '429,connect_error,timeout',
          rate_limit_scope TEXT DEFAULT 'account',
          proxy_rotation_mode TEXT DEFAULT 'global',
          active INTEGER NOT NULL DEFAULT 1,
          favicon_base64 TEXT,
          created_at TEXT NOT NULL DEFAULT (datetime('now')),
          updated_at TEXT NOT NULL DEFAULT (datetime('now')),
          notif_keyword_only INTEGER NOT NULL DEFAULT 0 CHECK (notif_keyword_only IN (0, 1)),
          CHECK (format IN ('openai', 'anthropic', 'mixed', 'gemini', 'responses'))
        );
        CREATE TABLE accounts (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          provider_id TEXT NOT NULL,
          current_proxy_id TEXT
        );
        CREATE TABLE provider_proxy_cooldowns (
          provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
          proxy_id TEXT NOT NULL REFERENCES free_proxies(id) ON DELETE CASCADE,
          cooldown_until TEXT NOT NULL,
          created_at TEXT NOT NULL DEFAULT (datetime('now')),
          PRIMARY KEY (provider_id, proxy_id)
        );
        CREATE TABLE provider_favicons (
          provider_id TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
          mime TEXT NOT NULL,
          data BLOB NOT NULL,
          updated_at INTEGER NOT NULL
        );",
    )
    .unwrap();
    conn
}
