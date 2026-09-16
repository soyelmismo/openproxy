//! Auto-creation of combos for models available across multiple healthy providers.

use crate::error::Result;
use rusqlite::Connection;
use std::collections::HashMap;

fn fetch_candidate_normalized_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT m.model_id_normalized
             FROM models m
             JOIN accounts a ON a.provider_id = m.provider_id
             WHERE m.active = 1
               AND a.health_status = 'healthy'
               AND m.model_id_normalized IS NOT NULL
               AND m.model_id_normalized != ''
             GROUP BY m.model_id_normalized
             HAVING COUNT(DISTINCT m.provider_id) >= 2
             ORDER BY m.model_id_normalized",
        )
        .map_err(openproxy_db::error::map_db_error)?;

    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(openproxy_db::error::map_db_error)?;
    rows.map(|row| row.map_err(openproxy_db::error::map_db_error))
        .collect::<std::result::Result<Vec<_>, crate::error::CoreError>>()
}

type TargetDescriptor = (i64, String, i64);
type TargetsByNormIdMap = HashMap<String, Vec<TargetDescriptor>>;
type ComboTargetKey = (i64, i64, i64);
type ExistingTargetsAndMaxOrders = (std::collections::HashSet<ComboTargetKey>, HashMap<i64, i32>);

fn fetch_targets_by_norm_id(
    conn: &Connection,
    normalized_ids: &[String],
) -> Result<TargetsByNormIdMap> {
    if normalized_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut stmt = conn
        .prepare(
            "SELECT m.model_id_normalized, m.rowid, m.provider_id, a.id
             FROM models m
             JOIN accounts a ON a.provider_id = m.provider_id AND a.health_status = 'healthy'
             WHERE m.active = 1
               AND m.model_id_normalized IS NOT NULL
               AND m.model_id_normalized != ''
             ORDER BY m.model_id_normalized, m.provider_id",
        )
        .map_err(openproxy_db::error::map_db_error)?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?.unwrap_or(-1),
            ))
        })
        .map_err(openproxy_db::error::map_db_error)?;

    let mut map: TargetsByNormIdMap = HashMap::new();
    for row in rows {
        let (norm_id, row_id, provider_id, account_id) =
            row.map_err(openproxy_db::error::map_db_error)?;
        map.entry(norm_id)
            .or_default()
            .push((row_id, provider_id, account_id));
    }
    Ok(map)
}

fn fetch_existing_combos(
    conn: &Connection,
    combo_names: &std::collections::HashSet<String>,
) -> Result<HashMap<String, i64>> {
    if combo_names.is_empty() {
        return Ok(HashMap::new());
    }
    let mut stmt = conn
        .prepare("SELECT name, id FROM combos")
        .map_err(openproxy_db::error::map_db_error)?;
    let result = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(openproxy_db::error::map_db_error)?
        .filter_map(std::result::Result::ok)
        .filter(|(name, _)| combo_names.contains(name))
        .collect();
    Ok(result)
}

fn fetch_existing_targets_and_max_orders(
    conn: &Connection,
    combo_ids: &[i64],
) -> Result<ExistingTargetsAndMaxOrders> {
    if combo_ids.is_empty() {
        return Ok((std::collections::HashSet::new(), HashMap::new()));
    }
    let rows = openproxy_db::batch::query_in_chunks(
        conn,
        "SELECT combo_id, account_id, model_row_id FROM combo_targets WHERE combo_id IN ({})",
        combo_ids,
        openproxy_db::batch::DEFAULT_CHUNK_SIZE,
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<i64>>(1)?.unwrap_or(-1),
                row.get::<_, i64>(2)?,
            ))
        },
    )
    .map_err(openproxy_db::error::map_db_error)?;
    let existing_targets = rows.into_iter().collect();

    let order_rows = openproxy_db::batch::query_in_chunks(
        conn,
        "SELECT combo_id, MAX(priority_order) FROM combo_targets WHERE combo_id IN ({}) GROUP BY combo_id",
        combo_ids,
        openproxy_db::batch::DEFAULT_CHUNK_SIZE,
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i32>(1)?)),
    )
    .map_err(openproxy_db::error::map_db_error)?;
    let max_orders = order_rows.into_iter().collect();

    Ok((existing_targets, max_orders))
}

fn append_combo_targets(
    insert_target_stmt: &mut rusqlite::Statement,
    combo_id: i64,
    targets: &[(i64, String, i64)],
    existing_targets: &std::collections::HashSet<(i64, i64, i64)>,
    max_orders: &mut std::collections::HashMap<i64, i32>,
) -> Result<()> {
    for &(row_id, ref provider_id, account_id) in targets {
        if !existing_targets.contains(&(combo_id, account_id, row_id)) {
            let next_order = max_orders.get(&combo_id).copied().unwrap_or(-1) + 1;
            max_orders.insert(combo_id, next_order);
            insert_target_stmt
                .execute(rusqlite::params![
                    combo_id,
                    provider_id,
                    account_id,
                    row_id,
                    next_order
                ])
                .map_err(openproxy_db::error::map_db_error)?;
        }
    }
    Ok(())
}

fn ensure_combo_id(
    conn: &Connection,
    insert_combo_stmt: &mut rusqlite::Statement,
    combo_name: &str,
    target_count: usize,
    existing_id: Option<i64>,
    created_count: &mut usize,
) -> Result<i64> {
    if let Some(id) = existing_id {
        return Ok(id);
    }
    let race_size = (target_count as u8).min(3);
    insert_combo_stmt
        .execute(rusqlite::params![combo_name, race_size])
        .map_err(openproxy_db::error::map_db_error)?;
    *created_count += 1;
    Ok(conn.last_insert_rowid())
}

struct ComboSyncState<'a, 'stmt> {
    existing_combos: &'a std::collections::HashMap<String, i64>,
    existing_targets: &'a std::collections::HashSet<(i64, i64, i64)>,
    max_orders: &'a mut std::collections::HashMap<i64, i32>,
    insert_combo_stmt: &'a mut rusqlite::Statement<'stmt>,
    insert_target_stmt: &'a mut rusqlite::Statement<'stmt>,
    created: &'a mut usize,
}

fn sync_single_candidate_combo(
    conn: &Connection,
    norm_id: &str,
    targets: &[(i64, String, i64)],
    state: &mut ComboSyncState<'_, '_>,
) -> Result<()> {
    if targets.len() < 2 {
        return Ok(());
    }

    let combo_name = format!("auto:{norm_id}");
    let combo_id = ensure_combo_id(
        conn,
        state.insert_combo_stmt,
        &combo_name,
        targets.len(),
        state.existing_combos.get(&combo_name).copied(),
        state.created,
    )?;

    append_combo_targets(
        state.insert_target_stmt,
        combo_id,
        targets,
        state.existing_targets,
        state.max_orders,
    )
}

/// Auto-create combos for models that are active in ≥2 providers.
pub fn auto_create_combos(conn: &Connection) -> Result<usize> {
    let normalized_ids = fetch_candidate_normalized_ids(conn)?;
    let targets_by_norm_id = fetch_targets_by_norm_id(conn, &normalized_ids)?;

    let combo_names: std::collections::HashSet<String> = normalized_ids
        .iter()
        .map(|id| format!("auto:{id}"))
        .collect();

    let existing_combos = fetch_existing_combos(conn, &combo_names)?;
    let combo_ids: Vec<i64> = existing_combos.values().copied().collect();
    let (existing_targets, mut max_orders) =
        fetch_existing_targets_and_max_orders(conn, &combo_ids)?;

    let is_in_tx = !conn.is_autocommit();
    if !is_in_tx {
        conn.execute("BEGIN", ())
            .map_err(openproxy_db::error::map_db_error)?;
    }

    let mut created = 0usize;
    let mut insert_combo_stmt = conn
        .prepare("INSERT INTO combos (name, strategy, race_size) VALUES (?1, 'priority', ?2)")
        .map_err(openproxy_db::error::map_db_error)?;

    let mut insert_target_stmt = conn
        .prepare(
            "INSERT INTO combo_targets (combo_id, provider_id, account_id, model_row_id, priority_order) VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .map_err(openproxy_db::error::map_db_error)?;

    let mut state = ComboSyncState {
        existing_combos: &existing_combos,
        existing_targets: &existing_targets,
        max_orders: &mut max_orders,
        insert_combo_stmt: &mut insert_combo_stmt,
        insert_target_stmt: &mut insert_target_stmt,
        created: &mut created,
    };

    let empty_targets = Vec::new();
    for norm_id in &normalized_ids {
        let targets = targets_by_norm_id.get(norm_id).unwrap_or(&empty_targets);
        sync_single_candidate_combo(conn, norm_id, targets, &mut state)?;
    }

    if !is_in_tx {
        conn.execute("COMMIT", ())
            .map_err(openproxy_db::error::map_db_error)?;
    }

    Ok(created)
}
