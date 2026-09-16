use openproxy_db::conn::DbPool;
use openproxy_types::ids::ProviderId;
use rusqlite::Connection;
use std::path::PathBuf;

mod crud;
mod oauth;
mod quota;

/// Build a fresh in-process pool: temp dir on disk, migrations applied,
/// a provider seeded so account FK constraints can be satisfied.
pub(crate) fn fresh_pool() -> (DbPool, PathBuf) {
    let pool = DbPool::test_pool_with_prefix("openproxy-accounts-test").expect("open pool");
    let path = pool.path().to_path_buf();
    (pool, path)
}

/// Seed a provider so accounts can be created against it.
pub(crate) fn seed_provider(conn: &Connection, id: &str) {
    crate::providers::create(
        conn,
        crate::providers::NewProvider {
            id: &ProviderId::new(id),
            name: id,
            base_url: "https://example.com",
            auth_type: crate::providers::AuthType::Bearer,
            format: crate::providers::ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: None,
            rate_limit_scope: crate::providers::RateLimitScope::Account,
        },
    )
    .expect("seed provider");
}

pub(crate) fn setup_account() -> (
    DbPool,
    PathBuf,
    openproxy_db::secrets::MasterKey,
    openproxy_types::AccountId,
) {
    let (pool, path) = fresh_pool();
    let mk = openproxy_db::secrets::MasterKey::generate().unwrap();
    let id = {
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");
        crate::accounts::create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create")
    };
    (pool, path, mk, id)
}
