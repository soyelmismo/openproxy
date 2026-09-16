use openproxy_db::conn::DbPool;
use std::path::PathBuf;

/// Build a fresh in-process pool: temp dir on disk, migrations applied.
pub(crate) fn fresh_pool() -> (DbPool, PathBuf) {
    let pool = DbPool::test_pool_with_prefix("openproxy-admin-test").expect("open pool");
    let path = pool.path().to_path_buf();
    (pool, path)
}

mod accounts;
mod combos;
mod models;
mod providers;
