use openproxy_db::conn::DbPool;
fn main() {
    let pool = DbPool::open(std::path::Path::new("/root/.openproxy/data.db")).unwrap();
    let conn = pool.open_connection().unwrap();
    let mut stmt = conn.prepare("SELECT id, host, port, type, username, password FROM free_proxies WHERE source = 'tboutme3@gmail.com' LIMIT 1").unwrap();
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
        println!("{:?}", row);
    }
}
