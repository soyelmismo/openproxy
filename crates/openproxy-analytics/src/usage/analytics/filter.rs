//! Filter and WHERE-clause builder for usage queries.

use openproxy_types::ids::{AccountId, ApiKeyId, ComboId, ProviderId};
use rusqlite::{Row, ToSql};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageFilter {
    /// Inclusive lower bound on `created_at` (ISO-8601).
    pub from: Option<String>,
    /// Exclusive upper bound on `created_at` (ISO-8601).
    pub to: Option<String>,
    pub provider_id: Option<ProviderId>,
    /// Matches `usage.upstream_model_id`.
    pub model_id: Option<String>,
    pub account_id: Option<AccountId>,
    pub combo_id: Option<ComboId>,
    /// Restrict to rows produced under a specific API key.
    pub api_key_id: Option<ApiKeyId>,
}

impl UsageFilter {
    /// Returns `true` if no filter is set.
    pub fn is_empty(&self) -> bool {
        let filters = [
            self.from.is_some(),
            self.to.is_some(),
            self.provider_id.is_some(),
            self.model_id.is_some(),
            self.account_id.is_some(),
            self.combo_id.is_some(),
            self.api_key_id.is_some(),
        ];
        !filters.into_iter().any(|active| active)
    }
}

pub(crate) struct BuiltWhere {
    pub(crate) sql: String,
    pub(crate) params: Vec<Box<dyn ToSql>>,
}

fn push_date_filters(
    from: Option<&str>,
    to: Option<&str>,
    clauses: &mut Vec<&'static str>,
    params: &mut Vec<Box<dyn ToSql>>,
) {
    if let Some(from) = from {
        // Column bare so SQLite can use `idx_usage_created(_provider)`; the
        // bound is normalized on the parameter side because callers pass
        // RFC-3339 strings while rows store SQLite's `YYYY-MM-DD HH:MM:SS`.
        clauses.push("created_at >= datetime(?)");
        params.push(Box::new(from.to_owned()));
    }
    if let Some(to) = to {
        clauses.push("created_at < datetime(?)");
        params.push(Box::new(to.to_owned()));
    }
}

fn push_entity_filters(
    f: &UsageFilter,
    clauses: &mut Vec<&'static str>,
    params: &mut Vec<Box<dyn ToSql>>,
) {
    if let Some(pid) = &f.provider_id {
        clauses.push("provider_id = ?");
        params.push(Box::new(pid.0.clone()));
    }
    if let Some(mid) = &f.model_id {
        clauses.push("upstream_model_id = ?");
        params.push(Box::new(mid.to_owned()));
    }
    if let Some(aid) = f.account_id {
        clauses.push("account_id = ?");
        params.push(Box::new(aid.0));
    }
    if let Some(cid) = f.combo_id {
        clauses.push("combo_id = ?");
        params.push(Box::new(cid.0));
    }
    if let Some(kid) = f.api_key_id {
        clauses.push("api_key_id = ?");
        params.push(Box::new(kid.0));
    }
}

impl BuiltWhere {
    pub(crate) fn from_filter(f: &UsageFilter) -> Self {
        if f.is_empty() {
            return Self {
                sql: String::new(),
                params: Vec::new(),
            };
        }

        let mut clauses: Vec<&'static str> = Vec::new();
        let mut params: Vec<Box<dyn ToSql>> = Vec::new();

        push_date_filters(
            f.from.as_deref(),
            f.to.as_deref(),
            &mut clauses,
            &mut params,
        );
        push_entity_filters(f, &mut clauses, &mut params);

        let joined = clauses.join(" AND ");
        let mut sql = String::with_capacity(joined.len() + 7);
        sql.push_str("WHERE ");
        sql.push_str(&joined);
        Self { sql, params }
    }
}

pub(crate) fn to_params(v: &[Box<dyn ToSql>]) -> Vec<&dyn ToSql> {
    v.iter().map(|b| b.as_ref() as &dyn ToSql).collect()
}

pub(crate) fn get_u64_col(row: &Row<'_>, idx: usize, field: &'static str) -> rusqlite::Result<u64> {
    as_u64(row.get(idx)?, field)
}

pub(crate) fn collect_rows<T>(
    iter: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
    query_name: &'static str,
) -> openproxy_types::Result<Vec<T>> {
    iter.map(|r| {
        r.map_err(|e| openproxy_db::error::map_db_error_ctx(format!("read {query_name} row"))(e))
    })
    .collect()
}

pub(crate) fn as_u64(v: i64, field: &'static str) -> rusqlite::Result<u64> {
    if v < 0 {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(SimpleErr(format!("{field} unexpectedly negative: {v}"))),
        ));
    }
    Ok(v as u64)
}

#[derive(Debug)]
struct SimpleErr(String);
impl std::fmt::Display for SimpleErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SimpleErr {}

#[cfg(test)]
mod tests {
    use super::*;

    fn range_filter() -> UsageFilter {
        UsageFilter {
            from: Some("2026-06-15T00:00:00Z".to_string()),
            to: Some("2026-06-16T00:00:00Z".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn date_bounds_keep_column_bare_for_index_use() {
        // Rows store created_at as SQLite's 'YYYY-MM-DD HH:MM:SS' while the
        // handlers send RFC-3339 bounds, so the parameter side must stay
        // wrapped in datetime(). Wrapping the column instead defeats
        // idx_usage_created(_provider) and full-scans 700k+ rows per query.
        let w = BuiltWhere::from_filter(&range_filter());
        assert!(w.sql.contains("created_at >= datetime(?)"));
        assert!(w.sql.contains("created_at < datetime(?)"));
        assert!(
            !w.sql.contains("datetime(created_at)"),
            "column must stay bare so SQLite can use the created_at index"
        );
        assert_eq!(w.params.len(), 2);
    }

    #[test]
    fn from_only_and_to_only_bounds_each_emit_one_clause() {
        let from_only = BuiltWhere::from_filter(&UsageFilter {
            from: Some("2026-06-15T00:00:00Z".to_string()),
            ..Default::default()
        });
        assert_eq!(from_only.sql, "WHERE created_at >= datetime(?)");
        assert_eq!(from_only.params.len(), 1);

        let to_only = BuiltWhere::from_filter(&UsageFilter {
            to: Some("2026-06-16T00:00:00Z".to_string()),
            ..Default::default()
        });
        assert_eq!(to_only.sql, "WHERE created_at < datetime(?)");
        assert_eq!(to_only.params.len(), 1);
    }
}
