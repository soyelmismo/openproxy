//! Context window and target tree resolution for combos.

use openproxy_types::ProviderId;
use openproxy_types::combos::ComboTarget;
use openproxy_types::error::Result;
use openproxy_types::ids::{AccountId, ComboId};
use rusqlite::params;

use super::mapping::{
    account_healthy_ids_select, combo_context_window_select, combo_target_model_sub_select,
};
use super::targets::list_targets;

fn resolve_target_context_window(
    conn: &rusqlite::Connection,
    model_row_id: Option<i64>,
    sub_combo_id: Option<i64>,
    model_cw: Option<i64>,
    model_id: &str,
    visited: &mut Vec<openproxy_types::ComboId>,
    depth: u32,
) -> openproxy_types::error::Result<Option<i64>> {
    if let Some(sub_id) = sub_combo_id {
        compute_effective_context_window_recursive(
            conn,
            openproxy_types::ComboId(sub_id),
            visited,
            depth + 1,
        )
    } else if model_row_id.is_some() {
        let cw = model_cw.filter(|&c| c > 0).or_else(|| {
            if model_id.is_empty() {
                None
            } else {
                openproxy_types::infer_context_length(model_id)
            }
        });
        Ok(cw)
    } else {
        Ok(None)
    }
}

fn validate_recursion_depth(
    combo_id: ComboId,
    visited: &mut Vec<ComboId>,
    depth: u32,
) -> Result<()> {
    if depth > openproxy_types::MAX_SUB_COMBO_DEPTH {
        return Err(openproxy_types::error::CoreError::Validation(format!(
            "max sub-combo depth ({}) exceeded",
            openproxy_types::MAX_SUB_COMBO_DEPTH
        )));
    }
    if visited.contains(&combo_id) {
        return Err(openproxy_types::error::CoreError::Validation(format!(
            "cyclic combo detected at id {}",
            combo_id.0
        )));
    }
    visited.push(combo_id);
    Ok(())
}

fn extract_and_resolve_window(
    conn: &rusqlite::Connection,
    row: &rusqlite::Row,
    visited: &mut Vec<ComboId>,
    depth: u32,
) -> Result<Option<i64>> {
    let (model_row_id, sub_combo_id, model_cw, model_id): (
        Option<i64>,
        Option<i64>,
        Option<i64>,
        String,
    ) = crate::map_row_tuple!(row => (0, 1, 2, 3)).map_err(crate::error::map_db_error)?;
    resolve_target_context_window(
        conn,
        model_row_id,
        sub_combo_id,
        model_cw,
        &model_id,
        visited,
        depth,
    )
}

fn aggregate_target_context_windows(
    conn: &rusqlite::Connection,
    combo_id: ComboId,
    visited: &mut Vec<ComboId>,
    depth: u32,
) -> Result<Option<i64>> {
    let mut stmt = conn
        .prepare(combo_target_model_sub_select!(
            "WHERE ct.combo_id = ?1 \
             AND ct.active = 1 \
             AND (ct.sub_combo_id IS NOT NULL OR (COALESCE(p.active, 0) = 1 AND m.id IS NOT NULL AND m.active = 1)) \
             AND NOT (ct.model_row_id IS NULL AND ct.sub_combo_id IS NULL)"
        ))
        .map_err(crate::error::map_db_error)?;
    let mut rows = stmt
        .query(rusqlite::params![combo_id.0])
        .map_err(crate::error::map_db_error)?;

    let mut min_window: Option<i64> = None;
    while let Some(row) = rows.next().map_err(crate::error::map_db_error)? {
        if let Some(cw) =
            extract_and_resolve_window(conn, row, visited, depth)?.filter(|&cw| cw > 0)
        {
            min_window = Some(min_window.map_or(cw, |min| std::cmp::min(min, cw)));
        }
    }
    Ok(min_window)
}

fn compute_effective_context_window_recursive(
    conn: &rusqlite::Connection,
    combo_id: openproxy_types::ComboId,
    visited: &mut Vec<openproxy_types::ComboId>,
    depth: u32,
) -> openproxy_types::error::Result<Option<i64>> {
    validate_recursion_depth(combo_id, visited, depth)?;

    let cw: Option<i64> = conn
        .query_row(
            combo_context_window_select!("WHERE id = ?1"),
            rusqlite::params![combo_id.0],
            |row| row.get(0),
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "get context_window for combo {}",
            combo_id.0
        )))?;

    let natural = aggregate_target_context_windows(conn, combo_id, visited, depth)?;

    let res = match (cw, natural) {
        (Some(o), Some(n)) => Ok(Some(std::cmp::min(o, n))),
        (Some(o), None) => Ok(Some(o)),
        (None, n) => Ok(n),
    };

    visited.pop();
    res
}

pub fn compute_effective_context_window(
    conn: &rusqlite::Connection,
    combo_id: ComboId,
) -> Result<Option<i64>> {
    let mut visited = Vec::new();
    compute_effective_context_window_recursive(conn, combo_id, &mut visited, 0)
}

pub fn resolve_combo_to_targets(
    conn: &rusqlite::Connection,
    combo_id: ComboId,
    visited: &mut Vec<ComboId>,
    depth: u32,
) -> Result<Vec<ComboTarget>> {
    validate_recursion_depth(combo_id, visited, depth)?;

    let targets = list_targets(conn, combo_id)?;
    let mut flat = Vec::new();
    for t in targets {
        if let Some(sub_id) = t.sub_combo_id {
            let sub = resolve_combo_to_targets(conn, sub_id, visited, depth + 1)?;
            flat.extend(sub);
        } else {
            flat.push(t);
        }
    }
    visited.pop();
    Ok(flat)
}

fn query_healthy_account_ids(
    conn: &rusqlite::Connection,
    provider_id: &ProviderId,
    check_rate_limit: bool,
) -> Result<Vec<AccountId>> {
    let sql = if check_rate_limit {
        account_healthy_ids_select!(
            "WHERE provider_id = ?1 AND health_status = 'healthy' AND (rate_limited_until IS NULL OR datetime(rate_limited_until) <= datetime('now')) ORDER BY priority ASC, id ASC"
        )
    } else {
        account_healthy_ids_select!(
            "WHERE provider_id = ?1 AND health_status = 'healthy' ORDER BY priority ASC, id ASC"
        )
    };
    let mut stmt = conn.prepare(sql).map_err(crate::error::map_db_error)?;
    let rows = stmt
        .query_map(params![provider_id.as_str()], |r| r.get::<_, i64>(0))
        .map_err(crate::error::map_db_error)?;
    Ok(rows.flatten().map(AccountId).collect())
}

fn fetch_healthy_accounts(
    conn: &rusqlite::Connection,
    provider_id: &ProviderId,
) -> Result<Vec<AccountId>> {
    let accounts = query_healthy_account_ids(conn, provider_id, true)?;
    if accounts.is_empty() {
        query_healthy_account_ids(conn, provider_id, false)
    } else {
        Ok(accounts)
    }
}

fn expand_single_target_rotation(
    conn: &rusqlite::Connection,
    target: ComboTarget,
    out: &mut Vec<ComboTarget>,
) -> Result<()> {
    if target.account_id.is_some() || target.sub_combo_id.is_some() {
        out.push(target);
        return Ok(());
    }
    let healthy_accounts = fetch_healthy_accounts(conn, &target.provider_id)?;
    if healthy_accounts.is_empty() {
        out.push(target);
    } else {
        for acc_id in healthy_accounts {
            let mut ct = target.clone();
            ct.account_id = Some(acc_id);
            out.push(ct);
        }
    }
    Ok(())
}

pub fn expand_account_rotation(
    conn: &rusqlite::Connection,
    targets: Vec<ComboTarget>,
) -> Result<Vec<ComboTarget>> {
    let mut out = Vec::with_capacity(targets.len());
    for t in targets {
        expand_single_target_rotation(conn, t, &mut out)?;
    }
    Ok(out)
}
