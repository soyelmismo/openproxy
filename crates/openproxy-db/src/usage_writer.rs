//! Atomic usage/cooldown writes. Call only from a blocking thread and publish
//! returned rows after releasing the connection lock.

use openproxy_types::usage::{RecentUsageRow, UsageInput};
use openproxy_types::{ComboId, ComboTargetId, CooldownMode, Result, UsageId};
use rusqlite::Connection;

pub struct AttemptCooldown<'a> {
    pub target_id: ComboTargetId,
    pub combo_id: ComboId,
    pub error: Option<&'a str>,
    pub is_upstream_health_issue: bool,
    pub mode: CooldownMode,
    pub base_secs: u64,
    pub max_secs: u64,
    pub factor: u32,
}

pub fn record(
    conn: &mut Connection,
    input: &UsageInput,
    cooldown: Option<&AttemptCooldown<'_>>,
) -> Result<(UsageId, RecentUsageRow)> {
    crate::with_busy_retry("usage_writer::record", || {
        let transaction = conn.transaction().map_err(crate::error::map_db_error)?;
        let row = crate::cost::record_row(&transaction, input)?;
        if let Some(params) = cooldown.filter(|params| params.combo_id.0 != -1) {
            match params.error {
                None => crate::cooldowns::clear_cooldown(&transaction, params.target_id)?,
                Some(reason) if params.is_upstream_health_issue => {
                    crate::cooldowns::record_cooldown(
                        &transaction,
                        params.target_id,
                        reason,
                        params.mode,
                        params.base_secs,
                        params.max_secs,
                        params.factor,
                    )?;
                }
                Some(_) => {}
            }
        }
        transaction.commit().map_err(crate::error::map_db_error)?;
        Ok(row)
    })
}

#[cfg(test)]
mod tests;
