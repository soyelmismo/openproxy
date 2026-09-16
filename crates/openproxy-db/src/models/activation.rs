use openproxy_types::{ModelRowId, ProviderId, Result};
use rusqlite::{Connection, OptionalExtension, params};

use super::model_auto_active_select;
use crate::error::{map_db_error, map_db_error_ctx};

pub fn set_active(conn: &Connection, id: ModelRowId, active: bool) -> Result<()> {
    if active {
        conn.execute(
            "UPDATE models SET active = 1, manually_disabled_at = NULL WHERE id = ?1",
            params![id.0],
        )
    } else {
        conn.execute(
            "UPDATE models SET active = 0, manually_disabled_at = datetime('now') WHERE id = ?1",
            params![id.0],
        )
    }
    .map_err(map_db_error)?;
    Ok(())
}

pub fn set_active_bulk(conn: &Connection, provider: &ProviderId, active: bool) -> Result<u64> {
    let rows = if active {
        conn.execute(
            "UPDATE models SET active = 1, manually_disabled_at = NULL \
             WHERE provider_id = ?1 AND custom = 0",
            params![provider.as_str()],
        )
    } else {
        conn.execute(
            "UPDATE models SET active = 0, manually_disabled_at = datetime('now') \
             WHERE provider_id = ?1 AND custom = 0",
            params![provider.as_str()],
        )
    }
    .map_err(map_db_error)?;
    Ok(rows as u64)
}

fn query_newly_active_models(
    tx: &rusqlite::Transaction,
    provider: &ProviderId,
    keyword: Option<&str>,
) -> Result<Vec<(String, Option<String>)>> {
    match keyword {
        Some(k) => crate::db_query_all!(
            tx,
            model_auto_active_select!(
                "WHERE provider_id = ?1 AND custom = 0 \
                 AND discovered_at >= datetime('now', '-60 seconds') \
                 AND active = 0 \
                 AND manually_disabled_at IS NULL \
                 AND model_id LIKE '%' || ?2 || '%'"
            ),
            params![provider.as_str(), k],
            |r| crate::map_row_tuple!(r => (0, 1)),
            "query newly active models with keyword"
        ),
        None => crate::db_query_all!(
            tx,
            model_auto_active_select!(
                "WHERE provider_id = ?1 AND custom = 0 \
                 AND discovered_at >= datetime('now', '-60 seconds') \
                 AND active = 0 \
                 AND manually_disabled_at IS NULL"
            ),
            params![provider.as_str()],
            |r| crate::map_row_tuple!(r => (0, 1)),
            "query newly active models"
        ),
    }
}

fn update_models_active_status(
    tx: &rusqlite::Transaction,
    provider: &ProviderId,
    keyword: Option<&str>,
) -> Result<usize> {
    match keyword {
        Some(k) => tx.execute(
            "UPDATE models \
              SET active = CASE WHEN model_id LIKE '%' || ?1 || '%' THEN 1 ELSE 0 END \
              WHERE provider_id = ?2 \
                AND custom = 0 \
                AND discovered_at >= datetime('now', '-60 seconds') \
                AND manually_disabled_at IS NULL",
            params![k, provider.as_str()],
        ),
        None => tx.execute(
            "UPDATE models SET active = 1 \
              WHERE provider_id = ?1 \
                AND custom = 0 \
                AND discovered_at >= datetime('now', '-60 seconds') \
                AND manually_disabled_at IS NULL",
            params![provider.as_str()],
        ),
    }
    .map_err(map_db_error_ctx(format!(
        "apply_auto_activation for {provider}"
    )))
}

pub(crate) fn notify_auto_activated_models(
    tx: &rusqlite::Transaction,
    provider: &ProviderId,
    keyword: Option<&str>,
    newly_active: &[(String, Option<String>)],
    notif_keyword_only: bool,
) -> Result<()> {
    let notifications_present: bool = tx
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'notifications'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n != 0);

    if !notifications_present || newly_active.is_empty() {
        return Ok(());
    }

    let candidates: Vec<&(String, Option<String>)> = match (notif_keyword_only, keyword) {
        (true, Some(k)) => {
            let mut keep: Vec<&(String, Option<String>)> = Vec::with_capacity(newly_active.len());
            for entry in newly_active {
                let matches: i64 = tx
                    .query_row(
                        "SELECT ?1 LIKE '%' || ?2 || '%'",
                        params![entry.0.as_str(), k],
                        |r| r.get(0),
                    )
                    .map_err(map_db_error)?;
                if matches != 0 {
                    keep.push(entry);
                }
            }
            keep
        }
        _ => newly_active.iter().collect(),
    };

    let already_notified: std::collections::HashSet<String> = {
        let mut stmt = tx
            .prepare(
                "SELECT dedup_key FROM notifications \
                 WHERE kind = 'model_auto_activated' AND provider_id = ?1 AND dedup_key IS NOT NULL",
            )
            .map_err(map_db_error)?;
        let rows = stmt
            .query_map(params![provider.as_str()], |r| r.get::<_, String>(0))
            .map_err(map_db_error)?;
        rows.filter_map(std::result::Result::ok).collect()
    };

    let to_notify: Vec<_> = candidates
        .into_iter()
        .filter(|(model_id, _)| {
            let dedup = format!("{}:{}:auto", provider.as_str(), model_id);
            !already_notified.contains(&dedup)
        })
        .collect();

    if !to_notify.is_empty() {
        let _ = crate::batch::batch_insert(
            tx,
            "INSERT OR IGNORE INTO",
            "notifications",
            &["kind", "payload_json", "dedup_key", "provider_id"],
            &to_notify,
            None,
            |(model_id, display_name), query_params| {
                let payload = serde_json::json!({
                    "provider_id": provider.as_str(),
                    "model_id": model_id,
                    "display_name": display_name,
                    "matched_keyword": keyword,
                });
                let dedup = format!("{}:{}:auto", provider.as_str(), model_id);

                query_params.push(rusqlite::types::Value::Text(
                    "model_auto_activated".to_string(),
                ));
                query_params.push(rusqlite::types::Value::Text(payload.to_string()));
                query_params.push(rusqlite::types::Value::Text(dedup));
                query_params.push(rusqlite::types::Value::Text(provider.as_str().to_string()));
            },
        );
    }
    Ok(())
}

fn provider_notif_keyword_only(conn: &Connection, provider: &ProviderId) -> Result<bool> {
    let flag: Option<i64> = conn
        .query_row(
            "SELECT notif_keyword_only FROM providers WHERE id = ?1",
            params![provider.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(map_db_error)?;
    Ok(flag.is_some_and(|v| v != 0))
}

pub fn apply_auto_activation(
    conn: &Connection,
    provider: &ProviderId,
    keyword: Option<&str>,
) -> Result<u64> {
    let notif_keyword_only = provider_notif_keyword_only(conn, provider)?;
    let tx = conn.unchecked_transaction().map_err(map_db_error)?;
    let newly_active = query_newly_active_models(&tx, provider, keyword)?;
    let updated = update_models_active_status(&tx, provider, keyword)?;
    notify_auto_activated_models(&tx, provider, keyword, &newly_active, notif_keyword_only)?;
    tx.commit().map_err(map_db_error)?;
    Ok(updated as u64)
}

pub use crate::error::BUSY_RETRY_DELAYS;

pub fn apply_auto_activation_with_retry(
    conn: &Connection,
    provider: &ProviderId,
    keyword: Option<&str>,
) -> Result<u64> {
    crate::error::with_busy_retry("apply_auto_activation", || {
        apply_auto_activation(conn, provider, keyword)
    })
}
