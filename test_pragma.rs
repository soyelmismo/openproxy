use rusqlite::Connection;

fn main() {
    let conn = Connection::open_in_memory().unwrap();
    if let Err(e) = conn.pragma_update(None, "temp_store_directory", "/tmp") {
        println!("Error: {}", e);
    } else {
        println!("Success");
    }
}
