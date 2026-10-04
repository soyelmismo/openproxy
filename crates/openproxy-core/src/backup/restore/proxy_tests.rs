use super::existing_proxy_id;
use rusqlite::Connection;

#[test]
fn test_existing_proxy_id_cases() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute("CREATE TABLE free_proxies (id TEXT PRIMARY KEY)", [])
        .unwrap();
    conn.execute("INSERT INTO free_proxies (id) VALUES ('proxy-1'), ('')", [])
        .unwrap();

    let cases = [
        (None, None),
        (Some("missing"), None),
        (Some("proxy-1"), Some("proxy-1")),
        (Some(""), Some("")),
    ];
    for (input, expected) in cases {
        let result = existing_proxy_id(&conn, input);
        assert_eq!(result, expected);
        if let (Some(in_s), Some(out_s)) = (input, result) {
            assert!(std::ptr::eq(in_s, out_s));
        }
    }
}

#[test]
fn test_existing_proxy_id_missing_table() {
    let conn = Connection::open_in_memory().unwrap();
    assert_eq!(existing_proxy_id(&conn, Some("proxy-1")), None);
    assert_eq!(existing_proxy_id(&conn, None), None);
}

#[test]
fn test_existing_proxy_id_tx_visibility_and_rollback() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute("CREATE TABLE free_proxies (id TEXT PRIMARY KEY)", [])
        .unwrap();
    assert_eq!(existing_proxy_id(&conn, Some("tx_proxy")), None);

    let tx = conn.transaction().unwrap();
    tx.execute("INSERT INTO free_proxies (id) VALUES ('tx_proxy')", [])
        .unwrap();
    assert_eq!(existing_proxy_id(&tx, Some("tx_proxy")), Some("tx_proxy"));

    tx.rollback().unwrap();
    assert_eq!(existing_proxy_id(&conn, Some("tx_proxy")), None);
}
