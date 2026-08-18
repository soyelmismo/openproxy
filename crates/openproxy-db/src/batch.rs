//! Batch operations and helpers for SQLite chunking and parameter limits.

use rusqlite::Connection;

/// Maximum variable number allowed by SQLite in standard configurations.
pub const SQLITE_MAX_VARIABLE_NUMBER: usize = 999;

/// Default chunk size for batch operations.
pub const DEFAULT_CHUNK_SIZE: usize = 900;

/// Returns comma-separated `?` placeholders for an `IN (...)` clause: `"?, ?, ?"`.
pub fn in_placeholders(count: usize) -> String {
    if count == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(count * 3 - 2);
    for i in 0..count {
        if i > 0 {
            out.push_str(", ");
        }
        out.push('?');
    }
    out
}

/// Returns comma-separated tuple placeholders for a `VALUES` clause:
/// `values_placeholders(2, 3)` -> `"(?, ?, ?), (?, ?, ?)"`.
pub fn values_placeholders(num_rows: usize, num_cols: usize) -> String {
    if num_rows == 0 || num_cols == 0 {
        return String::new();
    }
    let row_len = 3 * num_cols;
    let mut out = String::with_capacity(num_rows * (row_len + 2));
    for r in 0..num_rows {
        if r > 0 {
            out.push_str(", ");
        }
        out.push('(');
        for c in 0..num_cols {
            if c > 0 {
                out.push_str(", ");
            }
            out.push('?');
        }
        out.push(')');
    }
    out
}

/// Repeats a custom row template for `num_rows`:
/// e.g. `repeat_row_template("(?1, ?2, 'literal')", 3)` -> `"(?1, ?2, 'literal'), (?1, ?2, 'literal'), (?1, ?2, 'literal')"`
pub fn repeat_row_template(template: &str, num_rows: usize) -> String {
    if num_rows == 0 || template.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(num_rows * (template.len() + 2));
    for r in 0..num_rows {
        if r > 0 {
            out.push_str(", ");
        }
        out.push_str(template);
    }
    out
}

/// Builds a complete batch `INSERT` query.
///
/// Example:
/// `build_insert_sql("INSERT INTO", "users", &["name", "email"], 2, None)`
/// -> `"INSERT INTO users (name, email) VALUES (?, ?), (?, ?)"`
///
/// Suffix can include `ON CONFLICT ...` or `RETURNING ...`.
pub fn build_insert_sql(
    prefix: &str,
    table: &str,
    columns: &[&str],
    num_rows: usize,
    suffix: Option<&str>,
) -> String {
    let cols_str = columns.join(", ");
    let vals = values_placeholders(num_rows, columns.len());
    let mut sql = String::with_capacity(
        prefix.len()
            + 1
            + table.len()
            + 2
            + cols_str.len()
            + 9
            + vals.len()
            + suffix.map_or(0, |s| s.len() + 1),
    );
    sql.push_str(prefix);
    sql.push(' ');
    sql.push_str(table);
    sql.push_str(" (");
    sql.push_str(&cols_str);
    sql.push_str(") VALUES ");
    sql.push_str(&vals);
    if let Some(suf) = suffix {
        if !suf.starts_with(' ') {
            sql.push(' ');
        }
        sql.push_str(suf);
    }
    sql
}

/// Helper to query rows in chunks when filtering by `IN ({})`.
///
/// `sql_template` must contain `{}` which will be replaced by `?, ?, ...` placeholders.
/// Automatically clamps `chunk_size` so `chunk.len() <= SQLITE_MAX_VARIABLE_NUMBER`.
pub fn query_in_chunks<T, R, F>(
    conn: &Connection,
    sql_template: &str,
    items: &[T],
    chunk_size: usize,
    map_row: F,
) -> rusqlite::Result<Vec<R>>
where
    T: rusqlite::ToSql,
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<R>,
{
    query_in_chunks_with_params(conn, sql_template, &[], items, chunk_size, map_row)
}

/// Helper to query rows in chunks with additional prefix parameters.
///
/// `sql_template` must contain `{}` which will be replaced by `?, ?, ...` placeholders.
pub fn query_in_chunks_with_params<T, R, F>(
    conn: &Connection,
    sql_template: &str,
    prefix_params: &[&dyn rusqlite::ToSql],
    items: &[T],
    chunk_size: usize,
    mut map_row: F,
) -> rusqlite::Result<Vec<R>>
where
    T: rusqlite::ToSql,
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<R>,
{
    if items.is_empty() {
        return Ok(Vec::new());
    }

    let max_chunk = SQLITE_MAX_VARIABLE_NUMBER
        .saturating_sub(prefix_params.len())
        .max(1);
    let effective_chunk_size = chunk_size.clamp(1, max_chunk);
    let mut results = Vec::with_capacity(items.len());

    for chunk in items.chunks(effective_chunk_size) {
        let placeholders = in_placeholders(chunk.len());
        let sql = sql_template.replace("{}", &placeholders);
        let mut stmt = conn.prepare_cached(&sql)?;

        let mut params: Vec<&dyn rusqlite::ToSql> =
            Vec::with_capacity(prefix_params.len() + chunk.len());
        params.extend_from_slice(prefix_params);
        for item in chunk {
            params.push(item as &dyn rusqlite::ToSql);
        }

        let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
        while let Some(row) = rows.next()? {
            results.push(map_row(row)?);
        }
    }

    Ok(results)
}

/// Helper to query rows in chunks using an extraction closure to convert items to `ToSql`.
pub fn query_in_chunks_by<'a, T, V, R, E, F>(
    conn: &Connection,
    sql_template: &str,
    items: &'a [T],
    chunk_size: usize,
    extract: E,
    map_row: F,
) -> rusqlite::Result<Vec<R>>
where
    V: rusqlite::ToSql,
    E: Fn(&'a T) -> V,
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<R>,
{
    query_in_chunks_by_with_params(conn, sql_template, &[], items, chunk_size, extract, map_row)
}

/// Helper to query rows in chunks with prefix parameters and an extraction closure.
pub fn query_in_chunks_by_with_params<'a, T, V, R, E, F>(
    conn: &Connection,
    sql_template: &str,
    prefix_params: &[&dyn rusqlite::ToSql],
    items: &'a [T],
    chunk_size: usize,
    extract: E,
    map_row: F,
) -> rusqlite::Result<Vec<R>>
where
    V: rusqlite::ToSql,
    E: Fn(&'a T) -> V,
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<R>,
{
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let extracted: Vec<V> = items.iter().map(extract).collect();
    query_in_chunks_with_params(
        conn,
        sql_template,
        prefix_params,
        &extracted,
        chunk_size,
        map_row,
    )
}

/// Performs chunked batch inserts, splitting `items` so that `chunk.len() * columns.len() <= SQLITE_MAX_VARIABLE_NUMBER`.
///
/// Calls `row_fn` for each item to push values into the parameter list.
pub fn batch_insert<T, F>(
    conn: &Connection,
    prefix: &str,
    table: &str,
    columns: &[&str],
    items: &[T],
    suffix: Option<&str>,
    mut row_fn: F,
) -> rusqlite::Result<usize>
where
    F: FnMut(&T, &mut Vec<rusqlite::types::Value>),
{
    if items.is_empty() || columns.is_empty() {
        return Ok(0);
    }

    let num_cols = columns.len();
    let max_rows_per_chunk = (SQLITE_MAX_VARIABLE_NUMBER / num_cols).clamp(1, DEFAULT_CHUNK_SIZE);
    let mut total_affected = 0;

    for chunk in items.chunks(max_rows_per_chunk) {
        let sql = build_insert_sql(prefix, table, columns, chunk.len(), suffix);
        let mut params = Vec::with_capacity(chunk.len() * num_cols);
        for item in chunk {
            row_fn(item, &mut params);
        }
        let mut stmt = conn.prepare_cached(&sql)?;
        let count = stmt.execute(rusqlite::params_from_iter(params))?;
        total_affected += count;
    }

    Ok(total_affected)
}

/// Macro for convenient batch inserts into SQLite.
#[macro_export]
macro_rules! sqlite_batch_insert {
    ($conn:expr, $table:expr, [$($col:expr),+ $(,)?], $items:expr, |$item:pat, $params:ident| $body:block) => {
        $crate::batch::batch_insert(
            $conn,
            "INSERT INTO",
            $table,
            &[$($col),+],
            $items,
            None,
            |$item, $params| $body,
        )
    };
    ($conn:expr, prefix: $prefix:expr, table: $table:expr, cols: [$($col:expr),+ $(,)?], items: $items:expr, suffix: $suffix:expr, |$item:pat, $params:ident| $body:block) => {
        $crate::batch::batch_insert(
            $conn,
            $prefix,
            $table,
            &[$($col),+],
            $items,
            $suffix,
            |$item, $params| $body,
        )
    };
    ($conn:expr, $prefix:expr, $table:expr, [$($col:expr),+ $(,)?], $items:expr, |$item:pat, $params:ident| $body:block) => {
        $crate::batch::batch_insert(
            $conn,
            $prefix,
            $table,
            &[$($col),+],
            $items,
            None,
            |$item, $params| $body,
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_in_placeholders() {
        assert_eq!(in_placeholders(0), "");
        assert_eq!(in_placeholders(1), "?");
        assert_eq!(in_placeholders(3), "?, ?, ?");
    }

    #[test]
    fn test_values_placeholders() {
        assert_eq!(values_placeholders(0, 3), "");
        assert_eq!(values_placeholders(3, 0), "");
        assert_eq!(values_placeholders(1, 2), "(?, ?)");
        assert_eq!(values_placeholders(2, 3), "(?, ?, ?), (?, ?, ?)");
    }

    #[test]
    fn test_repeat_row_template() {
        assert_eq!(repeat_row_template("(?1, ?2)", 0), "");
        assert_eq!(repeat_row_template("(?1, ?2)", 1), "(?1, ?2)");
        assert_eq!(repeat_row_template("(?1, ?2)", 2), "(?1, ?2), (?1, ?2)");
    }

    #[test]
    fn test_build_insert_sql() {
        assert_eq!(
            build_insert_sql("INSERT INTO", "users", &["name", "age"], 2, None),
            "INSERT INTO users (name, age) VALUES (?, ?), (?, ?)"
        );
        assert_eq!(
            build_insert_sql(
                "INSERT OR IGNORE INTO",
                "users",
                &["name"],
                1,
                Some("RETURNING id")
            ),
            "INSERT OR IGNORE INTO users (name) VALUES (?) RETURNING id"
        );
    }

    #[test]
    fn test_query_in_chunks() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)", [])
            .unwrap();

        for i in 1..=10 {
            conn.execute(
                "INSERT INTO items (id, name) VALUES (?, ?)",
                rusqlite::params![i, format!("item_{}", i)],
            )
            .unwrap();
        }

        let ids = vec![2, 4, 6, 8, 10];
        let names: Vec<String> = query_in_chunks(
            &conn,
            "SELECT name FROM items WHERE id IN ({}) ORDER BY id",
            &ids,
            2,
            |row| row.get(0),
        )
        .unwrap();

        assert_eq!(
            names,
            vec!["item_2", "item_4", "item_6", "item_8", "item_10"]
        );
    }

    #[test]
    fn test_batch_insert() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE items (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, val INTEGER)",
            [],
        )
        .unwrap();

        let data = vec![("a", 10), ("b", 20), ("c", 30), ("d", 40)];
        let count = batch_insert(
            &conn,
            "INSERT INTO",
            "items",
            &["name", "val"],
            &data,
            None,
            |&(name, val), params| {
                params.push(name.to_string().into());
                params.push(val.into());
            },
        )
        .unwrap();

        assert_eq!(count, 4);

        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 4);
    }

    #[test]
    fn test_sqlite_batch_insert_macro() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE records (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT UNIQUE, score INTEGER)",
            [],
        )
        .unwrap();

        // 1. Basic form
        let data1 = vec![("user1", 100), ("user2", 200)];
        let count1 = sqlite_batch_insert!(
            &conn,
            "records",
            ["name", "score"],
            &data1,
            |&(n, s), params| {
                params.push(n.to_string().into());
                params.push(s.into());
            }
        )
        .unwrap();
        assert_eq!(count1, 2);

        // 2. Custom prefix form
        let data2 = vec![("user2", 250), ("user3", 300)];
        let count2 = sqlite_batch_insert!(
            &conn,
            "INSERT OR IGNORE INTO",
            "records",
            ["name", "score"],
            &data2,
            |&(n, s), params| {
                params.push(n.to_string().into());
                params.push(s.into());
            }
        )
        .unwrap();
        // user2 is ignored, user3 is inserted
        assert_eq!(count2, 1);

        // 3. Named fields form with suffix
        let data3 = vec![("user4", 400)];
        let count3 = sqlite_batch_insert!(
            &conn,
            prefix: "INSERT INTO",
            table: "records",
            cols: ["name", "score"],
            items: &data3,
            suffix: Some("ON CONFLICT(name) DO UPDATE SET score = excluded.score"),
            |&(n, s), params| {
                params.push(n.to_string().into());
                params.push(s.into());
            }
        )
        .unwrap();
        assert_eq!(count3, 1);

        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 4);
    }
}
