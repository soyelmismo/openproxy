use openproxy_types::{
    AuthType, CoreError, Provider, ProviderFormat, ProviderId, RateLimitScope, Result,
};
use rusqlite::{Connection, params};

#[derive(Debug)]
struct FromStrError(String);

impl std::fmt::Display for FromStrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FromStr error: {}", self.0)
    }
}

impl std::error::Error for FromStrError {}

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

pub fn get(conn: &Connection, id: &ProviderId) -> Result<Option<Provider>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, base_url, auth_type, format, extra_headers_json, auto_activate_keyword, active, created_at, use_proxies, current_proxy_id, proxy_rotation_errors, rate_limit_scope, proxy_rotation_mode \
             FROM providers WHERE id = ?1",
        )
        .map_err(crate::error::map_db_error_ctx("prepare get provider"))?;

    let mut rows = stmt
        .query_map(params![id.as_str()], row_to_provider)
        .map_err(crate::error::map_db_error_ctx("query get provider"))?;

    match rows.next() {
        Some(Ok(p)) => Ok(Some(p)),
        Some(Err(e)) => Err(crate::error::map_db_error_ctx("read provider row")(e)),
        None => Ok(None),
    }
}

pub fn update_current_proxy(
    conn: &Connection,
    id: &ProviderId,
    proxy_id: Option<&str>,
) -> Result<()> {
    crate::db_execute!(
        conn,
        "UPDATE providers SET current_proxy_id = ?1 WHERE id = ?2",
        params![proxy_id, id.as_str()],
        format!("update current proxy for provider {}", id)
    )?;
    Ok(())
}

fn row_to_provider(row: &rusqlite::Row<'_>) -> rusqlite::Result<Provider> {
    let id: String = row.get(0)?;
    let name: String = row.get(1)?;
    let base_url: String = row.get(2)?;
    let auth_type_str: String = row.get(3)?;
    let format_str: String = row.get(4)?;
    let extra_headers_json: Option<String> = row.get(5)?;
    let auto_activate_keyword: Option<String> = row.get(6)?;
    let active_val: i64 = row.get(7)?;
    let created_at: String = row.get(8)?;
    let use_proxies_val: i64 = row.get(9)?;
    let current_proxy_id: Option<String> = row.get(10)?;
    let proxy_rotation_errors: String = row.get(11)?;
    let rate_limit_scope_str: String = row.get(12)?;
    let proxy_rotation_mode: String = row.get(13)?;

    let auth_type = AuthType::parse(&auth_type_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            Box::new(FromStrError(e)),
        )
    })?;
    let format = ProviderFormat::parse(&format_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(FromStrError(e)),
        )
    })?;
    let rate_limit_scope = RateLimitScope::parse(&rate_limit_scope_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            12,
            rusqlite::types::Type::Text,
            Box::new(FromStrError(e)),
        )
    })?;

    Ok(Provider {
        id: ProviderId::new(id),
        name,
        base_url,
        auth_type,
        format,
        extra_headers_json,
        auto_activate_keyword,
        active: active_val != 0,
        created_at,
        use_proxies: use_proxies_val != 0,
        current_proxy_id,
        proxy_rotation_errors,
        rate_limit_scope,
        proxy_rotation_mode,
    })
}

impl crate::crud::FromRow for Provider {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        row_to_provider(row)
    }
}
