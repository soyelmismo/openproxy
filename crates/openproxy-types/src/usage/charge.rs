//! What the credit ledger actually charged for one usage row.
//!
//! Kept out of `usage.rs` so that file stays inside the per-file size budget
//! (AGENTS.md) while the charge semantics live in one place, shared by the DB
//! row projection, the analytics projections and the wire format.

use serde::{Deserialize, Serialize};

/// What the credit ledger actually charged for one usage row.
///
/// Never infer a charge from `cost_usd` (that is the calculated price) nor from
/// the `credits_debited` marker alone: the marker is set even when nothing moved
/// (a key with no prepaid credits debits 0 and is still marked) and a debit is
/// clamped to `min(cost, balance)`. Only the durable `charged_units` column says
/// how much money left the balance, so the dashboard reads this verdict instead
/// of guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChargeState {
    /// A positive amount was debited; `charged_units` carries it.
    Charged,
    /// Nothing was debited for this row. The amount is exactly 0.
    NotCharged,
    /// The debit ran but its amount was never recorded (rows older than the
    /// `charged_units` column). No amount may be invented for these.
    ///
    /// Also the `Default`: absent evidence must read as unknown, never as a
    /// claim that nothing was charged.
    #[default]
    Unknown,
}

impl ChargeState {
    /// Derive the verdict from the durable columns only.
    ///
    /// `charged_units` is the amount the debit wrote (micro-USD) and `debited`
    /// is the exactly-once marker. `None` + marker set means the amount is not
    /// recoverable: report `Unknown`, never the calculated price.
    #[must_use]
    pub fn derive(charged_units: Option<i64>, debited: bool) -> Self {
        match (charged_units, debited) {
            (Some(units), _) if units > 0 => Self::Charged,
            (Some(_), _) => Self::NotCharged,
            (None, true) => Self::Unknown,
            (None, false) => Self::NotCharged,
        }
    }

    /// Stable wire token (`charged` | `not_charged` | `unknown`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Charged => "charged",
            Self::NotCharged => "not_charged",
            Self::Unknown => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ChargeState;

    /// The cost column must never guess: the verdict comes from the durable
    /// columns, and a marked row without a recorded amount stays unknown.
    #[test]
    fn test_charge_state_derives_only_from_durable_columns() {
        assert_eq!(
            ChargeState::derive(Some(14_600), true),
            ChargeState::Charged
        );
        // A non-credit key (or a drained balance) debits 0 and is still marked.
        assert_eq!(ChargeState::derive(Some(0), true), ChargeState::NotCharged);
        // Historical row: the debit ran before amounts were recorded.
        assert_eq!(ChargeState::derive(None, true), ChargeState::Unknown);
        // No debit ever ran for this row.
        assert_eq!(ChargeState::derive(None, false), ChargeState::NotCharged);
        assert_eq!(ChargeState::Charged.as_str(), "charged");
        assert_eq!(ChargeState::NotCharged.as_str(), "not_charged");
        assert_eq!(ChargeState::Unknown.as_str(), "unknown");
    }
}
