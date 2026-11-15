//! Combo pricing resolution: flat rates, time-of-day schedules, and the
//! per-request snapshot every billing and admission path shares.
//!
//! Three rules shape this module:
//!
//! 1. **A combo's own price wins.** A flat pair or a schedule, whichever the
//!    combo carries, is the bill for every request routed through it; the
//!    member models' rates never blend in.
//! 2. **One instant per decision.** A schedule's band is a function of time, so
//!    every price is resolved *at* an instant and never "now" by accident: the
//!    billing path passes the instant it bills for, admission asks for the
//!    upper bound over both bands, and re-pricing history passes the stored
//!    usage timestamp.
//! 3. **A broken schedule fails closed.** A combo whose persisted schedule no
//!    longer parses is neither "unpriced" nor "flat": its requests are refused
//!    (admission) or recorded with `cost_usd = NULL` (billing) rather than
//!    charged a tariff the operator never agreed to.

use chrono::{DateTime, Utc};
use openproxy_types::pricing_schedule::{PricingRate, PricingSchedule};
use rusqlite::Connection;

use super::{Price, lookup_with_db};

/// The combo's flat list price, when the operator set one.
///
/// A flat combo price is a *pair*: both input and output rates or neither
/// (`update_combo_pricing` enforces it), so a half-priced row never leaks a
/// partial bill. The cached-input rate is optional and hangs off that pair: it
/// only exists while the pair does.
///
/// This is the **legacy** accessor: it ignores [`PricingSchedule`] entirely, so
/// it keeps reporting the flat pair for metadata surfaces (public plan pricing).
/// Anything that bills a request uses [`lookup_for_request_at`] — or
/// [`lookup_for_request_conservative`] to reserve credits — so a combo that only
/// carries a schedule is still priced.
pub fn lookup_combo_price(conn: &Connection, combo_id: i64) -> Option<Price> {
    combo_pricing_row(conn, combo_id)?.flat_price()
}

/// Every pricing column a combo can carry, in one read.
struct ComboPricingRow {
    input_per_1m: Option<f64>,
    output_per_1m: Option<f64>,
    cached_per_1m: Option<f64>,
    schedule: ComboSchedule,
}

/// What the `pricing_schedule` column says, including "says something broken".
///
/// A persisted schedule that no longer parses is **not** the same as no
/// schedule: the combo was sold on a calendar, and falling back to the flat
/// price (or worse, to the upstream model's rate) would bill a tariff the
/// operator never agreed to. [`ComboSchedule::Unreadable`] therefore fails
/// closed.
enum ComboSchedule {
    Absent,
    Valid(PricingSchedule),
    Unreadable,
}

/// Outcome of resolving one combo price.
enum ComboPrice {
    Priced(Price),
    /// The combo carries no price at all: the caller may fall back to the
    /// target model's rate.
    Unpriced,
    /// The combo carries a price that cannot be read, or a schedule whose band
    /// needs an instant we do not have. No fallback is allowed.
    Unresolvable,
}

impl ComboPricingRow {
    /// The flat `pricing_*` trio as a [`Price`], or `None` when the pair is
    /// incomplete (never bill one side of a price).
    fn flat_price(&self) -> Option<Price> {
        let (Some(input_per_1m), Some(output_per_1m)) = (self.input_per_1m, self.output_per_1m)
        else {
            return None;
        };
        Some(Price {
            input_per_1m,
            output_per_1m,
            cached_per_1m: self.cached_per_1m,
            ..Default::default()
        })
    }

    /// The price that governs `at`. A schedule always wins, even when the flat
    /// columns are unset: the schedule *is* the combo's price.
    fn resolve_at(&self, at: DateTime<Utc>) -> ComboPrice {
        match &self.schedule {
            ComboSchedule::Valid(schedule) => {
                ComboPrice::Priced(price_of_rate(schedule.rate_at(at)))
            }
            ComboSchedule::Unreadable => ComboPrice::Unresolvable,
            ComboSchedule::Absent => self.flat_or_unpriced(),
        }
    }

    /// Upper bound over both bands, for admission checks that run before the
    /// billing instant is known.
    fn resolve_conservative(&self) -> ComboPrice {
        match &self.schedule {
            ComboSchedule::Valid(schedule) => {
                ComboPrice::Priced(price_of_rate(&schedule.conservative_rate()))
            }
            ComboSchedule::Unreadable => ComboPrice::Unresolvable,
            ComboSchedule::Absent => self.flat_or_unpriced(),
        }
    }

    /// Price for a row already billed. A scheduled row needs the instant it was
    /// billed at; without it the row cannot be re-priced and must be left alone.
    fn resolve_historical(&self, billed_at: Option<DateTime<Utc>>) -> ComboPrice {
        match &self.schedule {
            ComboSchedule::Valid(schedule) => match billed_at {
                Some(at) => ComboPrice::Priced(price_of_rate(schedule.rate_at(at))),
                None => ComboPrice::Unresolvable,
            },
            ComboSchedule::Unreadable => ComboPrice::Unresolvable,
            ComboSchedule::Absent => self.flat_or_unpriced(),
        }
    }

    fn flat_or_unpriced(&self) -> ComboPrice {
        self.flat_price()
            .map_or(ComboPrice::Unpriced, ComboPrice::Priced)
    }

    /// Whether the combo bills through a schedule at all (valid or broken).
    fn is_scheduled(&self) -> bool {
        !matches!(self.schedule, ComboSchedule::Absent)
    }
}

fn price_of_rate(rate: &PricingRate) -> Price {
    Price {
        input_per_1m: rate.input_per_1m,
        output_per_1m: rate.output_per_1m,
        cached_per_1m: rate.cached_per_1m,
        ..Default::default()
    }
}

type ComboPriceCols = (Option<f64>, Option<f64>, Option<f64>, Option<String>);

fn combo_pricing_row(conn: &Connection, combo_id: i64) -> Option<ComboPricingRow> {
    use rusqlite::OptionalExtension;
    let result: Result<Option<ComboPriceCols>, _> = conn
        .query_row(
            "SELECT pricing_input_per_1m, pricing_output_per_1m, pricing_cached_per_1m, \
             pricing_schedule FROM combos WHERE id = ?1",
            rusqlite::params![combo_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional();
    let (input_per_1m, output_per_1m, cached_per_1m, schedule_json) = result.ok().flatten()?;
    let schedule = match schedule_json {
        None => ComboSchedule::Absent,
        Some(raw) => match serde_json::from_str::<PricingSchedule>(&raw) {
            // A calendar that parses but does not validate (wrong timezone,
            // negative rate, empty or overlapping windows) is just as unbillable
            // as truncated JSON: it must not silently degrade to a flat price.
            Ok(schedule) if schedule.validate().is_ok() => ComboSchedule::Valid(schedule),
            Ok(_) | Err(_) => {
                tracing::error!(
                    combo_id,
                    "combo pricing_schedule is not a valid calendar; refusing to price its \
                     requests instead of billing a fallback tariff"
                );
                ComboSchedule::Unreadable
            }
        },
    };
    Some(ComboPricingRow {
        input_per_1m,
        output_per_1m,
        cached_per_1m,
        schedule,
    })
}

/// Resolve a request price, falling back to the target model only when the
/// combo genuinely carries no price.
fn resolve(
    conn: &Connection,
    provider: &str,
    model: &str,
    combo_id: Option<i64>,
    outcome: impl FnOnce(&ComboPricingRow) -> ComboPrice,
) -> Option<Price> {
    if let Some(id) = combo_id
        && let Some(row) = combo_pricing_row(conn, id)
    {
        match outcome(&row) {
            ComboPrice::Priced(price) => return Some(price),
            ComboPrice::Unresolvable => return None,
            ComboPrice::Unpriced => {}
        }
    }
    lookup_with_db(conn, provider, model)
}

/// The combo's price at `at`, honouring its schedule.
pub fn lookup_combo_price_at(conn: &Connection, combo_id: i64, at: DateTime<Utc>) -> Option<Price> {
    match combo_pricing_row(conn, combo_id)?.resolve_at(at) {
        ComboPrice::Priced(price) => Some(price),
        ComboPrice::Unpriced | ComboPrice::Unresolvable => None,
    }
}

/// The combo's most expensive band, for admission before the band is known.
pub fn lookup_combo_price_conservative(conn: &Connection, combo_id: i64) -> Option<Price> {
    match combo_pricing_row(conn, combo_id)?.resolve_conservative() {
        ComboPrice::Priced(price) => Some(price),
        ComboPrice::Unpriced | ComboPrice::Unresolvable => None,
    }
}

/// Price that governs one request at a known instant.
///
/// When the request was routed through a combo that carries its own price —
/// flat or scheduled — that price is the bill; the target models' rates are
/// irrelevant (several upstreams publish no pricing at all, and the combo is
/// the product sold). Without a combo price the lookup falls back to the target
/// model's price.
pub fn lookup_for_request_at(
    conn: &Connection,
    provider: &str,
    model: &str,
    combo_id: Option<i64>,
    at: DateTime<Utc>,
) -> Option<Price> {
    resolve(conn, provider, model, combo_id, |row| row.resolve_at(at))
}

/// Price that governs one request right now.
///
/// Billing paths pass the instant they bill for; use [`lookup_for_request_at`]
/// whenever that instant is not "now" (a stored usage row, a replayed request)
/// so the band is reproducible.
pub fn lookup_for_request(
    conn: &Connection,
    provider: &str,
    model: &str,
    combo_id: Option<i64>,
) -> Option<Price> {
    lookup_for_request_at(conn, provider, model, combo_id, Utc::now())
}

/// Most expensive band that could govern a request through `combo_id`.
///
/// Admission (credit reservation, prepaid budget) happens before the request
/// runs and may straddle a peak-window boundary, so the reservation must bound
/// **both** bands: reserving at the cheap band and billing at the expensive one
/// is exactly the overdraft the ledger is not allowed to create.
pub fn lookup_for_request_conservative(
    conn: &Connection,
    provider: &str,
    model: &str,
    combo_id: Option<i64>,
) -> Option<Price> {
    resolve(
        conn,
        provider,
        model,
        combo_id,
        ComboPricingRow::resolve_conservative,
    )
}

/// Price for a row that is being (re)priced after the fact.
///
/// A scheduled combo's band is a function of the instant the row was billed at,
/// so re-pricing history requires that instant. When the combo has a schedule
/// and the timestamp is missing or unreadable this returns `None` — the row is
/// left untouched rather than re-priced at today's band, which would rewrite
/// history with a tariff that never applied to it. The same holds when the
/// persisted schedule itself is unreadable: history keeps whatever it recorded.
pub fn lookup_for_usage_at(
    conn: &Connection,
    provider: &str,
    model: &str,
    combo_id: Option<i64>,
    billed_at: Option<DateTime<Utc>>,
) -> Option<Price> {
    resolve(conn, provider, model, combo_id, |row| {
        row.resolve_historical(billed_at)
    })
}

/// Whether the combo bills through a schedule (valid **or** unreadable).
///
/// `None` when the combo row does not exist. Backfills use this to decide
/// whether a historical row must be re-priced from its own timestamp: a
/// scheduled combo's band cannot be reproduced from "now".
pub fn combo_is_scheduled(conn: &Connection, combo_id: i64) -> Option<bool> {
    combo_pricing_row(conn, combo_id).map(|row| row.is_scheduled())
}

/// Parse the `usage.created_at` shapes this database actually stores.
///
/// * `YYYY-MM-DD HH:MM:SS` — SQLite's `datetime('now')`, UTC (the hot path),
/// * `YYYY-MM-DDTHH:MM:SSZ` — the RFC-3339 shape older writers used, with
///   optional fractional seconds.
///
/// Only UTC instants are accepted. A timestamp carrying a non-UTC offset is not
/// guessed at: it returns `None`, and every caller leaves that row untouched
/// rather than re-pricing it at an invented instant.
#[must_use]
pub fn parse_usage_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    const FORMATS: [&str; 4] = [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%SZ",
        "%Y-%m-%dT%H:%M:%S%.fZ",
    ];
    FORMATS
        .iter()
        .find_map(|format| chrono::NaiveDateTime::parse_from_str(raw, format).ok())
        .map(|naive| naive.and_utc())
}

#[cfg(test)]
mod tests;
