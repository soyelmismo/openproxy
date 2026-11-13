//! Atomic admin plan assignments. Paid orders and grants share one contract.
use super::{tiers, transaction, usd_to_units};
use openproxy_db::error::map_db_error;
use openproxy_types::{CoreError, Result, ids::ApiKeyId};
use rusqlite::{Connection, params};

/// Grant a plan once per live assignment, including 1:1 plans with no explicit
/// credit override. Re-saving an exhausted but unexpired grant does not refill
/// it. A purchased plan is not followed by an additional free admin grant.
///
/// # Errors
/// Invalid commerce, missing key or database failure.
pub fn grant_plan(conn: &Connection, key: ApiKeyId, tier: &tiers::CreditTier) -> Result<bool> {
    transaction(conn, |conn| {
        let credit = tier
            .credit_usd
            .or(tier.price_usd)
            .and_then(usd_to_units)
            .ok_or_else(|| {
                CoreError::Validation("assigned plan must define positive credit or price".into())
            })?;
        if !(1..=36_500).contains(&tier.ttl_days) {
            return Err(CoreError::Validation("invalid plan lifetime".into()));
        }
        if crate::api_keys::crud::get_by_id(conn, key)?.is_none() {
            return Err(CoreError::Validation("unknown plan target key".into()));
        }
        let live: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM credit_transactions t \
             LEFT JOIN credit_orders o ON o.id = t.order_id \
             WHERE t.api_key_id = ?1 AND COALESCE(t.tier_id, o.tier_id) = ?2 \
               AND (SELECT tier_id FROM api_keys WHERE id = ?1) = ?2 \
               AND t.kind IN ('grant', 'topup') AND t.status IN ('active', 'exhausted') \
               AND (t.expires_at IS NULL OR t.expires_at > datetime('now')))",
                params![key.0, tier.id],
                |r| r.get(0),
            )
            .map_err(map_db_error)?;
        tiers::apply_to_key_forced(conn, key, &tiers::tier_own_config(tier))?;
        tiers::set_key_tier(conn, key, Some(tier.id))?;
        if live {
            return Ok(false);
        }
        conn.execute(
            "UPDATE credit_transactions SET status = 'expired' \
            WHERE api_key_id = ?1 AND status = 'active'",
            [key.0],
        )
        .map_err(map_db_error)?;
        conn.execute(
            "INSERT INTO credit_transactions \
            (api_key_id, kind, amount_units, remaining_units, status, expires_at, tier_id) \
            VALUES (?1, 'grant', ?2, ?2, 'active', datetime('now', ?3), ?4)",
            params![key.0, credit, format!("+{} days", tier.ttl_days), tier.id],
        )
        .map_err(map_db_error)?;
        Ok(true)
    })
}
