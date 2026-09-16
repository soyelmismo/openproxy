//! Backfill of normalized model IDs and usage cost recomputations.

use crate::error::Result;
use rusqlite::Connection;

fn fetch_unnormalized_rows(conn: &Connection, table: &str) -> Result<Vec<(String, String)>> {
    let sql = match table {
        "models" => "SELECT provider_id, model_id FROM models WHERE model_id_normalized IS NULL",
        "model_capabilities_sync" => {
            "SELECT provider_id, model_id FROM model_capabilities_sync WHERE model_id_normalized IS NULL"
        }
        _ => unreachable!("invalid table"),
    };
    let mut stmt = conn
        .prepare(sql)
        .map_err(openproxy_db::error::map_db_error)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(openproxy_db::error::map_db_error)?;
    Ok(rows.filter_map(std::result::Result::ok).collect())
}

fn backfill_table_normalized(
    conn: &Connection,
    table: &str,
    rows: &[(String, String)],
) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut total = 0;
    for chunk in rows.chunks(900 / 3) {
        let vals = openproxy_db::batch::values_placeholders(chunk.len(), 3);
        let mut sql = String::with_capacity(160 + vals.len() + table.len() * 3);
        sql.push_str("WITH updates(provider_id, model_id, normalized) AS (VALUES ");
        sql.push_str(&vals);
        sql.push_str(") UPDATE ");
        sql.push_str(table);
        sql.push_str(" SET model_id_normalized = updates.normalized FROM updates WHERE ");
        sql.push_str(table);
        sql.push_str(".provider_id = updates.provider_id AND ");
        sql.push_str(table);
        sql.push_str(".model_id = updates.model_id");

        let mut norm_strings = Vec::with_capacity(chunk.len());
        for (_, model_id) in chunk {
            norm_strings.push(crate::model_normalize::normalize_model_id(model_id));
        }
        let mut params: Vec<&str> = Vec::with_capacity(chunk.len() * 3);
        for ((provider_id, model_id), normalized) in chunk.iter().zip(&norm_strings) {
            params.push(provider_id.as_str());
            params.push(model_id.as_str());
            params.push(normalized.as_str());
        }
        let count = conn
            .execute(&sql, rusqlite::params_from_iter(params))
            .map_err(openproxy_db::error::map_db_error)?;
        total += count;
    }
    Ok(total)
}

/// Backfill `model_id_normalized` for existing rows in both `models` and
/// `model_capabilities_sync` that have NULL.
pub fn backfill_model_id_normalized(conn: &Connection) -> Result<usize> {
    let model_rows = fetch_unnormalized_rows(conn, "models")?;
    let sync_rows = fetch_unnormalized_rows(conn, "model_capabilities_sync")?;

    let total = backfill_table_normalized(conn, "models", &model_rows)?
        + backfill_table_normalized(conn, "model_capabilities_sync", &sync_rows)?;

    if total > 0 {
        tracing::info!(
            total,
            models_backfilled = model_rows.len(),
            sync_backfilled = sync_rows.len(),
            "backfilled model_id_normalized for existing rows"
        );
    }

    Ok(total)
}

type UsageRow = (i64, String, String, Option<u32>, Option<u32>);

fn resolve_recompute_price(
    conn: &Connection,
    provider_id: &str,
    model_id: &str,
) -> Option<crate::pricing::Price> {
    let price = crate::pricing::lookup_with_db(conn, provider_id, model_id);
    match price {
        Some(p) if p.input_per_1m == 0.0 && p.output_per_1m == 0.0 => {
            let base_model = crate::model_normalize::normalize_model_id(model_id);
            let paid = crate::pricing::lookup_by_normalized(conn, &base_model)
                .filter(|p| p.input_per_1m > 0.0 || p.output_per_1m > 0.0);
            paid.or(Some(p))
        }
        other => other,
    }
}

fn compute_usage_row_cost(
    price: Option<crate::pricing::Price>,
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
) -> Option<f64> {
    let p = price?;
    let prompt = f64::from(prompt_tokens.unwrap_or(0));
    let completion = f64::from(completion_tokens.unwrap_or(0));
    let cost = p.input_per_1m * prompt / 1_000_000.0 + p.output_per_1m * completion / 1_000_000.0;
    if cost > 0.0 { Some(cost) } else { None }
}

/// Recompute `cost_usd` for usage rows that have `cost_usd = 0` AND `prompt_tokens > 0`.
pub fn recompute_costs(conn: &Connection) -> Result<usize> {
    let rows: Vec<UsageRow> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, provider_id, upstream_model_id, prompt_tokens, completion_tokens \
                 FROM usage \
                 WHERE cost_usd = 0.0 \
                   AND status_code >= 200 AND status_code < 400 \
                   AND (prompt_tokens > 0 OR completion_tokens > 0)",
            )
            .map_err(openproxy_db::error::map_db_error)?;
        let result = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<u32>>(3)?,
                    row.get::<_, Option<u32>>(4)?,
                ))
            })
            .map_err(openproxy_db::error::map_db_error)?;
        result.filter_map(std::result::Result::ok).collect()
    };

    let mut updated = 0usize;
    {
        let mut stmt = conn
            .prepare("UPDATE usage SET cost_usd = ?1 WHERE id = ?2")
            .map_err(openproxy_db::error::map_db_error)?;
        for (id, provider_id, model_id, prompt_tokens, completion_tokens) in &rows {
            let price = resolve_recompute_price(conn, provider_id, model_id);
            if let Some(cost) = compute_usage_row_cost(price, *prompt_tokens, *completion_tokens) {
                stmt.execute(rusqlite::params![cost, id])
                    .map_err(openproxy_db::error::map_db_error)?;
                updated += 1;
            }
        }
    }

    if updated > 0 {
        tracing::info!(
            updated,
            total_candidates = rows.len(),
            "recomputed cost_usd for previously-unpriced usage rows"
        );
    }

    Ok(updated)
}
