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
    /// Upstream models excluded from every aggregate (stats-view toggles).
    #[serde(default)]
    pub exclude_models: Vec<String>,
    /// Providers excluded from every aggregate (stats-view toggles).
    #[serde(default)]
    pub exclude_providers: Vec<String>,
    /// Public-space sandbox for non-manage (analytics-scope) keys: rows are
    /// projected to `{combo alias, whitelisted catalog entry}` and the
    /// visibility clause hides upstream internals entirely.
    #[serde(default)]
    pub scope_public: Option<PublicScope>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PublicScope {
    /// Public names (combo aliases / catalog model ids) toggled off in the
    /// ranking view. Applies to both branches of the visibility clause.
    #[serde(default)]
    pub excluded_public: Vec<String>,
    /// The key's catalog whitelist: direct rows (combo_id <= 0) must point to
    /// a catalog entry whose provider is whitelisted for this key.
    pub allowed_providers: Option<Vec<String>>,
    /// The key's catalog model whitelist (direct rows only).
    pub allowed_models: Option<Vec<String>>,
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
            !self.exclude_models.is_empty(),
            !self.exclude_providers.is_empty(),
            self.scope_public.is_some(),
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
    clauses: &mut Vec<String>,
    params: &mut Vec<Box<dyn ToSql>>,
) {
    if let Some(from) = from {
        // Qualified with the table name: public-scope queries LEFT JOIN
        // combos/models, which also have a `created_at` column. Bare
        // `usage.created_at` stays valid for the joinless queries.
        // The bound is normalized on the parameter side because callers pass
        // RFC-3339 strings while rows store SQLite's `YYYY-MM-DD HH:MM:SS`.
        clauses.push("usage.created_at >= datetime(?)".into());
        params.push(Box::new(from.to_owned()));
    }
    if let Some(to) = to {
        clauses.push("usage.created_at < datetime(?)".into());
        params.push(Box::new(to.to_owned()));
    }
}

fn push_entity_filters(
    f: &UsageFilter,
    clauses: &mut Vec<String>,
    params: &mut Vec<Box<dyn ToSql>>,
) {
    if let Some(scope) = &f.scope_public {
        push_public_entity_filters(f, scope, clauses, params);
        return;
    }
    if let Some(pid) = &f.provider_id {
        clauses.push("provider_id = ?".into());
        params.push(Box::new(pid.0.clone()));
    }
    if let Some(mid) = &f.model_id {
        clauses.push("upstream_model_id = ?".into());
        params.push(Box::new(mid.to_owned()));
    }
    if let Some(aid) = f.account_id {
        clauses.push("account_id = ?".into());
        params.push(Box::new(aid.0));
    }
    if let Some(cid) = f.combo_id {
        clauses.push("combo_id = ?".into());
        params.push(Box::new(cid.0));
    }
    if let Some(kid) = f.api_key_id {
        clauses.push("api_key_id = ?".into());
        params.push(Box::new(kid.0));
    }
    push_not_in(clauses, params, "upstream_model_id", &f.exclude_models);
    push_not_in(clauses, params, "provider_id", &f.exclude_providers);
}

/// Sandbox clause for non-manage keys. Rows are visible only when:
/// 1. They came through a combo (`combo_id > 0`) - the public name is the
///    combo alias; upstream internals of that combo are never exposed. The
///    excluded-public toggles (ranking) filter the alias set.
/// 2. They were direct catalog calls (`combo_id <= 0` or NULL) - the public
///    name is the catalog `model_id` the key sees in /v1/models, restricted
///    to the key's provider/model whitelists.
///
/// provider_id (internal) is deliberately not filterable: the projection
/// never exposes it, so probing it via filters must not reveal row counts.
fn push_public_entity_filters(
    f: &UsageFilter,
    scope: &PublicScope,
    clauses: &mut Vec<String>,
    params: &mut Vec<Box<dyn ToSql>>,
) {
    if let Some(aid) = f.account_id {
        clauses.push("account_id = ?".into());
        params.push(Box::new(aid.0));
    }
    if let Some(cid) = f.combo_id {
        clauses.push("combo_id = ?".into());
        params.push(Box::new(cid.0));
    }
    if let Some(kid) = f.api_key_id {
        clauses.push("api_key_id = ?".into());
        params.push(Box::new(kid.0));
    }
    if let Some(mid) = &f.model_id {
        // Map the public model filter to its two possible origins: a combo
        // alias or a direct catalog entry. Provider is never matched.
        clauses.push(
            "(combo_id IN (SELECT c.id FROM combos c WHERE c.name = ?) \
             OR (model_row_id IS NOT NULL AND model_row_id IN \
             (SELECT m.id FROM models m WHERE m.model_id = ?)))"
                .into(),
        );
        params.push(Box::new(mid.clone()));
        params.push(Box::new(mid.clone()));
    }
    push_public_visibility(scope, clauses, params);
}

fn push_in_list(sql: &mut String, params: &mut Vec<Box<dyn ToSql>>, col: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    let placeholders = std::iter::repeat("?")
        .take(values.len())
        .collect::<Vec<_>>()
        .join(", ");
    sql.push_str(&format!(" AND {col} IN ({placeholders})"));
    for v in values {
        params.push(Box::new(v.clone()));
    }
}

fn push_public_visibility(
    scope: &PublicScope,
    clauses: &mut Vec<String>,
    params: &mut Vec<Box<dyn ToSql>>,
) {
    let excluded = &scope.excluded_public;

    // Branch 1: combo rows. Alias = combos.name; hide the excluded toggles.
    let mut combo_sub = String::from(
        "(combo_id IS NOT NULL AND combo_id > 0 AND combo_id IN \
         (SELECT c.id FROM combos c WHERE 1=1",
    );
    push_in_list(&mut combo_sub, params, "c.name NOT", excluded);
    combo_sub.push_str("))");

    // Branch 2: direct catalog rows. Alias = models.model_id, restricted to
    // the key's whitelists; excluded toggles apply to the catalog name too.
    let mut direct_sub = String::from(
        "((combo_id IS NULL OR combo_id <= 0) AND model_row_id IS NOT NULL \
         AND model_row_id IN (SELECT m.id FROM models m WHERE 1=1",
    );
    push_in_list(&mut direct_sub, params, "m.model_id NOT", excluded);
    if let Some(providers) = &scope.allowed_providers {
        push_in_list(&mut direct_sub, params, "m.provider_id", providers);
    }
    if let Some(models) = &scope.allowed_models {
        push_in_list(&mut direct_sub, params, "m.model_id", models);
    }
    direct_sub.push_str("))");

    clauses.push(format!("({combo_sub} OR {direct_sub})"));
}

fn push_not_in(
    clauses: &mut Vec<String>,
    params: &mut Vec<Box<dyn ToSql>>,
    col: &str,
    values: &[String],
) {
    if values.is_empty() {
        return;
    }
    let placeholders = std::iter::repeat("?")
        .take(values.len())
        .collect::<Vec<_>>()
        .join(", ");
    clauses.push(format!("{col} NOT IN ({placeholders})"));
    for v in values {
        params.push(Box::new(v.clone()));
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

        let mut clauses: Vec<String> = Vec::new();
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

    #[test]
    fn test_as_u64_valid_and_negative() {
        assert_eq!(as_u64(0, "test_field").expect("valid 0"), 0);
        assert_eq!(as_u64(12345, "test_field").expect("valid positive"), 12345);

        let err = as_u64(-1, "test_field").unwrap_err();
        if let rusqlite::Error::FromSqlConversionFailure(_, _, err_box) = err {
            assert!(
                err_box
                    .to_string()
                    .contains("test_field unexpectedly negative: -1")
            );
        } else {
            panic!("expected FromSqlConversionFailure, got {err:?}");
        }
    }

    #[test]
    fn test_usage_filter_is_empty_and_built_where() {
        let empty_filter = UsageFilter::default();
        assert!(empty_filter.is_empty());

        let built_empty = BuiltWhere::from_filter(&empty_filter);
        assert_eq!(built_empty.sql, "");
        assert!(built_empty.params.is_empty());

        let filter = UsageFilter {
            from: Some("2025-01-01T00:00:00Z".to_string()),
            provider_id: Some(ProviderId::new("openai")),
            ..Default::default()
        };
        assert!(!filter.is_empty());

        let built = BuiltWhere::from_filter(&filter);
        assert!(built.sql.starts_with("WHERE "));
        assert!(built.sql.contains("created_at >= datetime(?)"));
        assert!(built.sql.contains("provider_id = ?"));
        assert_eq!(built.params.len(), 2);
    }

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
        assert_eq!(from_only.sql, "WHERE usage.created_at >= datetime(?)");
        assert_eq!(from_only.params.len(), 1);

        let to_only = BuiltWhere::from_filter(&UsageFilter {
            to: Some("2026-06-16T00:00:00Z".to_string()),
            ..Default::default()
        });
        assert_eq!(to_only.sql, "WHERE usage.created_at < datetime(?)");
        assert_eq!(to_only.params.len(), 1);
    }
}
