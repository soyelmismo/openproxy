use openproxy_types::{
    AuthType, CoreError, Provider, ProviderFormat, ProviderId, RateLimitScope, Result,
};
use rusqlite::{Connection, params};

pub use openproxy_types::providers::{NewProvider, VIRTUAL_COMBO_PROVIDER_ID};

crate::def_table_select!(
    provider_select,
    "providers",
    "id, name, base_url, auth_type, format, extra_headers_json, auto_activate_keyword, active, created_at, use_proxies, current_proxy_id, proxy_rotation_errors, rate_limit_scope, proxy_rotation_mode, EXISTS(SELECT 1 FROM provider_favicons WHERE provider_id = providers.id), notif_keyword_only"
);

define_column_updaters! {
    table: "providers",
    id_type: &ProviderId,
    id_field: |id| id.as_str(),
    pub fn set_active(active: bool => i64::from(active)) => "active";
}

pub fn list(conn: &Connection) -> Result<Vec<Provider>> {
    crate::db_query_all!(
        conn,
        provider_select!("WHERE id != ?1 ORDER BY id"),
        params![VIRTUAL_COMBO_PROVIDER_ID],
        "list providers"
    )
}

pub fn list_active(conn: &Connection) -> Result<Vec<Provider>> {
    crate::db_query_all!(
        conn,
        provider_select!("WHERE active = 1 AND id != ?1 ORDER BY id"),
        params![VIRTUAL_COMBO_PROVIDER_ID],
        "list active providers"
    )
}

pub fn delete(conn: &Connection, id: &ProviderId) -> Result<()> {
    crate::db_execute!(
        conn,
        "DELETE FROM providers WHERE id = ?1",
        params![id.as_str()],
        format!("delete provider {id}")
    )?;
    Ok(())
}

#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateProviderParams<'a> {
    pub name: Option<&'a str>,
    pub base_url: Option<&'a str>,
    pub extra_headers_json: Option<Option<&'a str>>,
    pub auto_activate_keyword: Option<Option<&'a str>>,
    pub use_proxies: Option<bool>,
    pub proxy_rotation_errors: Option<&'a str>,
    pub proxy_rotation_mode: Option<&'a str>,
    pub rate_limit_scope: Option<RateLimitScope>,
    pub notif_keyword_only: Option<Option<bool>>,
}

fn build_provider_update_clauses(
    params: &UpdateProviderParams<'_>,
    sets: &mut Vec<&'static str>,
    bound_values: &mut Vec<Box<dyn rusqlite::ToSql>>,
) {
    if let Some(v) = params.name {
        sets.push("name = ?");
        bound_values.push(Box::new(v.to_string()));
    }
    if let Some(v) = params.base_url {
        sets.push("base_url = ?");
        bound_values.push(Box::new(v.to_string()));
    }
    if let Some(v) = params.extra_headers_json {
        sets.push("extra_headers_json = ?");
        bound_values.push(Box::new(v.map(std::string::ToString::to_string)));
    }
    if let Some(v) = params.auto_activate_keyword {
        sets.push("auto_activate_keyword = ?");
        bound_values.push(Box::new(v.map(std::string::ToString::to_string)));
    }
    if let Some(v) = params.use_proxies {
        sets.push("use_proxies = ?");
        bound_values.push(Box::new(i64::from(v)));
    }
    if let Some(v) = params.proxy_rotation_errors {
        sets.push("proxy_rotation_errors = ?");
        bound_values.push(Box::new(v.to_string()));
    }
    if let Some(v) = params.proxy_rotation_mode {
        sets.push("proxy_rotation_mode = ?");
        bound_values.push(Box::new(v.to_string()));
    }
    if let Some(v) = params.rate_limit_scope {
        sets.push("rate_limit_scope = ?");
        bound_values.push(Box::new(v.as_str().to_string()));
    }
    if let Some(v) = params.notif_keyword_only {
        sets.push("notif_keyword_only = ?");
        bound_values.push(Box::new(i64::from(v.unwrap_or(false))));
    }
}

pub fn update(conn: &Connection, id: &ProviderId, params: UpdateProviderParams<'_>) -> Result<()> {
    let mut sets = Vec::new();
    let mut bound_values = Vec::new();
    build_provider_update_clauses(&params, &mut sets, &mut bound_values);

    if sets.is_empty() {
        if get(conn, id)?.is_none() {
            return Err(CoreError::ProviderNotFound(id.to_string()));
        }
        return Ok(());
    }

    let mut sql = String::with_capacity(40 + sets.len() * 20);
    sql.push_str("UPDATE providers SET ");
    for (i, set) in sets.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push_str(set);
    }
    sql.push_str(" WHERE id = ?");
    let id_owned = id.as_str().to_string();
    let mut bound: Vec<&dyn rusqlite::ToSql> = bound_values.iter().map(|b| b.as_ref()).collect();
    bound.push(&id_owned);

    let affected = conn
        .execute(&sql, rusqlite::params_from_iter(bound.iter().copied()))
        .map_err(crate::error::map_db_error_ctx(format!(
            "update provider {id}"
        )))?;

    if affected == 0 {
        return Err(CoreError::ProviderNotFound(id.to_string()));
    }
    Ok(())
}

pub fn create(conn: &Connection, new: NewProvider<'_>) -> Result<()> {
    let NewProvider {
        id,
        name,
        base_url,
        auth_type,
        format,
        extra_headers_json,
        auto_activate_keyword,
        rate_limit_scope,
    } = new;
    let result = conn.execute(
        // `notif_keyword_only` is intentionally absent: the column is
        // `NOT NULL DEFAULT 0` (migration 000074) so a freshly created
        // provider starts with the toggle off.
        "INSERT INTO providers(id, name, base_url, auth_type, format, extra_headers_json, auto_activate_keyword, rate_limit_scope) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id.as_str(),
            name,
            base_url,
            auth_type.as_str(),
            format.as_str(),
            extra_headers_json,
            auto_activate_keyword,
            rate_limit_scope.as_str(),
        ],
    );

    match result {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Err(CoreError::Validation("provider id already exists".into()))
        }
        Err(e) => Err(crate::error::map_db_error_ctx("create provider")(e)),
    }
}

pub fn get(conn: &Connection, id: &ProviderId) -> Result<Option<Provider>> {
    crate::db_query_one!(
        conn,
        provider_select!("WHERE id = ?1"),
        params![id.as_str()],
        format!("get provider {id}")
    )
}

pub fn update_current_proxy(
    conn: &Connection,
    id: &ProviderId,
    proxy_id: Option<&str>,
) -> Result<()> {
    crate::db_update_field!(
        conn,
        "providers",
        current_proxy_id = proxy_id,
        WHERE id = id.as_str(),
        format!("update current proxy for provider {id}")
    )?;
    Ok(())
}

fn row_to_provider(row: &rusqlite::Row<'_>) -> rusqlite::Result<Provider> {
    crate::map_row_struct!(row, Provider {
        id: @id_str(0, ProviderId),
        name: @box_str(1),
        base_url: @box_str(2),
        auth_type: @enum_parse(3, AuthType),
        format: @enum_parse(4, ProviderFormat),
        extra_headers_json: @opt_box_str(5),
        auto_activate_keyword: @opt_box_str(6),
        active: @bool(7),
        created_at: @box_str(8),
        use_proxies: @bool(9),
        current_proxy_id: @opt_box_str(10),
        proxy_rotation_errors: @box_str(11),
        rate_limit_scope: @enum_parse(12, RateLimitScope),
        proxy_rotation_mode: @box_str(13),
        has_favicon: @bool(14),
        notif_keyword_only: @bool(15),
    })
}

impl crate::crud::FromRow for Provider {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        row_to_provider(row)
    }
}

pub fn get_auth_types(
    conn: &Connection,
    provider_ids: &[ProviderId],
) -> Result<std::collections::HashMap<String, String>> {
    if provider_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let rows: Vec<(String, String)> = crate::batch::query_in_chunks_by(
        conn,
        "SELECT id, auth_type FROM providers WHERE id IN ({})",
        provider_ids,
        crate::batch::DEFAULT_CHUNK_SIZE,
        |id| id.as_str(),
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .map_err(crate::error::map_db_error_ctx(
        "batch query providers auth_type",
    ))?;

    Ok(rows.into_iter().collect())
}

pub fn get_provider_favicon(
    conn: &Connection,
    provider_id: &str,
) -> Result<Option<(String, Vec<u8>)>> {
    let res = conn.query_row(
        "SELECT mime, data FROM provider_favicons WHERE provider_id = ?1",
        params![provider_id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
    );
    match res {
        Ok(pair) => Ok(Some(pair)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(crate::error::map_db_error(e)),
    }
}

pub fn set_provider_favicon(
    conn: &Connection,
    provider_id: &str,
    mime: &str,
    data: &[u8],
) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    crate::db_execute!(
        conn,
        "INSERT INTO provider_favicons (provider_id, mime, data, updated_at) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(provider_id) DO UPDATE SET \
         mime = excluded.mime, data = excluded.data, updated_at = excluded.updated_at",
        params![provider_id, mime, data, now],
        format!("set favicon for provider {provider_id}")
    )?;
    Ok(())
}

pub fn delete_provider_favicon(conn: &Connection, provider_id: &str) -> Result<()> {
    crate::db_execute!(
        conn,
        "DELETE FROM provider_favicons WHERE provider_id = ?1",
        params![provider_id],
        format!("delete favicon for provider {provider_id}")
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open memory db");
        crate::migrations::run(&mut conn).expect("run migrations");
        conn
    }

    #[test]
    fn test_provider_favicon_crud_and_has_favicon() {
        let conn = test_conn();
        let pid = ProviderId::new("test-fav-prov");
        create(
            &conn,
            NewProvider {
                id: &pid,
                name: "Favicon Test Provider",
                base_url: "https://fav.example.com",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: RateLimitScope::Account,
            },
        )
        .expect("create provider");

        // Before favicon: has_favicon is false, get returns None
        let p = get(&conn, &pid).expect("get").expect("found");
        assert!(!p.has_favicon, "initially false");
        assert_eq!(
            get_provider_favicon(&conn, pid.as_str()).expect("get_fav"),
            None
        );

        // Store favicon
        let fake_icon = b"\x89PNG\r\n\x1a\nfakeimagebytes";
        set_provider_favicon(&conn, pid.as_str(), "image/png", fake_icon).expect("set_fav");

        // After favicon: has_favicon is true, get returns data
        let p2 = get(&conn, &pid).expect("get").expect("found");
        assert!(p2.has_favicon, "has_favicon must be true");
        let (mime, data) = get_provider_favicon(&conn, pid.as_str())
            .expect("get_fav")
            .expect("found favicon");
        assert_eq!(mime, "image/png");
        assert_eq!(data, fake_icon);

        // Update favicon
        let fake_ico = b"\x00\x00\x01\x00newbytes";
        set_provider_favicon(&conn, pid.as_str(), "image/x-icon", fake_ico).expect("update_fav");
        let (mime2, data2) = get_provider_favicon(&conn, pid.as_str())
            .expect("get_fav")
            .expect("found favicon");
        assert_eq!(mime2, "image/x-icon");
        assert_eq!(data2, fake_ico);

        // Delete favicon
        delete_provider_favicon(&conn, pid.as_str()).expect("del_fav");
        let p3 = get(&conn, &pid).expect("get").expect("found");
        assert!(!p3.has_favicon, "has_favicon must be false after deletion");
        assert_eq!(
            get_provider_favicon(&conn, pid.as_str()).expect("get_fav"),
            None
        );
    }
}
