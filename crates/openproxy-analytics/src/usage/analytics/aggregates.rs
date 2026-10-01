//! Aggregation queries: summary, by_model, by_provider, monthly, by_day, by_account, by_status, errors.

use openproxy_types::Result;
use openproxy_types::ids::{AccountId, ProviderId};
use rusqlite::{Connection, Row, params_from_iter};
use serde::{Deserialize, Serialize};

use super::filter::{BuiltWhere, UsageFilter, as_u64, collect_rows, get_u64_col, to_params};

/// Aggregate roll-up of a filtered set of usage rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSummary {
    pub unique_requests: u64,
    pub total_rows: u64,
    pub total_attempts: u64,
    pub winners: u64,
    pub losers: u64,
    pub errors: u64,
    pub total_prompt_tokens: i64,
    pub total_completion_tokens: i64,
    pub total_cached_tokens: i64,
    pub total_cost_usd: f64,
    pub avg_ttft_ms: Option<f64>,
    pub avg_total_ms: f64,
    pub avg_success_connect_ms: Option<f64>,
    pub avg_success_ttft_ms: Option<f64>,
    pub avg_success_total_ms: Option<f64>,
    pub rows_with_null_pricing: u64,
    pub avg_compression_savings_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ByModelRow {
    pub provider_id: ProviderId,
    pub upstream_model_id: String,
    pub unique_requests: u64,
    pub total_rows: u64,
    pub winners: u64,
    pub total_prompt_tokens: i64,
    pub total_completion_tokens: i64,
    pub total_cached_tokens: i64,
    pub total_cost_usd: f64,
    pub avg_compression_savings_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ByProviderRow {
    pub provider_id: String,
    pub unique_requests: u64,
    pub total_rows: u64,
    pub winners: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_cached_tokens: u64,
    pub total_cost_usd: f64,
    pub avg_compression_savings_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MonthlyByProviderRow {
    pub provider_id: String,
    pub month: String,
    pub unique_requests: u64,
    pub total_rows: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_cost_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ByDayRow {
    pub date: String,
    pub unique_requests: u64,
    pub total_rows: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_cost_usd: f64,
    pub errors: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ByAccountRow {
    pub account_id: AccountId,
    pub provider_id: ProviderId,
    pub unique_requests: u64,
    pub total_rows: u64,
    pub errors: u64,
    pub total_cost_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ByStatusRow {
    pub status_code: u16,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorRow {
    pub request_id: String,
    pub trace_id: String,
    pub provider_id: ProviderId,
    pub upstream_model_id: String,
    pub status_code: u16,
    pub error_msg_redacted: Option<String>,
    pub created_at: String,
}

fn row_to_usage_summary(row: &Row<'_>) -> rusqlite::Result<UsageSummary> {
    Ok(UsageSummary {
        unique_requests: as_u64(row.get(0)?, "unique_requests")?,
        total_rows: as_u64(row.get(1)?, "total_rows")?,
        total_attempts: as_u64(row.get(2)?, "total_attempts")?,
        winners: as_u64(row.get::<_, Option<i64>>(3)?.unwrap_or(0), "winners")?,
        losers: as_u64(row.get::<_, Option<i64>>(4)?.unwrap_or(0), "losers")?,
        errors: as_u64(row.get::<_, Option<i64>>(5)?.unwrap_or(0), "errors")?,
        total_prompt_tokens: row.get(6)?,
        total_completion_tokens: row.get(7)?,
        total_cached_tokens: row.get(8)?,
        total_cost_usd: row.get(9)?,
        avg_ttft_ms: row.get(10)?,
        avg_total_ms: row.get(11)?,
        rows_with_null_pricing: as_u64(
            row.get::<_, Option<i64>>(12)?.unwrap_or(0),
            "rows_with_null_pricing",
        )?,
        avg_success_connect_ms: row.get(13)?,
        avg_success_ttft_ms: row.get(14)?,
        avg_success_total_ms: row.get(15)?,
        avg_compression_savings_pct: row.get(16)?,
    })
}

pub fn summary(conn: &Connection, f: &UsageFilter) -> Result<UsageSummary> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT \
             COUNT(DISTINCT request_id) AS unique_requests, \
             COUNT(*) AS total_rows, \
             COUNT(DISTINCT trace_id) AS total_attempts, \
             SUM(CASE WHEN status_code >= 200 AND status_code < 400 AND race_lost = 0 THEN 1 ELSE 0 END) AS winners, \
             SUM(CASE WHEN race_lost = 1 THEN 1 ELSE 0 END) AS losers, \
             SUM(CASE WHEN status_code >= 400 OR (status_code = 0 AND (error_msg IS NULL OR error_msg != 'predict_skipped')) THEN 1 ELSE 0 END) AS errors, \
             COALESCE(SUM(CASE WHEN status_code >= 200 AND status_code < 400 THEN prompt_tokens ELSE 0 END), 0) AS total_prompt_tokens, \
             COALESCE(SUM(CASE WHEN status_code >= 200 AND status_code < 400 THEN completion_tokens ELSE 0 END), 0) AS total_completion_tokens, \
             COALESCE(SUM(CASE WHEN status_code >= 200 AND status_code < 400 THEN cached_tokens ELSE 0 END), 0) AS total_cached_tokens, \
             COALESCE(SUM(CASE WHEN status_code >= 200 AND status_code < 400 THEN cost_usd ELSE 0.0 END), 0.0) AS total_cost_usd, \
             AVG(ttft_ms) FILTER (WHERE status_code >= 200 AND status_code < 400 AND ttft_ms IS NOT NULL) AS avg_ttft_ms, \
             COALESCE(AVG(total_ms) FILTER (WHERE error_msg IS NULL OR error_msg != 'predict_skipped'), 0.0) AS avg_total_ms, \
             SUM(CASE WHEN cost_usd IS NULL AND prompt_tokens > 0 AND status_code >= 200 AND status_code < 400 THEN 1 ELSE 0 END) AS rows_with_null_pricing, \
             AVG(connect_ms) FILTER (WHERE status_code >= 200 AND status_code < 400 AND connect_ms IS NOT NULL) AS avg_success_connect_ms, \
             AVG(ttft_ms) FILTER (WHERE status_code >= 200 AND status_code < 400 AND ttft_ms IS NOT NULL) AS avg_success_ttft_ms, \
             AVG(total_ms) FILTER (WHERE status_code >= 200 AND status_code < 400 AND total_ms IS NOT NULL) AS avg_success_total_ms, \
             AVG(compression_savings_pct) FILTER (WHERE status_code >= 200 AND status_code < 400) AS avg_compression_savings_pct \
         FROM usage {}",
        w.sql,
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(openproxy_db::error::map_db_error)?;

    let params_slice = to_params(&w.params);
    let summary = stmt
        .query_row(params_from_iter(params_slice), row_to_usage_summary)
        .map_err(openproxy_db::error::map_db_error)?;

    Ok(summary)
}

fn row_to_by_model(row: &Row<'_>) -> rusqlite::Result<ByModelRow> {
    Ok(ByModelRow {
        provider_id: ProviderId::new(row.get::<_, String>(0)?),
        upstream_model_id: row.get(1)?,
        unique_requests: get_u64_col(row, 2, "unique_requests")?,
        total_rows: get_u64_col(row, 3, "total_rows")?,
        winners: get_u64_col(row, 4, "winners")?,
        total_prompt_tokens: row.get(5)?,
        total_completion_tokens: row.get(6)?,
        total_cached_tokens: row.get(7)?,
        total_cost_usd: row.get(8)?,
        avg_compression_savings_pct: row.get(9)?,
    })
}

pub fn by_model(conn: &Connection, f: &UsageFilter) -> Result<Vec<ByModelRow>> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT \
             provider_id, \
             upstream_model_id, \
             COUNT(DISTINCT request_id) AS unique_requests, \
             COUNT(*) AS total_rows, \
             SUM(CASE WHEN status_code >= 200 AND status_code < 400 AND race_lost = 0 THEN 1 ELSE 0 END) AS winners, \
             COALESCE(SUM(CASE WHEN status_code >= 200 AND status_code < 400 THEN prompt_tokens ELSE 0 END), 0) AS total_prompt_tokens, \
             COALESCE(SUM(CASE WHEN status_code >= 200 AND status_code < 400 THEN completion_tokens ELSE 0 END), 0) AS total_completion_tokens, \
             COALESCE(SUM(CASE WHEN status_code >= 200 AND status_code < 400 THEN cached_tokens ELSE 0 END), 0) AS total_cached_tokens, \
             COALESCE(SUM(CASE WHEN status_code >= 200 AND status_code < 400 THEN cost_usd ELSE 0.0 END), 0.0) AS total_cost_usd, \
             AVG(compression_savings_pct) FILTER (WHERE status_code >= 200 AND status_code < 400) AS avg_compression_savings_pct \
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
        .query_map(params_from_iter(params_slice), row_to_by_model)
        .map_err(openproxy_db::error::map_db_error)?;

    collect_rows(rows, "by_model")
}

fn row_to_by_provider(row: &Row<'_>) -> rusqlite::Result<ByProviderRow> {
    Ok(ByProviderRow {
        provider_id: row.get(0)?,
        unique_requests: get_u64_col(row, 1, "unique_requests")?,
        total_rows: get_u64_col(row, 2, "total_rows")?,
        winners: get_u64_col(row, 3, "winners")?,
        total_prompt_tokens: get_u64_col(row, 4, "total_prompt_tokens")?,
        total_completion_tokens: get_u64_col(row, 5, "total_completion_tokens")?,
        total_cached_tokens: get_u64_col(row, 6, "total_cached_tokens")?,
        total_cost_usd: row.get(7)?,
        avg_compression_savings_pct: row.get(8)?,
    })
}

pub fn by_provider(conn: &Connection, f: &UsageFilter) -> Result<Vec<ByProviderRow>> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT \
             provider_id, \
             COUNT(DISTINCT request_id) AS unique_requests, \
             COUNT(*) AS total_rows, \
             SUM(CASE WHEN status_code >= 200 AND status_code < 400 AND race_lost = 0 THEN 1 ELSE 0 END) AS winners, \
             COALESCE(SUM(CASE WHEN status_code >= 200 AND status_code < 400 THEN prompt_tokens ELSE 0 END), 0) AS total_prompt_tokens, \
             COALESCE(SUM(CASE WHEN status_code >= 200 AND status_code < 400 THEN completion_tokens ELSE 0 END), 0) AS total_completion_tokens, \
             COALESCE(SUM(CASE WHEN status_code >= 200 AND status_code < 400 THEN cached_tokens ELSE 0 END), 0) AS total_cached_tokens, \
             COALESCE(SUM(CASE WHEN status_code >= 200 AND status_code < 400 THEN cost_usd ELSE 0.0 END), 0.0) AS total_cost_usd, \
             AVG(compression_savings_pct) FILTER (WHERE status_code >= 200 AND status_code < 400) AS avg_compression_savings_pct \
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
        .query_map(params_from_iter(params_slice), row_to_by_provider)
        .map_err(openproxy_db::error::map_db_error)?;

    collect_rows(rows, "by_provider")
}

pub fn monthly_by_provider(
    conn: &Connection,
    f: &UsageFilter,
) -> Result<Vec<MonthlyByProviderRow>> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT \
             provider_id, \
             strftime('%Y-%m', created_at) AS month, \
             COUNT(DISTINCT request_id) AS unique_requests, \
             COUNT(*) AS total_rows, \
             COALESCE(SUM(CASE WHEN status_code >= 200 AND status_code < 400 THEN prompt_tokens ELSE 0 END), 0) AS total_prompt_tokens, \
             COALESCE(SUM(CASE WHEN status_code >= 200 AND status_code < 400 THEN completion_tokens ELSE 0 END), 0) AS total_completion_tokens, \
             COALESCE(SUM(CASE WHEN status_code >= 200 AND status_code < 400 THEN cost_usd ELSE 0.0 END), 0.0) AS total_cost_usd \
         FROM usage {} \
         GROUP BY provider_id, strftime('%Y-%m', created_at) \
         ORDER BY month ASC, total_cost_usd DESC, provider_id ASC",
        w.sql,
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(openproxy_db::error::map_db_error)?;

    let params_slice = to_params(&w.params);
    let rows = stmt
        .query_map(params_from_iter(params_slice), |row| {
            Ok(MonthlyByProviderRow {
                provider_id: row.get(0)?,
                month: row.get(1)?,
                unique_requests: get_u64_col(row, 2, "unique_requests")?,
                total_rows: get_u64_col(row, 3, "total_rows")?,
                total_prompt_tokens: get_u64_col(row, 4, "total_prompt_tokens")?,
                total_completion_tokens: get_u64_col(row, 5, "total_completion_tokens")?,
                total_cost_usd: row.get(6)?,
            })
        })
        .map_err(openproxy_db::error::map_db_error)?;

    collect_rows(rows, "monthly_by_provider")
}

pub fn by_day(conn: &Connection, f: &UsageFilter) -> Result<Vec<ByDayRow>> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT \
             strftime('%Y-%m-%d', created_at) AS date, \
             COUNT(DISTINCT request_id) AS unique_requests, \
             COUNT(*) AS total_rows, \
             COALESCE(SUM(CASE WHEN status_code >= 200 AND status_code < 400 THEN prompt_tokens ELSE 0 END), 0) AS total_prompt_tokens, \
             COALESCE(SUM(CASE WHEN status_code >= 200 AND status_code < 400 THEN completion_tokens ELSE 0 END), 0) AS total_completion_tokens, \
             COALESCE(SUM(CASE WHEN status_code >= 200 AND status_code < 400 THEN cost_usd ELSE 0.0 END), 0.0) AS total_cost_usd, \
             SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END) AS errors \
         FROM usage {} \
         GROUP BY strftime('%Y-%m-%d', created_at) \
         ORDER BY date ASC",
        w.sql,
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(openproxy_db::error::map_db_error)?;

    let params_slice = to_params(&w.params);
    let rows = stmt
        .query_map(params_from_iter(params_slice), |row| {
            Ok(ByDayRow {
                date: row.get(0)?,
                unique_requests: get_u64_col(row, 1, "unique_requests")?,
                total_rows: get_u64_col(row, 2, "total_rows")?,
                total_prompt_tokens: get_u64_col(row, 3, "total_prompt_tokens")?,
                total_completion_tokens: get_u64_col(row, 4, "total_completion_tokens")?,
                total_cost_usd: row.get(5)?,
                errors: as_u64(row.get::<_, Option<i64>>(6)?.unwrap_or(0), "errors")?,
            })
        })
        .map_err(openproxy_db::error::map_db_error)?;

    collect_rows(rows, "by_day")
}

pub fn by_account(conn: &Connection, f: &UsageFilter) -> Result<Vec<ByAccountRow>> {
    let w = BuiltWhere::from_filter(f);
    let where_clause = if w.sql.is_empty() {
        "WHERE account_id IS NOT NULL".to_string()
    } else {
        format!("{} AND account_id IS NOT NULL", w.sql)
    };

    let sql = format!(
        "SELECT \
             account_id, \
             provider_id, \
             COUNT(DISTINCT request_id) AS unique_requests, \
             COUNT(*) AS total_rows, \
             SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END) AS errors, \
             COALESCE(SUM(CASE WHEN status_code >= 200 AND status_code < 400 THEN cost_usd ELSE 0.0 END), 0.0) AS total_cost_usd \
         FROM usage {where_clause} \
         GROUP BY account_id, provider_id \
         ORDER BY total_cost_usd DESC, account_id ASC"
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(openproxy_db::error::map_db_error)?;

    let params_slice = to_params(&w.params);
    let rows = stmt
        .query_map(params_from_iter(params_slice), |row| {
            let aid: i64 = row.get(0)?;
            let pid: String = row.get(1)?;
            Ok(ByAccountRow {
                account_id: AccountId(aid),
                provider_id: ProviderId::new(pid),
                unique_requests: get_u64_col(row, 2, "unique_requests")?,
                total_rows: get_u64_col(row, 3, "total_rows")?,
                errors: as_u64(row.get::<_, Option<i64>>(4)?.unwrap_or(0), "errors")?,
                total_cost_usd: row.get(5)?,
            })
        })
        .map_err(openproxy_db::error::map_db_error)?;

    collect_rows(rows, "by_account")
}

pub fn by_status(conn: &Connection, f: &UsageFilter) -> Result<Vec<ByStatusRow>> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT status_code, COUNT(*) AS count \
         FROM usage {} \
         GROUP BY status_code \
         ORDER BY count DESC, status_code ASC",
        w.sql,
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(openproxy_db::error::map_db_error)?;

    let params_slice = to_params(&w.params);
    let rows = stmt
        .query_map(params_from_iter(params_slice), |row| {
            let status_code_i: i64 = row.get(0)?;
            let count: i64 = row.get(1)?;
            let status_code = u16::try_from(status_code_i).map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Integer,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "status_code out of u16 range",
                    )),
                )
            })?;
            Ok(ByStatusRow {
                status_code,
                count: as_u64(count, "count")?,
            })
        })
        .map_err(openproxy_db::error::map_db_error)?;

    collect_rows(rows, "by_status")
}

pub fn errors(conn: &Connection, f: &UsageFilter, limit: u32) -> Result<Vec<ErrorRow>> {
    let w = BuiltWhere::from_filter(f);
    let where_clause = if w.sql.is_empty() {
        "WHERE status_code >= 400".to_string()
    } else {
        format!("{} AND status_code >= 400", w.sql)
    };

    let limit_param: i64 = i64::from(limit);
    let sql = format!(
        "SELECT request_id, trace_id, provider_id, upstream_model_id, status_code, \
                error_msg_redacted, created_at \
         FROM usage {where_clause} \
         ORDER BY created_at DESC, id DESC \
         LIMIT ?"
    );

    let mut all_params: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(w.params.len() + 1);
    for p in w.params {
        all_params.push(p);
    }
    all_params.push(Box::new(limit_param));
    let params_slice = to_params(&all_params);

    let mut stmt = conn
        .prepare(&sql)
        .map_err(openproxy_db::error::map_db_error)?;

    let rows = stmt
        .query_map(params_from_iter(params_slice), |row| {
            let pid: String = row.get(2)?;
            let status_i: i64 = row.get(4)?;
            let status_code = u16::try_from(status_i).map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Integer,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "status_code out of u16 range",
                    )),
                )
            })?;
            Ok(ErrorRow {
                request_id: row.get(0)?,
                trace_id: row.get(1)?,
                provider_id: ProviderId::new(pid),
                upstream_model_id: row.get(3)?,
                status_code,
                error_msg_redacted: row.get(5)?,
                created_at: row.get(6)?,
            })
        })
        .map_err(openproxy_db::error::map_db_error)?;

    collect_rows(rows, "errors")
}
