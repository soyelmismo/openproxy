use crate::error::*;
use crate::ids::*;
use rusqlite::{Connection, OptionalExtension, ToSql, params, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
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
    /// Restrict to rows produced under a specific API key. The
    /// per-key usage view (the dashboard's "Usage" tab on the
    /// keys page) sets this; the public-facing analytics endpoints
    /// leave it `None` so the global roll-up is unaffected.
    pub api_key_id: Option<ApiKeyId>,
}

impl UsageFilter {
    /// Returns `true` if no filter is set.
    fn is_empty(&self) -> bool {
        self.from.is_none()
            && self.to.is_none()
            && self.provider_id.is_none()
            && self.model_id.is_none()
            && self.account_id.is_none()
            && self.combo_id.is_none()
            && self.api_key_id.is_none()
    }
}

/// Aggregate roll-up of a filtered set of usage rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSummary {
    /// `COUNT(DISTINCT request_id)` — one per logical user request, regardless
    /// of how many race losers it produced.
    pub unique_requests: u64,
    /// `COUNT(*)` — every row, including race losers and retry attempts.
    pub total_rows: u64,
    /// `COUNT(DISTINCT trace_id)` — every per-attempt trace.
    pub total_attempts: u64,
    /// Rows where `race_lost = 0`. Note: non-race rows are also winners.
    pub winners: u64,
    /// Rows where `race_lost = 1`.
    pub losers: u64,
    /// Rows with `status_code >= 400`.
    pub errors: u64,
    pub total_prompt_tokens: i64,
    pub total_completion_tokens: i64,
    pub total_cost_usd: f64,
    /// `AVG(ttft_ms)` over rows where `ttft_ms IS NOT NULL`. `None` when no
    /// such row exists in the filter.
    pub avg_ttft_ms: Option<f64>,
    /// `AVG(total_ms)` over all rows in the filter.
    pub avg_total_ms: f64,
    /// Rows where `cost_usd = 0.0 AND prompt_tokens > 0` — i.e. the row
    /// consumed tokens (so pricing should have applied) but the cost column
    /// is zero, meaning pricing was missing at record time. Surfaces
    /// under-reporting in the dashboard.
    pub rows_with_null_pricing: u64,
}

/// One row of the `by_model` aggregation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ByModelRow {
    pub provider_id: ProviderId,
    pub upstream_model_id: String,
    pub unique_requests: u64,
    pub total_rows: u64,
    pub winners: u64,
    pub total_prompt_tokens: i64,
    pub total_completion_tokens: i64,
    pub total_cost_usd: f64,
}

/// One row of the `by_provider` aggregation.
///
/// Mirrors [`ByModelRow`] but groups by `provider_id` only — the frontend
/// "monthly usage by provider" report uses this for the top-level roll-up,
/// and [`monthly_by_provider`] for the time-bucketed breakdown.
#[derive(Debug, Clone, Serialize)]
pub struct ByProviderRow {
    pub provider_id: String,
    pub unique_requests: u64,
    pub total_rows: u64,
    pub winners: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_cost_usd: f64,
}

/// One row of the `monthly_by_provider` aggregation.
///
/// `month` is `strftime('%Y-%m', created_at)` — e.g. `"2026-06"`. Ordered
/// by `month ASC, total_cost_usd DESC` so the frontend can pivot into a
/// providers × months matrix that walks time forward.
#[derive(Debug, Clone, Serialize)]
pub struct MonthlyByProviderRow {
    pub provider_id: String,
    /// `"YYYY-MM"` — the calendar month in UTC.
    pub month: String,
    pub unique_requests: u64,
    pub total_rows: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_cost_usd: f64,
}

/// One row of the `by_day` aggregation — daily usage totals for
/// charting. `date` is `"YYYY-MM-DD"` in UTC.
#[derive(Debug, Clone, Serialize)]
pub struct ByDayRow {
    /// `"YYYY-MM-DD"` — the calendar day in UTC.
    pub date: String,
    pub unique_requests: u64,
    pub total_rows: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_cost_usd: f64,
    pub errors: u64,
}

/// One row of the `by_account` aggregation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ByAccountRow {
    pub account_id: AccountId,
    pub provider_id: ProviderId,
    pub unique_requests: u64,
    pub total_rows: u64,
    pub errors: u64,
    pub total_cost_usd: f64,
}

/// One row of the `by_status` aggregation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ByStatusRow {
    pub status_code: u16,
    pub count: u64,
}

/// One row of the `errors` query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorRow {
    pub request_id: String,
    pub trace_id: String,
    pub provider_id: ProviderId,
    pub upstream_model_id: String,
    pub status_code: u16,
    /// Pre-redacted error message; secrets already stripped by the writer
    /// ([`crate::cost::redact_error_msg`]).
    pub error_msg_redacted: Option<String>,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// WHERE-clause builder
// ---------------------------------------------------------------------------

/// A built `(where_sql, params)` pair ready to splice into a query.
///
/// `where_sql` is either the literal string `"WHERE ..."` (with the leading
/// `WHERE` keyword) or the empty string if no filter is set. It is designed
/// to be inserted directly into a SQL template via `format!`.
struct BuiltWhere {
    sql: String,
    params: Vec<Box<dyn ToSql>>,
}

impl BuiltWhere {
    fn from_filter(f: &UsageFilter) -> Self {
        if f.is_empty() {
            return Self {
                sql: String::new(),
                params: Vec::new(),
            };
        }

        let mut clauses: Vec<&'static str> = Vec::new();
        let mut params: Vec<Box<dyn ToSql>> = Vec::new();

        // IMPORTANT: wrap both `created_at` and the bound in `datetime(...)`.
        // `created_at` is written by SQLite's `datetime('now')` and stored as
        // `"YYYY-MM-DD HH:MM:SS"` (space separator), but the filter bounds come
        // in as RFC-3339 `"YYYY-MM-DDTHH:MM:SSZ"` (T separator, Z suffix).
        // SQLite TEXT comparison is byte-wise: `' '` (0x20) sorts before `'T'`
        // (0x54), so a raw `created_at >= ?` against an RFC-3339 bound would
        // be FALSE for every row and the query would return 0 rows (which is
        // exactly the dashboard "all zeros" bug). Wrapping both sides in
        // `datetime(...)` makes SQLite parse them as real datetimes before
        // comparing, regardless of the textual format. The prune functions
        // below already do the same thing — this just brings the read-side
        // analytics queries in line with them.
        if let Some(from) = &f.from {
            clauses.push("datetime(created_at) >= datetime(?)");
            params.push(Box::new(from.clone()));
        }
        if let Some(to) = &f.to {
            clauses.push("datetime(created_at) < datetime(?)");
            params.push(Box::new(to.clone()));
        }
        if let Some(pid) = &f.provider_id {
            clauses.push("provider_id = ?");
            params.push(Box::new(pid.0.clone()));
        }
        if let Some(mid) = &f.model_id {
            clauses.push("upstream_model_id = ?");
            params.push(Box::new(mid.clone()));
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

        let joined = clauses.join(" AND ");
        // joined is non-empty because is_empty() returned false above.
        let mut sql = String::with_capacity(joined.len() + 7);
        sql.push_str("WHERE ");
        sql.push_str(&joined);
        Self { sql, params }
    }
}

/// Wrapper that lets us call [`params_from_iter`] over a `Vec<Box<dyn ToSql>>`.
///
/// `params_from_iter` needs an `IntoIterator`; the boxed `ToSql` trait objects
/// own their data and implement the trait via `dyn ToSql`, but `params_from_iter`
/// is generic over `IntoIterator` so this pass-through is the cleanest way to
/// keep the call site readable.
fn to_params(v: &[Box<dyn ToSql>]) -> Vec<&dyn ToSql> {
    v.iter().map(|b| b.as_ref() as &dyn ToSql).collect()
}

// ---------------------------------------------------------------------------
// Public queries
// ---------------------------------------------------------------------------

/// Aggregate summary over all rows matching `f`.
pub fn summary(conn: &Connection, f: &UsageFilter) -> Result<UsageSummary> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT \
             COUNT(DISTINCT request_id)                                    AS unique_requests, \
             COUNT(*)                                                      AS total_rows, \
             COUNT(DISTINCT trace_id)                                      AS total_attempts, \
             SUM(CASE WHEN race_lost = 0 THEN 1 ELSE 0 END)                AS winners, \
             SUM(CASE WHEN race_lost = 1 THEN 1 ELSE 0 END)                AS losers, \
             SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END)            AS errors, \
             COALESCE(SUM(prompt_tokens), 0)                                AS total_prompt_tokens, \
             COALESCE(SUM(completion_tokens), 0)                            AS total_completion_tokens, \
             COALESCE(SUM(cost_usd), 0.0)                                   AS total_cost_usd, \
             AVG(ttft_ms) FILTER (WHERE ttft_ms IS NOT NULL)               AS avg_ttft_ms, \
             COALESCE(AVG(total_ms), 0.0)                                   AS avg_total_ms, \
             SUM(CASE WHEN cost_usd = 0.0 AND prompt_tokens > 0 THEN 1 ELSE 0 END) AS rows_with_null_pricing \
         FROM usage {}",
        w.sql,
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(openproxy_db::error::map_db_error)?;

    let params_slice = to_params(&w.params);
    let summary = stmt
        .query_row(params_from_iter(params_slice), |row| {
            let unique_requests: i64 = row.get(0)?;
            let total_rows: i64 = row.get(1)?;
            let total_attempts: i64 = row.get(2)?;
            // SUM(...) over an empty result set yields NULL in SQLite, not 0,
            // so we accept Option<i64> and substitute 0. COALESCE only helps
            // for nullable columns (prompt_tokens, cost_usd), not for the
            // SUM(CASE WHEN...) zero-row edge case.
            let winners: i64 = row.get::<_, Option<i64>>(3)?.unwrap_or(0);
            let losers: i64 = row.get::<_, Option<i64>>(4)?.unwrap_or(0);
            let errors: i64 = row.get::<_, Option<i64>>(5)?.unwrap_or(0);
            let total_prompt_tokens: i64 = row.get(6)?;
            let total_completion_tokens: i64 = row.get(7)?;
            let total_cost_usd: f64 = row.get(8)?;
            let avg_ttft_ms: Option<f64> = row.get(9)?;
            let avg_total_ms: f64 = row.get(10)?;
            let rows_with_null_pricing: i64 = row.get::<_, Option<i64>>(11)?.unwrap_or(0);

            Ok(UsageSummary {
                unique_requests: as_u64(unique_requests, "unique_requests")?,
                total_rows: as_u64(total_rows, "total_rows")?,
                total_attempts: as_u64(total_attempts, "total_attempts")?,
                winners: as_u64(winners, "winners")?,
                losers: as_u64(losers, "losers")?,
                errors: as_u64(errors, "errors")?,
                total_prompt_tokens,
                total_completion_tokens,
                total_cost_usd,
                avg_ttft_ms,
                avg_total_ms,
                rows_with_null_pricing: as_u64(rows_with_null_pricing, "rows_with_null_pricing")?,
            })
        })
        .map_err(openproxy_db::error::map_db_error)?;

    Ok(summary)
}

/// Per-(provider, model) breakdown. Ordered by total cost descending.
pub fn by_model(conn: &Connection, f: &UsageFilter) -> Result<Vec<ByModelRow>> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT \
             provider_id, \
             upstream_model_id, \
             COUNT(DISTINCT request_id)                       AS unique_requests, \
             COUNT(*)                                         AS total_rows, \
             SUM(CASE WHEN race_lost = 0 THEN 1 ELSE 0 END)   AS winners, \
             COALESCE(SUM(prompt_tokens), 0)                  AS total_prompt_tokens, \
             COALESCE(SUM(completion_tokens), 0)              AS total_completion_tokens, \
             COALESCE(SUM(cost_usd), 0.0)                     AS total_cost_usd \
         FROM usage {} \
         GROUP BY provider_id, upstream_model_id \
         ORDER BY total_cost_usd DESC, provider_id ASC, upstream_model_id ASC",
        w.sql,
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(openproxy_db::error::map_db_error)?;

    let params_slice = to_params(&w.params);
    let rows = stmt
        .query_map(params_from_iter(params_slice), |row| {
            let provider_id: String = row.get(0)?;
            let upstream_model_id: String = row.get(1)?;
            let unique_requests: i64 = row.get(2)?;
            let total_rows: i64 = row.get(3)?;
            let winners: i64 = row.get(4)?;
            let total_prompt_tokens: i64 = row.get(5)?;
            let total_completion_tokens: i64 = row.get(6)?;
            let total_cost_usd: f64 = row.get(7)?;

            Ok(ByModelRow {
                provider_id: ProviderId::new(provider_id),
                upstream_model_id,
                unique_requests: as_u64(unique_requests, "unique_requests")?,
                total_rows: as_u64(total_rows, "total_rows")?,
                winners: as_u64(winners, "winners")?,
                total_prompt_tokens,
                total_completion_tokens,
                total_cost_usd,
            })
        })
        .map_err(openproxy_db::error::map_db_error)?;

    collect_rows(rows, "by_model")
}

/// Per-`provider_id` breakdown. Ordered by total cost descending so the
/// biggest spend providers float to the top.
///
/// Mirrors [`by_model`] but groups by `provider_id` only (no
/// `upstream_model_id`). The frontend's "monthly usage by provider" report
/// uses this for the top-level roll-up and [`monthly_by_provider`] for the
/// time-bucketed breakdown.
pub fn by_provider(conn: &Connection, f: &UsageFilter) -> Result<Vec<ByProviderRow>> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT \
             provider_id, \
             COUNT(DISTINCT request_id)                       AS unique_requests, \
             COUNT(*)                                         AS total_rows, \
             SUM(CASE WHEN race_lost = 0 THEN 1 ELSE 0 END)   AS winners, \
             COALESCE(SUM(prompt_tokens), 0)                  AS total_prompt_tokens, \
             COALESCE(SUM(completion_tokens), 0)              AS total_completion_tokens, \
             COALESCE(SUM(cost_usd), 0.0)                     AS total_cost_usd \
         FROM usage {} \
         GROUP BY provider_id \
         ORDER BY total_cost_usd DESC, provider_id ASC",
        w.sql,
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(openproxy_db::error::map_db_error)?;

    let params_slice = to_params(&w.params);
    let rows = stmt
        .query_map(params_from_iter(params_slice), |row| {
            let provider_id: String = row.get(0)?;
            let unique_requests: i64 = row.get(1)?;
            let total_rows: i64 = row.get(2)?;
            let winners: i64 = row.get(3)?;
            let total_prompt_tokens: i64 = row.get(4)?;
            let total_completion_tokens: i64 = row.get(5)?;
            let total_cost_usd: f64 = row.get(6)?;

            Ok(ByProviderRow {
                provider_id,
                unique_requests: as_u64(unique_requests, "unique_requests")?,
                total_rows: as_u64(total_rows, "total_rows")?,
                winners: as_u64(winners, "winners")?,
                total_prompt_tokens: as_u64(total_prompt_tokens, "total_prompt_tokens")?,
                total_completion_tokens: as_u64(
                    total_completion_tokens,
                    "total_completion_tokens",
                )?,
                total_cost_usd,
            })
        })
        .map_err(openproxy_db::error::map_db_error)?;

    collect_rows(rows, "by_provider")
}

/// Per-`(provider_id, month)` breakdown, where `month = strftime('%Y-%m',
/// created_at)`. Ordered by `month ASC, total_cost_usd DESC` so the
/// frontend can pivot into a providers × months matrix that walks time
/// forward and within each month shows the biggest spend provider first.
///
/// `winners` is intentionally omitted — the monthly view is for cost /
/// token tracking, not race outcomes. Use [`by_provider`] if you need
/// winner counts.
pub fn monthly_by_provider(
    conn: &Connection,
    f: &UsageFilter,
) -> Result<Vec<MonthlyByProviderRow>> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT \
             provider_id, \
             strftime('%Y-%m', created_at)                     AS month, \
             COUNT(DISTINCT request_id)                       AS unique_requests, \
             COUNT(*)                                         AS total_rows, \
             COALESCE(SUM(prompt_tokens), 0)                  AS total_prompt_tokens, \
             COALESCE(SUM(completion_tokens), 0)              AS total_completion_tokens, \
             COALESCE(SUM(cost_usd), 0.0)                     AS total_cost_usd \
         FROM usage {} \
         GROUP BY provider_id, month \
         ORDER BY month ASC, total_cost_usd DESC, provider_id ASC",
        w.sql,
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(openproxy_db::error::map_db_error)?;

    let params_slice = to_params(&w.params);
    let rows = stmt
        .query_map(params_from_iter(params_slice), |row| {
            let provider_id: String = row.get(0)?;
            let month: String = row.get(1)?;
            let unique_requests: i64 = row.get(2)?;
            let total_rows: i64 = row.get(3)?;
            let total_prompt_tokens: i64 = row.get(4)?;
            let total_completion_tokens: i64 = row.get(5)?;
            let total_cost_usd: f64 = row.get(6)?;

            Ok(MonthlyByProviderRow {
                provider_id,
                month,
                unique_requests: as_u64(unique_requests, "unique_requests")?,
                total_rows: as_u64(total_rows, "total_rows")?,
                total_prompt_tokens: as_u64(total_prompt_tokens, "total_prompt_tokens")?,
                total_completion_tokens: as_u64(
                    total_completion_tokens,
                    "total_completion_tokens",
                )?,
                total_cost_usd,
            })
        })
        .map_err(openproxy_db::error::map_db_error)?;

    collect_rows(rows, "monthly_by_provider")
}

/// Per-day usage totals for charting. Groups by
/// `strftime('%Y-%m-%d', created_at)` and returns rows ordered by
/// date ASC. Each row includes request counts, token totals, cost,
/// and error count (rows with `status_code >= 400`).
pub fn by_day(conn: &Connection, f: &UsageFilter) -> Result<Vec<ByDayRow>> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT \
             strftime('%Y-%m-%d', created_at)                     AS date, \
             COUNT(DISTINCT request_id)                           AS unique_requests, \
             COUNT(*)                                             AS total_rows, \
             COALESCE(SUM(prompt_tokens), 0)                      AS total_prompt_tokens, \
             COALESCE(SUM(completion_tokens), 0)                  AS total_completion_tokens, \
             COALESCE(SUM(cost_usd), 0.0)                         AS total_cost_usd, \
             SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END)  AS errors \
         FROM usage {} \
         GROUP BY date \
         ORDER BY date ASC",
        w.sql,
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(openproxy_db::error::map_db_error)?;

    let params_slice = to_params(&w.params);
    let rows = stmt
        .query_map(params_from_iter(params_slice), |row| {
            let date: String = row.get(0)?;
            let unique_requests: i64 = row.get(1)?;
            let total_rows: i64 = row.get(2)?;
            let total_prompt_tokens: i64 = row.get(3)?;
            let total_completion_tokens: i64 = row.get(4)?;
            let total_cost_usd: f64 = row.get(5)?;
            let errors: i64 = row.get(6)?;

            Ok(ByDayRow {
                date,
                unique_requests: as_u64(unique_requests, "unique_requests")?,
                total_rows: as_u64(total_rows, "total_rows")?,
                total_prompt_tokens: as_u64(total_prompt_tokens, "total_prompt_tokens")?,
                total_completion_tokens: as_u64(
                    total_completion_tokens,
                    "total_completion_tokens",
                )?,
                total_cost_usd,
                errors: as_u64(errors, "errors")?,
            })
        })
        .map_err(openproxy_db::error::map_db_error)?;

    collect_rows(rows, "by_day")
}

/// Per-(account, provider) breakdown. Ordered by total cost descending.
///
/// Note: an account's `provider_id` is technically redundant in a real schema
/// with a FK on `accounts.provider_id`, but the `usage` table itself doesn't
/// enforce that relationship, so we group by both for correctness and to
/// avoid a join.
pub fn by_account(conn: &Connection, f: &UsageFilter) -> Result<Vec<ByAccountRow>> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT \
             account_id, \
             provider_id, \
             COUNT(DISTINCT request_id)                          AS unique_requests, \
             COUNT(*)                                            AS total_rows, \
             SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END) AS errors, \
             COALESCE(SUM(cost_usd), 0.0)                        AS total_cost_usd \
         FROM usage {} \
         GROUP BY account_id, provider_id \
         ORDER BY total_cost_usd DESC, account_id ASC",
        w.sql,
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(openproxy_db::error::map_db_error)?;

    let params_slice = to_params(&w.params);
    let rows = stmt
        .query_map(params_from_iter(params_slice), |row| {
            let account_id: Option<i64> = row.get(0)?;
            let provider_id: String = row.get(1)?;
            let unique_requests: i64 = row.get(2)?;
            let total_rows: i64 = row.get(3)?;
            let errors: i64 = row.get(4)?;
            let total_cost_usd: f64 = row.get(5)?;

            // `account_id` is nullable in the schema. The group-by collapses
            // NULLs into a single bucket in SQLite, so an account-less row
            // is possible. Surface it as a stable synthetic id of -1 so
            // downstream JSON has a numeric value, but keep the provider
            // id alongside so callers can disambiguate.
            let aid = account_id.unwrap_or(-1);
            if aid == 0 {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr("account_id must be non-zero".into())),
                ));
            }

            Ok(ByAccountRow {
                account_id: AccountId::new(aid),
                provider_id: ProviderId::new(provider_id),
                unique_requests: as_u64(unique_requests, "unique_requests")?,
                total_rows: as_u64(total_rows, "total_rows")?,
                errors: as_u64(errors, "errors")?,
                total_cost_usd,
            })
        })
        .map_err(openproxy_db::error::map_db_error)?;

    collect_rows(rows, "by_account")
}

/// Per-`status_code` count. Ordered by count descending so the busiest code
/// comes first; ties broken by `status_code` ascending.
pub fn by_status(conn: &Connection, f: &UsageFilter) -> Result<Vec<ByStatusRow>> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT status_code, COUNT(*) AS cnt \
         FROM usage {} \
         GROUP BY status_code \
         ORDER BY cnt DESC, status_code ASC",
        w.sql,
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(openproxy_db::error::map_db_error)?;

    let params_slice = to_params(&w.params);
    let rows = stmt
        .query_map(params_from_iter(params_slice), |row| {
            let status_code: i64 = row.get(0)?;
            let count: i64 = row.get(1)?;
            if !(0..=u16::MAX as i64).contains(&status_code) {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr(format!(
                        "status_code out of u16 range: {}",
                        status_code
                    ))),
                ));
            }
            Ok(ByStatusRow {
                status_code: status_code as u16,
                count: as_u64(count, "count")?,
            })
        })
        .map_err(openproxy_db::error::map_db_error)?;

    collect_rows(rows, "by_status")
}

/// Recent error rows (`status_code >= 400`), newest first, capped at `limit`.
///
/// `limit` is a hard cap on the number of returned rows; callers typically
/// pass 100 (matching the spec example) and the value is forwarded verbatim
/// to SQL as an integer parameter.
pub fn errors(conn: &Connection, f: &UsageFilter, limit: u32) -> Result<Vec<ErrorRow>> {
    let w = BuiltWhere::from_filter(f);

    // Build the WHERE clause manually because we need to AND in the
    // status_code predicate that isn't part of `UsageFilter`.
    let mut clauses: Vec<String> = Vec::new();
    if !w.sql.is_empty() {
        // Strip the leading "WHERE " to get bare clauses we can AND with more.
        let bare = w.sql.trim_start_matches("WHERE ").to_string();
        clauses.push(format!("({})", bare));
    }
    clauses.push("status_code >= 400".to_string());
    let where_clause = format!("WHERE {}", clauses.join(" AND "));

    let mut sql = String::new();
    write!(
        &mut sql,
        "SELECT request_id, trace_id, provider_id, upstream_model_id, \
                status_code, error_msg_redacted, created_at \
         FROM usage {} \
         ORDER BY created_at DESC, id DESC \
         LIMIT ?",
        where_clause,
    )
    .expect("writing to String never fails");

    // `limit` is a u32 — well under i64::MAX — so this cast is safe.
    let limit_param: i64 = limit as i64;
    let mut all_params: Vec<Box<dyn ToSql>> = w.params;
    all_params.push(Box::new(limit_param));
    let params_slice = to_params(&all_params);

    let mut stmt = conn
        .prepare(&sql)
        .map_err(openproxy_db::error::map_db_error)?;

    let rows = stmt
        .query_map(params_from_iter(params_slice), |row| {
            let request_id: String = row.get(0)?;
            let trace_id: String = row.get(1)?;
            let provider_id: String = row.get(2)?;
            let upstream_model_id: String = row.get(3)?;
            let status_code: i64 = row.get(4)?;
            let error_msg_redacted: Option<String> = row.get(5)?;
            let created_at: String = row.get(6)?;
            if !(0..=u16::MAX as i64).contains(&status_code) {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr(format!(
                        "status_code out of u16 range: {}",
                        status_code
                    ))),
                ));
            }
            Ok(ErrorRow {
                request_id,
                trace_id,
                provider_id: ProviderId::new(provider_id),
                upstream_model_id,
                status_code: status_code as u16,
                error_msg_redacted,
                created_at,
            })
        })
        .map_err(openproxy_db::error::map_db_error)?;

    collect_rows(rows, "errors")
}

// ---------------------------------------------------------------------------
// Recent rows (long-polling support)
// ---------------------------------------------------------------------------

/// Full `usage` row projection for live-log detail views.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageDetailRow {
    pub id: UsageId,
    pub request_id: String,
    pub trace_id: String,
    pub attempt: i64,
    pub provider_id: ProviderId,
    pub account_id: Option<AccountId>,
    pub combo_id: Option<ComboId>,
    pub combo_target_id: Option<ComboTargetId>,
    pub model_row_id: Option<ModelRowId>,
    pub upstream_model_id: String,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub connect_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub total_ms: i64,
    pub tokens_per_sec: Option<f64>,
    pub status_code: u16,
    pub error_msg: Option<String>,
    pub error_msg_redacted: Option<String>,
    pub request_body_json: Option<Value>,
    pub response_body_json: Option<Value>,
    pub request_headers: Option<BTreeMap<String, String>>,
    pub response_headers: Option<BTreeMap<String, String>>,
    pub error_message: Option<String>,
    pub race_total: i64,
    pub race_attempts: i64,
    pub race_lost: bool,
    pub is_streaming: bool,
    pub stream_complete: bool,
    pub api_key_id: Option<ApiKeyId>,
    /// True iff this row's response was actually delivered to the HTTP
    /// client (winning attempt). False for intermediate retries.
    pub client_response: bool,
    /// True if prompt_tokens were estimated (upstream didn't report usage).
    pub prompt_tokens_estimated: bool,
    /// True if completion_tokens were estimated (upstream didn't report usage).
    pub completion_tokens_estimated: bool,
    pub proxy_url: Option<String>,
    pub proxy_status: Option<String>,
    pub is_proxy_rotated: bool,
    /// The endpoint kind (chat, audio, image, etc.). Defaults to Chat.
    pub endpoint_kind: openproxy_types::endpoint::EndpointKind,
    pub created_at: String,
}

/// Return up to `limit` usage rows whose `id` is strictly greater than
/// `since_id`, oldest first (so the dashboard can append in order).
///
/// The implementation is the read side of the long-polling feed the
/// dashboard uses in place of an SSE channel: the client passes back the
/// last id it has seen, and we return everything that arrived since.
///
/// `limit` is a hard cap and is forwarded verbatim to SQL.
pub fn recent(
    conn: &Connection,
    since_id: i64,
    limit: u32,
) -> Result<Vec<openproxy_types::usage::RecentUsageRow>> {
    // Each retry within a single request writes a separate row to the
    // `usage` table. The dashboard's live-logs view shows EACH attempt
    // as its own row (keyed by `trace_id`), so the operator can inspect
    // a particular 5xx/4xx/timeout attempt individually.
    //
    // Previously this query GROUPed BY `request_id` and SUMmed
    // `prompt_tokens` / `completion_tokens` / `cost_usd` across all
    // attempts. That was misleading: a request with 10 race attempts
    // of 128k tokens each showed "1.28M tokens" (or more) in the
    // table, and the `client_response` indicator used `MAX(...)` so
    // the checkmark appeared on rows where `client_response` was
    // actually `false`. The operator cannot debug a specific attempt
    // when its data is merged with sibling attempts.
    //
    // The fix: return each attempt as its own row, no grouping. The
    // WS broadcast already sends individual rows (see
    // `publish_usage_row`), so this makes the history fetch and the
    // WS broadcast consistent — the merge function in the frontend
    // keys by `trace_id` (per-attempt) and no longer collapses
    // sibling attempts.
    //
    // `since_id` pagination still works: each row has its own unique
    // `id`, and `id > ?1` correctly returns only rows the dashboard
    // hasn't seen yet. The long-poll feed returns rows in id ASC order
    // (oldest first) so the client processes them in arrival order;
    // the dashboard's `mergeLogsByDescId` sorts by id DESC for display.
    let limit_param: i64 = limit as i64;
    let mut stmt = conn
        .prepare(
            "SELECT id, request_id, trace_id, provider_id, upstream_model_id, \
                    status_code, total_ms, prompt_tokens, completion_tokens, \
                    cost_usd, connect_ms, ttft_ms, request_body_json, response_body_json, \
                    request_headers, response_headers, error_msg_redacted, error_msg, \
                    race_total, race_attempts, is_streaming, stream_complete, \
                    race_lost, created_at, stop_reason, \
                    compression_savings_pct, compression_techniques, \
                    client_response, prompt_tokens_estimated, completion_tokens_estimated, \
                    endpoint_kind, proxy_url, proxy_status, is_proxy_rotated \
             FROM usage \
             WHERE id > ?1 \
             ORDER BY id ASC \
             LIMIT ?2",
        )
        .map_err(openproxy_db::error::map_db_error)?;

    let rows = stmt
        .query_map(params![since_id, limit_param], |row| {
            let mut col_idx = 0;
            let id: i64 = row.get(col_idx)?;
            col_idx += 1;
            let request_id: String = row.get(col_idx)?;
            col_idx += 1;
            let trace_id: String = row.get(col_idx)?;
            col_idx += 1;
            let provider_id: String = row.get(col_idx)?;
            col_idx += 1;
            let upstream_model_id: String = row.get(col_idx)?;
            col_idx += 1;
            let status_code: i64 = row.get(col_idx)?;
            col_idx += 1;
            let total_ms: i64 = row.get(col_idx)?;
            col_idx += 1;
            let prompt_tokens: Option<i64> = row.get(col_idx)?;
            col_idx += 1;
            let completion_tokens: Option<i64> = row.get(col_idx)?;
            col_idx += 1;
            let cost_usd: Option<f64> = row.get(col_idx)?;
            col_idx += 1;
            let connect_ms: Option<i64> = row.get(col_idx)?;
            col_idx += 1;
            let ttft_ms: Option<i64> = row.get(col_idx)?;
            col_idx += 1;
            let request_body_json: Option<serde_json::Value> = row
                .get::<_, Option<String>>(col_idx)?
                .and_then(|s| serde_json::from_str(&s).ok());
            col_idx += 1;
            let response_body_json: Option<serde_json::Value> = row
                .get::<_, Option<String>>(col_idx)?
                .and_then(|s| serde_json::from_str(&s).ok());
            col_idx += 1;
            let request_headers: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let response_headers: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let error_msg_redacted: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let error_msg: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let race_total: i64 = row.get(col_idx)?;
            col_idx += 1;
            let race_attempts: i64 = row.get(col_idx)?;
            col_idx += 1;
            let is_streaming: i64 = row.get(col_idx)?;
            col_idx += 1;
            let stream_complete: i64 = row.get(col_idx)?;
            col_idx += 1;
            let race_lost: i64 = row.get(col_idx)?;
            col_idx += 1;
            let created_at: String = row.get(col_idx)?;
            col_idx += 1;
            let stop_reason: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let compression_savings_pct: Option<f64> = row.get(col_idx)?;
            col_idx += 1;
            let compression_techniques: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let client_response: i64 = row.get(col_idx)?;
            col_idx += 1;
            let prompt_tokens_estimated: i64 = row.get(col_idx)?;
            col_idx += 1;
            let completion_tokens_estimated: i64 = row.get(col_idx)?;
            col_idx += 1;
            let endpoint_kind_str: String = row.get(col_idx)?;
            col_idx += 1;
            let proxy_url: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let proxy_status: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let is_proxy_rotated: i64 = row.get(col_idx)?;

            if !(0..=u16::MAX as i64).contains(&status_code) {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr(format!(
                        "status_code out of u16 range: {}",
                        status_code
                    ))),
                ));
            }
            if total_ms < 0 {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr(format!(
                        "total_ms unexpectedly negative: {}",
                        total_ms
                    ))),
                ));
            }
            let request_headers = request_headers.and_then(|s| serde_json::from_str(&s).ok());
            let response_headers = response_headers.and_then(|s| serde_json::from_str(&s).ok());
            let error_message = error_msg_redacted.or(error_msg);
            let prompt_tokens = prompt_tokens.and_then(|v| u32::try_from(v).ok());
            let completion_tokens = completion_tokens.and_then(|v| u32::try_from(v).ok());
            let race_total_u8 = u8::try_from(race_total).ok();
            let race_attempts_u8 = u8::try_from(race_attempts).ok();
            let is_streaming_bool = is_streaming != 0;
            let stream_complete_bool = stream_complete != 0;
            let endpoint_kind = match endpoint_kind_str.as_str() {
                "chat" => openproxy_types::endpoint::EndpointKind::Chat,
                "audio" => openproxy_types::endpoint::EndpointKind::Audio,
                "image" => openproxy_types::endpoint::EndpointKind::Image,
                "embedding" => openproxy_types::endpoint::EndpointKind::Embedding,
                "video" => openproxy_types::endpoint::EndpointKind::Video,
                _ => openproxy_types::endpoint::EndpointKind::Chat,
            };
            Ok(openproxy_types::usage::RecentUsageRow {
                id: UsageId(id),
                request_id,
                trace_id,
                provider_id: ProviderId::new(provider_id),
                upstream_model_id,
                status_code: status_code as u16,
                total_ms: total_ms as u64,
                prompt_tokens,
                completion_tokens,
                cost_usd,
                connect_ms: connect_ms.map(|v| v as u64),
                ttft_ms: ttft_ms.map(|v| v as u64),
                request_body_json,
                response_body_json,
                request_headers,
                response_headers,
                error_message,
                race_total: race_total_u8,
                race_attempts: race_attempts_u8,
                is_streaming: is_streaming_bool,
                stream_complete: stream_complete_bool,
                race_lost: race_lost != 0,
                stop_reason,
                compression_savings_pct,
                compression_techniques,
                client_response: client_response != 0,
                prompt_tokens_estimated: prompt_tokens_estimated != 0,
                completion_tokens_estimated: completion_tokens_estimated != 0,
                proxy_url,
                proxy_status,
                is_proxy_rotated: is_proxy_rotated != 0,
                endpoint_kind,
                created_at,
            })
        })
        .map_err(openproxy_db::error::map_db_error)?;

    collect_rows(rows, "recent")
}

/// Return the most recent `limit` usage rows, newest first.
///
/// H1 fix: this mirrors the per-request aggregation in
/// `recent()` so the dashboard sees the same grouped
/// cost/token SUMs whether it's polling (`recent`) or
/// fetching the head (`recent_desc`). Without this, the
/// same 3-attempt request that `recent()` shows as 0.03
/// cost would show as 0.01 in the admin table at the top.
pub fn recent_desc(
    conn: &Connection,
    limit: u32,
) -> Result<Vec<openproxy_types::usage::RecentUsageRow>> {
    // Each attempt as its own row, no grouping. See `recent()` for the
    // full rationale — the short version is: grouping by `request_id`
    // and SUMming tokens across race attempts produced misleading
    // values (3.9M instead of 128k) and the `MAX(client_response)`
    // showed a checkmark on rows where `client_response` was actually
    // `false`. The WS broadcast sends individual rows, so the history
    // fetch must too.
    let limit_param: i64 = limit as i64;
    let mut stmt = conn
        .prepare(
            "SELECT id, request_id, trace_id, provider_id, upstream_model_id, \
                    status_code, total_ms, prompt_tokens, completion_tokens, \
                    cost_usd, connect_ms, ttft_ms, request_body_json, response_body_json, \
                    request_headers, response_headers, error_msg_redacted, error_msg, \
                    race_total, race_attempts, is_streaming, stream_complete, \
                    race_lost, created_at, stop_reason, \
                    compression_savings_pct, compression_techniques, \
                    client_response, prompt_tokens_estimated, completion_tokens_estimated, \
                    endpoint_kind, proxy_url, proxy_status, is_proxy_rotated \
             FROM usage \
             ORDER BY id DESC \
             LIMIT ?1",
        )
        .map_err(openproxy_db::error::map_db_error)?;

    let rows = stmt
        .query_map(params![limit_param], |row| {
            let mut col_idx = 0;
            let id: i64 = row.get(col_idx)?;
            col_idx += 1;
            let request_id: String = row.get(col_idx)?;
            col_idx += 1;
            let trace_id: String = row.get(col_idx)?;
            col_idx += 1;
            let provider_id: String = row.get(col_idx)?;
            col_idx += 1;
            let upstream_model_id: String = row.get(col_idx)?;
            col_idx += 1;
            let status_code: i64 = row.get(col_idx)?;
            col_idx += 1;
            let total_ms: i64 = row.get(col_idx)?;
            col_idx += 1;
            let prompt_tokens: Option<i64> = row.get(col_idx)?;
            col_idx += 1;
            let completion_tokens: Option<i64> = row.get(col_idx)?;
            col_idx += 1;
            let cost_usd: Option<f64> = row.get(col_idx)?;
            col_idx += 1;
            let connect_ms: Option<i64> = row.get(col_idx)?;
            col_idx += 1;
            let ttft_ms: Option<i64> = row.get(col_idx)?;
            col_idx += 1;
            let request_body_json: Option<serde_json::Value> = row
                .get::<_, Option<String>>(col_idx)?
                .and_then(|s| serde_json::from_str(&s).ok());
            col_idx += 1;
            let response_body_json: Option<serde_json::Value> = row
                .get::<_, Option<String>>(col_idx)?
                .and_then(|s| serde_json::from_str(&s).ok());
            col_idx += 1;
            let request_headers: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let response_headers: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let error_msg_redacted: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let error_msg: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let race_total: i64 = row.get(col_idx)?;
            col_idx += 1;
            let race_attempts: i64 = row.get(col_idx)?;
            col_idx += 1;
            let is_streaming: i64 = row.get(col_idx)?;
            col_idx += 1;
            let stream_complete: i64 = row.get(col_idx)?;
            col_idx += 1;
            let race_lost: i64 = row.get(col_idx)?;
            col_idx += 1;
            let created_at: String = row.get(col_idx)?;
            col_idx += 1;
            let stop_reason: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let compression_savings_pct: Option<f64> = row.get(col_idx)?;
            col_idx += 1;
            let compression_techniques: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let client_response: i64 = row.get(col_idx)?;
            col_idx += 1;
            let prompt_tokens_estimated: i64 = row.get(col_idx)?;
            col_idx += 1;
            let completion_tokens_estimated: i64 = row.get(col_idx)?;
            col_idx += 1;
            let endpoint_kind_str: String = row.get(col_idx)?;
            col_idx += 1;
            let proxy_url: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let proxy_status: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let is_proxy_rotated: i64 = row.get(col_idx)?;

            if !(0..=u16::MAX as i64).contains(&status_code) {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr(format!(
                        "status_code out of u16 range: {}",
                        status_code
                    ))),
                ));
            }
            if total_ms < 0 {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr(format!(
                        "total_ms unexpectedly negative: {}",
                        total_ms
                    ))),
                ));
            }
            let request_headers = request_headers.and_then(|s| serde_json::from_str(&s).ok());
            let response_headers = response_headers.and_then(|s| serde_json::from_str(&s).ok());
            let error_message = error_msg_redacted.or(error_msg);
            let prompt_tokens = prompt_tokens.and_then(|v| u32::try_from(v).ok());
            let completion_tokens = completion_tokens.and_then(|v| u32::try_from(v).ok());
            let race_total_u8 = u8::try_from(race_total).ok();
            let race_attempts_u8 = u8::try_from(race_attempts).ok();
            let is_streaming_bool = is_streaming != 0;
            let stream_complete_bool = stream_complete != 0;
            let endpoint_kind = match endpoint_kind_str.as_str() {
                "chat" => openproxy_types::endpoint::EndpointKind::Chat,
                "audio" => openproxy_types::endpoint::EndpointKind::Audio,
                "image" => openproxy_types::endpoint::EndpointKind::Image,
                "embedding" => openproxy_types::endpoint::EndpointKind::Embedding,
                "video" => openproxy_types::endpoint::EndpointKind::Video,
                _ => openproxy_types::endpoint::EndpointKind::Chat,
            };

            Ok(openproxy_types::usage::RecentUsageRow {
                id: UsageId(id),
                request_id,
                trace_id,
                provider_id: ProviderId::new(provider_id),
                upstream_model_id,
                status_code: status_code as u16,
                total_ms: total_ms as u64,
                prompt_tokens,
                completion_tokens,
                cost_usd,
                connect_ms: connect_ms.map(|v| v as u64),
                ttft_ms: ttft_ms.map(|v| v as u64),
                request_body_json,
                response_body_json,
                request_headers,
                response_headers,
                error_message,
                race_total: race_total_u8,
                race_attempts: race_attempts_u8,
                is_streaming: is_streaming_bool,
                stream_complete: stream_complete_bool,
                race_lost: race_lost != 0,
                stop_reason,
                compression_savings_pct,
                compression_techniques,
                client_response: client_response != 0,
                prompt_tokens_estimated: prompt_tokens_estimated != 0,
                completion_tokens_estimated: completion_tokens_estimated != 0,
                proxy_url,
                proxy_status,
                is_proxy_rotated: is_proxy_rotated != 0,
                endpoint_kind,
                created_at,
            })
        })
        .map_err(openproxy_db::error::map_db_error)?;

    collect_rows(rows, "recent_desc")
}

/// Read a single usage row by id as a `RecentUsageRow` (the broadcast
/// shape, with ALL fields including `cost_usd`, `stop_reason`, and
/// compression stats). Used by `mark_client_response` to re-broadcast
/// the row after the `client_response` UPDATE — the re-broadcast MUST
/// carry all fields so the frontend's null-skipping merge preserves
/// the enriched data (cost, stop_reason, compression) from the original
/// broadcast. The previous code used `detail_by_id` which doesn't
/// select `cost_usd` / `stop_reason` / `compression_*` — the
/// re-broadcast hard-coded them to `None`, which could clobber the
/// dashboard's data if the frontend's merge didn't skip nulls
/// correctly.
pub fn row_for_broadcast_by_id(
    conn: &Connection,
    id: i64,
) -> Result<Option<openproxy_types::usage::RecentUsageRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, request_id, trace_id, provider_id, upstream_model_id, \
                    status_code, total_ms, prompt_tokens, completion_tokens, \
                    cost_usd, connect_ms, ttft_ms, request_body_json, response_body_json, \
                    request_headers, response_headers, error_msg_redacted, error_msg, \
                    race_total, race_attempts, is_streaming, stream_complete, \
                    race_lost, created_at, stop_reason, \
                    compression_savings_pct, compression_techniques, \
                    client_response, prompt_tokens_estimated, completion_tokens_estimated, \
                    endpoint_kind, proxy_url, proxy_status, is_proxy_rotated \
             FROM usage \
             WHERE id = ?1",
        )
        .map_err(openproxy_db::error::map_db_error)?;

    let row = stmt
        .query_row(params![id], |row| {
            let mut col_idx = 0;
            let id: i64 = row.get(col_idx)?;
            col_idx += 1;
            let request_id: String = row.get(col_idx)?;
            col_idx += 1;
            let trace_id: String = row.get(col_idx)?;
            col_idx += 1;
            let provider_id: String = row.get(col_idx)?;
            col_idx += 1;
            let upstream_model_id: String = row.get(col_idx)?;
            col_idx += 1;
            let status_code: i64 = row.get(col_idx)?;
            col_idx += 1;
            let total_ms: i64 = row.get(col_idx)?;
            col_idx += 1;
            let prompt_tokens: Option<i64> = row.get(col_idx)?;
            col_idx += 1;
            let completion_tokens: Option<i64> = row.get(col_idx)?;
            col_idx += 1;
            let cost_usd: Option<f64> = row.get(col_idx)?;
            col_idx += 1;
            let connect_ms: Option<i64> = row.get(col_idx)?;
            col_idx += 1;
            let ttft_ms: Option<i64> = row.get(col_idx)?;
            col_idx += 1;
            let request_body_json: Option<serde_json::Value> = row
                .get::<_, Option<String>>(col_idx)?
                .and_then(|s| serde_json::from_str(&s).ok());
            col_idx += 1;
            let response_body_json: Option<serde_json::Value> = row
                .get::<_, Option<String>>(col_idx)?
                .and_then(|s| serde_json::from_str(&s).ok());
            col_idx += 1;
            let request_headers: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let response_headers: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let error_msg_redacted: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let error_msg: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let race_total: i64 = row.get(col_idx)?;
            col_idx += 1;
            let race_attempts: i64 = row.get(col_idx)?;
            col_idx += 1;
            let is_streaming: i64 = row.get(col_idx)?;
            col_idx += 1;
            let stream_complete: i64 = row.get(col_idx)?;
            col_idx += 1;
            let race_lost: i64 = row.get(col_idx)?;
            col_idx += 1;
            let created_at: String = row.get(col_idx)?;
            col_idx += 1;
            let stop_reason: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let compression_savings_pct: Option<f64> = row.get(col_idx)?;
            col_idx += 1;
            let compression_techniques: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let client_response: i64 = row.get(col_idx)?;
            col_idx += 1;
            let prompt_tokens_estimated: i64 = row.get(col_idx)?;
            col_idx += 1;
            let completion_tokens_estimated: i64 = row.get(col_idx)?;
            col_idx += 1;
            let endpoint_kind_str: String = row.get(col_idx)?;
            col_idx += 1;
            let proxy_url: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let proxy_status: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let is_proxy_rotated: i64 = row.get(col_idx)?;

            if !(0..=u16::MAX as i64).contains(&status_code) {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr(format!(
                        "status_code out of u16 range: {}",
                        status_code
                    ))),
                ));
            }
            let request_headers = request_headers.and_then(|s| serde_json::from_str(&s).ok());
            let response_headers = response_headers.and_then(|s| serde_json::from_str(&s).ok());
            let error_message = error_msg_redacted.or(error_msg);
            let prompt_tokens = prompt_tokens.and_then(|v| u32::try_from(v).ok());
            let completion_tokens = completion_tokens.and_then(|v| u32::try_from(v).ok());
            let race_total_u8 = u8::try_from(race_total).ok();
            let race_attempts_u8 = u8::try_from(race_attempts).ok();
            let is_streaming_bool = is_streaming != 0;
            let stream_complete_bool = stream_complete != 0;
            let endpoint_kind = match endpoint_kind_str.as_str() {
                "chat" => openproxy_types::endpoint::EndpointKind::Chat,
                "audio" => openproxy_types::endpoint::EndpointKind::Audio,
                "image" => openproxy_types::endpoint::EndpointKind::Image,
                "embedding" => openproxy_types::endpoint::EndpointKind::Embedding,
                "video" => openproxy_types::endpoint::EndpointKind::Video,
                _ => openproxy_types::endpoint::EndpointKind::Chat,
            };
            Ok(openproxy_types::usage::RecentUsageRow {
                id: UsageId(id),
                request_id,
                trace_id,
                provider_id: ProviderId::new(provider_id),
                upstream_model_id,
                status_code: status_code as u16,
                total_ms: total_ms as u64,
                prompt_tokens,
                completion_tokens,
                cost_usd,
                connect_ms: connect_ms.map(|v| v as u64),
                ttft_ms: ttft_ms.map(|v| v as u64),
                request_body_json,
                response_body_json,
                request_headers,
                response_headers,
                error_message,
                race_total: race_total_u8,
                race_attempts: race_attempts_u8,
                is_streaming: is_streaming_bool,
                stream_complete: stream_complete_bool,
                race_lost: race_lost != 0,
                stop_reason,
                compression_savings_pct,
                compression_techniques,
                client_response: client_response != 0,
                prompt_tokens_estimated: prompt_tokens_estimated != 0,
                completion_tokens_estimated: completion_tokens_estimated != 0,
                proxy_url,
                proxy_status,
                is_proxy_rotated: is_proxy_rotated != 0,
                endpoint_kind,
                created_at,
            })
        })
        .map(Some);

    match row {
        Ok(r) => Ok(r),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(openproxy_db::error::map_db_error_ctx(
            "query row_for_broadcast_by_id",
        )(e)),
    }
}

/// Return one full `usage` row by id.
pub fn detail_by_id(conn: &Connection, id: i64) -> Result<Option<UsageDetailRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, request_id, trace_id, attempt, provider_id, account_id, \
                    combo_id, combo_target_id, model_row_id, upstream_model_id, \
                    prompt_tokens, completion_tokens, connect_ms, ttft_ms, \
                    total_ms, tokens_per_sec, status_code, error_msg, \
                    error_msg_redacted, race_total, race_attempts, race_lost, \
                    api_key_id, created_at, is_streaming, stream_complete, \
                    request_body_json, response_body_json, request_headers, \
                    response_headers, error_message, client_response, \
                    prompt_tokens_estimated, completion_tokens_estimated, \
                    endpoint_kind, proxy_url, proxy_status, is_proxy_rotated \
             FROM usage \
             WHERE id = ?1",
        )
        .map_err(openproxy_db::error::map_db_error)?;

    let row = stmt
        .query_row(params![id], |row| {
            let id: i64 = row.get(0)?;
            let request_id: String = row.get(1)?;
            let trace_id: String = row.get(2)?;
            let attempt: i64 = row.get(3)?;
            let provider_id: String = row.get(4)?;
            let account_id: Option<i64> = row.get(5)?;
            let combo_id: Option<i64> = row.get(6)?;
            let combo_target_id: Option<i64> = row.get(7)?;
            let model_row_id: Option<i64> = row.get(8)?;
            let upstream_model_id: String = row.get(9)?;
            let prompt_tokens: Option<i64> = row.get(10)?;
            let completion_tokens: Option<i64> = row.get(11)?;
            let connect_ms: Option<i64> = row.get(12)?;
            let ttft_ms: Option<i64> = row.get(13)?;
            let total_ms: i64 = row.get(14)?;
            let tokens_per_sec: Option<f64> = row.get(15)?;
            let status_code: i64 = row.get(16)?;
            let error_msg: Option<String> = row.get(17)?;
            let error_msg_redacted: Option<String> = row.get(18)?;
            let race_total: i64 = row.get(19)?;
            let race_attempts: i64 = row.get(20)?;
            let race_lost: i64 = row.get(21)?;
            let api_key_id: Option<i64> = row.get(22)?;
            let created_at: String = row.get(23)?;
            let is_streaming: i64 = row.get(24)?;
            let stream_complete: i64 = row.get(25)?;
            let mut col_idx = 26;
            let request_body_json: Option<serde_json::Value> = row
                .get::<_, Option<String>>(col_idx)?
                .and_then(|s| serde_json::from_str(&s).ok());
            col_idx += 1;
            let response_body_json: Option<serde_json::Value> = row
                .get::<_, Option<String>>(col_idx)?
                .and_then(|s| serde_json::from_str(&s).ok());
            col_idx += 1;
            let request_headers: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let response_headers: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let error_message: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let client_response: i64 = row.get(col_idx)?;
            col_idx += 1;
            let prompt_tokens_estimated: i64 = row.get(col_idx)?;
            col_idx += 1;
            let completion_tokens_estimated: i64 = row.get(col_idx)?;
            col_idx += 1;
            let endpoint_kind_str: String = row.get(col_idx)?;
            col_idx += 1;
            let proxy_url: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let proxy_status: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let is_proxy_rotated: i64 = row.get(col_idx)?;

            if !(0..=u16::MAX as i64).contains(&status_code) {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    16,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr(format!(
                        "status_code out of u16 range: {}",
                        status_code
                    ))),
                ));
            }
            if total_ms < 0 {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    14,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr(format!(
                        "total_ms unexpectedly negative: {}",
                        total_ms
                    ))),
                ));
            }
            let request_headers = request_headers.and_then(|s| serde_json::from_str(&s).ok());
            let response_headers = response_headers.and_then(|s| serde_json::from_str(&s).ok());
            let endpoint_kind = match endpoint_kind_str.as_str() {
                "chat" => openproxy_types::endpoint::EndpointKind::Chat,
                "audio" => openproxy_types::endpoint::EndpointKind::Audio,
                "image" => openproxy_types::endpoint::EndpointKind::Image,
                "embedding" => openproxy_types::endpoint::EndpointKind::Embedding,
                "video" => openproxy_types::endpoint::EndpointKind::Video,
                _ => openproxy_types::endpoint::EndpointKind::Chat,
            };

            Ok(UsageDetailRow {
                id: UsageId(id),
                request_id,
                trace_id,
                attempt,
                provider_id: ProviderId::new(provider_id),
                account_id: account_id.map(AccountId),
                combo_id: combo_id.map(ComboId),
                combo_target_id: combo_target_id.map(ComboTargetId),
                model_row_id: model_row_id.map(ModelRowId),
                upstream_model_id,
                prompt_tokens,
                completion_tokens,
                connect_ms,
                ttft_ms,
                total_ms,
                tokens_per_sec,
                status_code: status_code as u16,
                error_msg,
                error_msg_redacted,
                request_body_json,
                response_body_json,
                request_headers,
                response_headers,
                race_total,
                race_attempts,
                race_lost: race_lost != 0,
                is_streaming: is_streaming != 0,
                stream_complete: stream_complete != 0,
                created_at,
                api_key_id: api_key_id.map(ApiKeyId),
                error_message,
                client_response: client_response != 0,
                prompt_tokens_estimated: prompt_tokens_estimated != 0,
                completion_tokens_estimated: completion_tokens_estimated != 0,
                proxy_url,
                proxy_status,
                is_proxy_rotated: is_proxy_rotated != 0,
                endpoint_kind,
            })
        })
        .optional()
        .map_err(openproxy_db::error::map_db_error)?;

    Ok(row)
}

/// Return one full `usage` row by trace_id.
pub fn detail_by_trace_id(conn: &Connection, trace_id: &str) -> Result<Option<UsageDetailRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, request_id, trace_id, attempt, provider_id, account_id, \
                    combo_id, combo_target_id, model_row_id, upstream_model_id, \
                    prompt_tokens, completion_tokens, connect_ms, ttft_ms, \
                    total_ms, tokens_per_sec, status_code, error_msg, \
                    error_msg_redacted, race_total, race_attempts, race_lost, \
                    api_key_id, created_at, is_streaming, stream_complete, \
                    request_body_json, response_body_json, request_headers, \
                    response_headers, error_message, client_response, \
                    prompt_tokens_estimated, completion_tokens_estimated, \
                    endpoint_kind, proxy_url, proxy_status, is_proxy_rotated \
             FROM usage \
             WHERE trace_id = ?1",
        )
        .map_err(openproxy_db::error::map_db_error)?;

    let row = stmt
        .query_row(params![trace_id], |row| {
            let id: i64 = row.get(0)?;
            let request_id: String = row.get(1)?;
            let trace_id: String = row.get(2)?;
            let attempt: i64 = row.get(3)?;
            let provider_id: String = row.get(4)?;
            let account_id: Option<i64> = row.get(5)?;
            let combo_id: Option<i64> = row.get(6)?;
            let combo_target_id: Option<i64> = row.get(7)?;
            let model_row_id: Option<i64> = row.get(8)?;
            let upstream_model_id: String = row.get(9)?;
            let prompt_tokens: Option<i64> = row.get(10)?;
            let completion_tokens: Option<i64> = row.get(11)?;
            let connect_ms: Option<i64> = row.get(12)?;
            let ttft_ms: Option<i64> = row.get(13)?;
            let total_ms: i64 = row.get(14)?;
            let tokens_per_sec: Option<f64> = row.get(15)?;
            let status_code: i64 = row.get(16)?;
            let error_msg: Option<String> = row.get(17)?;
            let error_msg_redacted: Option<String> = row.get(18)?;
            let race_total: i64 = row.get(19)?;
            let race_attempts: i64 = row.get(20)?;
            let race_lost: i64 = row.get(21)?;
            let api_key_id: Option<i64> = row.get(22)?;
            let created_at: String = row.get(23)?;
            let is_streaming: i64 = row.get(24)?;
            let stream_complete: i64 = row.get(25)?;
            let mut col_idx = 26;
            let request_body_json: Option<serde_json::Value> = row
                .get::<_, Option<String>>(col_idx)?
                .and_then(|s| serde_json::from_str(&s).ok());
            col_idx += 1;
            let response_body_json: Option<serde_json::Value> = row
                .get::<_, Option<String>>(col_idx)?
                .and_then(|s| serde_json::from_str(&s).ok());
            col_idx += 1;
            let request_headers: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let response_headers: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let error_message: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let client_response: i64 = row.get(col_idx)?;
            col_idx += 1;
            let prompt_tokens_estimated: i64 = row.get(col_idx)?;
            col_idx += 1;
            let completion_tokens_estimated: i64 = row.get(col_idx)?;
            col_idx += 1;
            let endpoint_kind_str: String = row.get(col_idx)?;
            col_idx += 1;
            let proxy_url: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let proxy_status: Option<String> = row.get(col_idx)?;
            col_idx += 1;
            let is_proxy_rotated: i64 = row.get(col_idx)?;

            if !(0..=u16::MAX as i64).contains(&status_code) {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    16,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr(format!(
                        "status_code out of u16 range: {}",
                        status_code
                    ))),
                ));
            }
            if total_ms < 0 {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    14,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr(format!(
                        "total_ms unexpectedly negative: {}",
                        total_ms
                    ))),
                ));
            }
            let request_headers = request_headers.and_then(|s| serde_json::from_str(&s).ok());
            let response_headers = response_headers.and_then(|s| serde_json::from_str(&s).ok());
            let endpoint_kind = match endpoint_kind_str.as_str() {
                "chat" => openproxy_types::endpoint::EndpointKind::Chat,
                "audio" => openproxy_types::endpoint::EndpointKind::Audio,
                "image" => openproxy_types::endpoint::EndpointKind::Image,
                "embedding" => openproxy_types::endpoint::EndpointKind::Embedding,
                "video" => openproxy_types::endpoint::EndpointKind::Video,
                _ => openproxy_types::endpoint::EndpointKind::Chat,
            };

            Ok(UsageDetailRow {
                id: UsageId(id),
                request_id,
                trace_id,
                attempt,
                provider_id: ProviderId::new(provider_id),
                account_id: account_id.map(AccountId),
                combo_id: combo_id.map(ComboId),
                combo_target_id: combo_target_id.map(ComboTargetId),
                model_row_id: model_row_id.map(ModelRowId),
                upstream_model_id,
                prompt_tokens,
                completion_tokens,
                connect_ms,
                ttft_ms,
                total_ms,
                tokens_per_sec,
                status_code: status_code as u16,
                error_msg,
                error_msg_redacted,
                request_body_json,
                response_body_json,
                request_headers,
                response_headers,
                race_total,
                race_attempts,
                race_lost: race_lost != 0,
                is_streaming: is_streaming != 0,
                stream_complete: stream_complete != 0,
                created_at,
                api_key_id: api_key_id.map(ApiKeyId),
                error_message,
                client_response: client_response != 0,
                prompt_tokens_estimated: prompt_tokens_estimated != 0,
                completion_tokens_estimated: completion_tokens_estimated != 0,
                proxy_url,
                proxy_status,
                is_proxy_rotated: is_proxy_rotated != 0,
                endpoint_kind,
            })
        })
        .optional()
        .map_err(openproxy_db::error::map_db_error)?;

    Ok(row)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Drain a `MappedRows` iterator into a `Vec`, converting each row's rusqlite
/// error into a `CoreError::Database` with a query-specific message.
fn collect_rows<T>(
    iter: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
    query_name: &'static str,
) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for r in iter {
        out.push(r.map_err(openproxy_db::error::map_db_error_ctx(format!(
            "read {} row",
            query_name
        )))?);
    }
    Ok(out)
}

/// Coerce a non-negative `i64` (from `COUNT(*)` etc.) to `u64`, panicking on
/// the unreachable negative case with a clear field name.
fn as_u64(v: i64, field: &'static str) -> rusqlite::Result<u64> {
    if v < 0 {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(SimpleErr(format!("{} unexpectedly negative: {}", field, v))),
        ));
    }
    Ok(v as u64)
}

/// Tiny `std::error::Error` shim for ad-hoc conversion errors.
#[derive(Debug)]
struct SimpleErr(String);
impl std::fmt::Display for SimpleErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SimpleErr {}

// ---------------------------------------------------------------------------
// Recording TTL cleanup
// ---------------------------------------------------------------------------

pub fn prune_expired_recording_bodies(
    conn: &rusqlite::Connection,
    ttl_secs: i64,
) -> crate::error::Result<usize> {
    let ttl_secs = ttl_secs.max(0);
    let n = conn
        .execute(
            "UPDATE usage \
             SET request_body_json = NULL, \
                 response_body_json = NULL, \
                 request_headers = NULL, \
                 response_headers = NULL \
             WHERE datetime(created_at) <= datetime(?1, ?2)",
            rusqlite::params![
                chrono::Utc::now().to_rfc3339(),
                format!("-{} seconds", ttl_secs)
            ],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    Ok(n)
}

pub fn prune_expired_usage_rows(
    conn: &rusqlite::Connection,
    ttl_days: i64,
) -> crate::error::Result<usize> {
    let ttl_days = ttl_days.max(0);
    let n = conn
        .execute(
            "DELETE FROM usage \
             WHERE datetime(created_at) <= datetime(?1, ?2)",
            rusqlite::params![
                chrono::Utc::now().to_rfc3339(),
                format!("-{} days", ttl_days)
            ],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    Ok(n)
}
