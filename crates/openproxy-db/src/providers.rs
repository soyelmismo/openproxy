use openproxy_types::{
    AuthType, CoreError, Provider, ProviderFormat, ProviderId, RateLimitScope, Result,
};
use rusqlite::{Connection, params};

#[derive(Debug, Clone, Copy)]
pub struct NewProvider<'a> {
    pub id: &'a ProviderId,
    pub name: &'a str,
    pub base_url: &'a str,
    pub auth_type: AuthType,
    pub format: ProviderFormat,
    pub extra_headers_json: Option<&'a str>,
    pub auto_activate_keyword: Option<&'a str>,
    pub rate_limit_scope: RateLimitScope,
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

crate::def_table_select!(
    provider_select,
    "providers",
    "id, name, base_url, auth_type, format, extra_headers_json, auto_activate_keyword, active, created_at, use_proxies, current_proxy_id, proxy_rotation_errors, rate_limit_scope, proxy_rotation_mode, favicon_base64"
);

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
        name: 1,
        base_url: 2,
        auth_type: @enum_parse(3, AuthType),
        format: @enum_parse(4, ProviderFormat),
        extra_headers_json: 5,
        auto_activate_keyword: 6,
        active: @bool(7),
        created_at: 8,
        use_proxies: @bool(9),
        current_proxy_id: 10,
        proxy_rotation_errors: 11,
        rate_limit_scope: @enum_parse(12, RateLimitScope),
        proxy_rotation_mode: 13,
        favicon_base64: 14,
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
