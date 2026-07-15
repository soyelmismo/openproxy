use rusqlite::Connection;
fn main() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute("CREATE TABLE providers (id TEXT, active INTEGER NOT NULL DEFAULT 1)", []).unwrap();
    conn.execute("INSERT INTO providers(id) VALUES ('openrouter')", []).unwrap();
    let mut stmt = conn.prepare("SELECT id FROM providers WHERE active = 1").unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
    let vec: Vec<String> = rows.collect::<Result<_, _>>().unwrap();
    println!("Found: {:?}", vec);
}
