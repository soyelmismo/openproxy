use super::*;
use rusqlite::Connection;

#[derive(Debug, PartialEq, Eq)]
enum TestRole {
    Admin,
    User,
}

impl std::str::FromStr for TestRole {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "admin" => Ok(Self::Admin),
            "user" => Ok(Self::User),
            o => Err(o.to_string()),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum TestMode {
    Fast,
    Safe,
}
impl TestMode {
    fn from_db(s: Option<&str>) -> Self {
        if s == Some("fast") {
            Self::Fast
        } else {
            Self::Safe
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct CustomRowId(i64);
#[derive(Debug, PartialEq, Eq)]
struct CustomStrId(String);

#[derive(Debug, PartialEq, Eq)]
struct TestItem {
    id: CustomRowId,
    parent_id: Option<CustomRowId>,
    name_id: CustomStrId,
    is_active: bool,
    opt_bool: Option<bool>,
    u8_val: u8,
    port: u16,
    opt_u16_val: Option<u16>,
    u32_val: u32,
    opt_u32_val: Option<u32>,
    u64_val: u64,
    opt_u64_val: Option<u64>,
    role: TestRole,
    opt_role: Option<TestRole>,
    mode: TestMode,
    metadata: Option<serde_json::Value>,
    tag: String,
    description: String,
    custom_computed: String,
    default_payload: serde_json::Value,
}

#[test]
fn test_map_row_struct_and_qualifiers() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, parent_id INTEGER, name_id TEXT NOT NULL, is_active INTEGER NOT NULL, opt_bool INTEGER, u8_val INTEGER NOT NULL, port INTEGER NOT NULL, opt_u16_val INTEGER, u32_val INTEGER NOT NULL, opt_u32_val INTEGER, u64_val INTEGER NOT NULL, opt_u64_val INTEGER, role TEXT NOT NULL, opt_role TEXT, mode TEXT, metadata TEXT, tag TEXT, description TEXT NOT NULL, default_payload TEXT)", []).unwrap();
    conn.execute("INSERT INTO items VALUES (1, 42, 'str-id-1', 1, 0, 8, 8080, 9090, 3000, 4000, 50000, 60000, 'admin', 'user', 'fast', '{\"k\":\"v\"}', NULL, 'a test item', NULL)", []).unwrap();

    let computed_val = "computed-value".to_string();
    let item: TestItem = conn.query_row("SELECT * FROM items WHERE id = 1", [], |row| {
        map_row_struct!(row, TestItem {
            id: @id(0, CustomRowId), parent_id: @opt_id(1, CustomRowId), name_id: @id_str(2, CustomStrId),
            is_active: @bool(3), opt_bool: @opt_bool(4), u8_val: @u8(5), port: @u16(6), opt_u16_val: @opt_u16(7),
            u32_val: @u32(8), opt_u32_val: @opt_u32(9), u64_val: @u64(10), opt_u64_val: @opt_u64(11),
            role: @enum_parse(12, TestRole), opt_role: @opt_enum_parse(13, TestRole), mode: @from_db(14, TestMode),
            metadata: @json(15), tag: @opt_default(16, "default_tag".to_string()), description: 17,
            custom_computed: @expr(computed_val.clone()), default_payload: @json_or_default(18),
        })
    }).unwrap();

    assert_eq!(item.id, CustomRowId(1));
    assert_eq!(item.parent_id, Some(CustomRowId(42)));
    assert_eq!(item.name_id, CustomStrId("str-id-1".into()));
    assert!(item.is_active);
    assert_eq!(item.opt_bool, Some(false));
    assert_eq!(item.u8_val, 8);
    assert_eq!(item.port, 8080);
    assert_eq!(item.opt_u16_val, Some(9090));
    assert_eq!(item.u32_val, 3000);
    assert_eq!(item.opt_u32_val, Some(4000));
    assert_eq!(item.u64_val, 50000);
    assert_eq!(item.opt_u64_val, Some(60000));
    assert_eq!(item.role, TestRole::Admin);
    assert_eq!(item.opt_role, Some(TestRole::User));
    assert_eq!(item.mode, TestMode::Fast);
    assert_eq!(item.metadata.as_ref().unwrap()["k"], "v");
    assert_eq!(item.tag, "default_tag");
    assert_eq!(item.description, "a test item");
    assert_eq!(item.custom_computed, "computed-value");
    assert_eq!(item.default_payload, serde_json::Value::Null);
}

#[derive(Debug, PartialEq, Eq)]
struct SimpleUser {
    id: i64,
    name: String,
}
impl FromRow for SimpleUser {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
        })
    }
}
def_table_select!(user_select, "users", "id, name");

#[test]
fn test_crud_macros() {
    let conn = Connection::open_in_memory().unwrap();
    db_execute!(
        &conn,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
        [],
        "create"
    )
    .unwrap();
    assert_eq!(
        db_execute!(
            &conn,
            "INSERT INTO users (id, name) VALUES (?, ?)",
            rusqlite::params![1, "Alice"],
            "insert"
        )
        .unwrap(),
        1
    );
    db_execute!(
        &conn,
        "INSERT INTO users (id, name) VALUES (?, ?)",
        rusqlite::params![2, "Bob"],
        "insert"
    )
    .unwrap();

    let alice: Option<SimpleUser> = db_query_one!(
        &conn,
        user_select!("WHERE id = ?1"),
        rusqlite::params![1],
        "q"
    )
    .unwrap();
    assert_eq!(
        alice,
        Some(SimpleUser {
            id: 1,
            name: "Alice".into()
        })
    );
    assert_eq!(
        db_query_one!(
            &conn,
            user_select!("WHERE id = ?1"),
            rusqlite::params![999],
            "none"
        )
        .unwrap(),
        None::<SimpleUser>
    );

    let bob_name: Option<String> = db_query_one!(
        &conn,
        "SELECT name FROM users WHERE id = ?",
        rusqlite::params![2],
        |r| r.get(0),
        "q"
    )
    .unwrap();
    assert_eq!(bob_name, Some("Bob".into()));

    let all: Vec<SimpleUser> =
        db_query_all!(&conn, user_select!("ORDER BY id ASC"), [], "q").unwrap();
    assert_eq!(
        all,
        vec![
            SimpleUser {
                id: 1,
                name: "Alice".into()
            },
            SimpleUser {
                id: 2,
                name: "Bob".into()
            }
        ]
    );

    let names: Vec<String> = db_query_all!(
        &conn,
        "SELECT name FROM users ORDER BY id DESC",
        [],
        |r| r.get(0),
        "q"
    )
    .unwrap();
    assert_eq!(names, vec!["Bob", "Alice"]);

    assert_eq!(
        db_update_field!(&conn, "users", name = "Alice Updated", WHERE id = 1, "u").unwrap(),
        1
    );
    assert_eq!(
        db_query_one!(
            &conn,
            user_select!("WHERE id = ?1"),
            rusqlite::params![1],
            "q"
        )
        .unwrap(),
        Some(SimpleUser {
            id: 1,
            name: "Alice Updated".into()
        })
    );

    assert!(db_exists!(&conn, "users", WHERE id = 1, "e").unwrap());
    assert!(!db_exists!(&conn, "users", WHERE id = 999, "e").unwrap());
    assert!(db_exists!(&conn, "users", WHERE name = "Bob", "e").unwrap());

    let (id, name): (i64, String) = conn
        .query_row(
            "SELECT id, name FROM users WHERE id = 2",
            [],
            |r| map_row_tuple!(r => (0, 1)),
        )
        .unwrap();
    assert_eq!((id, name), (2, "Bob".to_string()));
}
