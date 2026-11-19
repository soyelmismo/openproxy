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
    openproxy_db::testing::open_in_memory()
}
