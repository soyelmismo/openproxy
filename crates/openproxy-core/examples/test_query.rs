#![allow(clippy::unwrap_used)]

use openproxy_db::conn::DbPool;
fn main() {
    let db_path = std::env::var("OPENPROXY_DATABASE_PATH")
        .unwrap_or_else(|_| "./target/test-data.db".to_string());
    let pool = DbPool::open(std::path::Path::new(&db_path)).unwrap();
    let conn = pool.open_connection().unwrap();
    let mut stmt = conn.prepare("SELECT id, host, port, type, username, password FROM free_proxies WHERE source = 'user@example.com' LIMIT 1").unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })
        .unwrap();

    for row in rows {
        println!("{row:?}");
    }
}
