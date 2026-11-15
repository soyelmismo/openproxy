use super::CreditTier;
use rusqlite::{Connection, params};

/// Seats taken for a plan: live customer keys on the tier + pending anonymous
/// mint orders targeting it. A pending checkout RESERVES its seat until it
/// settles (pays / expires / cancels), so two buyers holding QRs cannot both
/// take the last seat. Renewals and key-bound orders take no seat here: the
/// key is already counted, and its plan only moves when payment lands.
///
/// # Errors
/// Propagates DB failures.
pub fn seats_taken(conn: &Connection, tier_id: i64) -> openproxy_types::Result<i64> {
    // ROOT FIX (seat oversell, audit A1-02): an anonymous order whose window
    // expired can STILL be paid for CONFIRMATION_GRACE_SECS longer (the
    // matcher accepts `status IN ('pending','expired')` inside the grace so a
    // late confirmation never strands the buyer's money). Counting only
    // `pending` orders freed the seat while the right to buy it lived on, so
    // buyer B took the "free" seat and a late-paying buyer A oversold the
    // plan. The grace window here mirrors the matcher's exactly.
    let grace = format!(
        "+{} seconds",
        crate::credits::orders::CONFIRMATION_GRACE_SECS
    );
    conn.query_row(
        "SELECT \
               (SELECT COUNT(*) FROM api_keys k \
                 WHERE k.tier_id = ?1 AND k.kind = 'customer') \
             + (SELECT COUNT(*) FROM credit_orders o \
                 WHERE o.tier_id = ?1 AND o.api_key_id IS NULL \
                   AND (o.status = 'pending' \
                        OR (o.status = 'expired' \
                            AND datetime(o.expires_at, ?2) >= datetime('now'))))",
        params![tier_id, grace],
        |row| row.get(0),
    )
    .map_err(openproxy_db::error::map_db_error)
}

/// Seats still sellable: `None` = unlimited (stock 0). Never negative: a
/// stock cut below current occupancy reads 0 (sold out), not negative.
///
/// # Errors
/// Propagates DB failures.
pub fn seats_left(conn: &Connection, tier: &CreditTier) -> openproxy_types::Result<Option<i64>> {
    if tier.stock <= 0 {
        return Ok(None);
    }
    let taken = seats_taken(conn, tier.id)?;
    Ok(Some((tier.stock - taken).max(0)))
}
