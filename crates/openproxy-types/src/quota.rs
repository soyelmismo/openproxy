use serde::{Deserialize, Serialize};

/// Which commercial entitlement a quota pool is billed against.
///
/// Z.ai exposes two unrelated products under one login, so the source is part
/// of the identity of a pool and never merged away:
///
/// * [`QuotaSource::ZcodeStarter`] — the free/promotional starter buckets
///   served by the ZCode billing contract, keyed by `zcode_jwt_token`.
/// * [`QuotaSource::CodingPlan`] — the paid Coding Plan subscription, keyed by
///   `business_access_token`.
///
/// "No active Coding Plan" is a statement about one source only; it says
/// nothing about the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaSource {
    ZcodeStarter,
    CodingPlan,
}

impl QuotaSource {
    #[inline]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ZcodeStarter => "zcode_starter",
            Self::CodingPlan => "coding_plan",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "zcode_starter" => Ok(Self::ZcodeStarter),
            "coding_plan" => Ok(Self::CodingPlan),
            other => Err(format!("invalid quota source: {other}")),
        }
    }
}

impl std::fmt::Display for QuotaSource {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for QuotaSource {
    type Err = String;

    #[inline]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Lifecycle of one quota pool, deliberately kept apart from the numbers.
///
/// [`QuotaPoolStatus::Unavailable`] means "we could not read this source" and
/// is *not* exhausted: a credential or transport failure must never be scored
/// as zero remaining quota, because doing so silently retires an account whose
/// plan may still be fully available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaPoolStatus {
    /// Readable, in its validity window, with budget left.
    Active,
    /// The upstream reports no such entitlement for this account.
    Absent,
    /// Readable and exhausted (`remaining <= 0`, or `used >= limit`).
    Exhausted,
    /// The entitlement's own `expires_at` has passed.
    Expired,
    /// Unreadable (auth/transport/parse). Balance unknown, not zero.
    Unavailable,
}

impl QuotaPoolStatus {
    #[inline]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Absent => "absent",
            Self::Exhausted => "exhausted",
            Self::Expired => "expired",
            Self::Unavailable => "unavailable",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "active" => Ok(Self::Active),
            "absent" => Ok(Self::Absent),
            "exhausted" => Ok(Self::Exhausted),
            "expired" => Ok(Self::Expired),
            "unavailable" => Ok(Self::Unavailable),
            other => Err(format!("invalid quota pool status: {other}")),
        }
    }
}

impl std::fmt::Display for QuotaPoolStatus {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for QuotaPoolStatus {
    type Err = String;

    #[inline]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// One independently accounted entitlement of an account.
///
/// Pools are **alternatives**, never a shared budget: the same request may be
/// paid for by the Coding Plan *or* by a ZCode starter bucket, so combining
/// them with a global `min` (the way the legacy aggregate quota path does)
/// manufactures exhaustion that neither product has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaPool {
    pub id: String,
    pub source: QuotaSource,
    pub plan_name: Option<String>,
    pub status: QuotaPoolStatus,
    pub unit: String,
    pub used: Option<i64>,
    pub limit: Option<i64>,
    pub remaining: Option<i64>,
    pub reset_at: Option<String>,
    pub expires_at: Option<String>,
    pub starts_at: Option<String>,
    /// Models this entitlement authorizes. Empty means "the whole catalog"
    /// **only** for [`QuotaSource::CodingPlan`], whose plan is account-wide;
    /// an empty list on a starter bucket is treated as authorizing nothing,
    /// because those buckets are scoped per model by `capabilities`.
    pub model_ids: Vec<String>,
    pub model_details: Option<Box<[ModelQuotaDetail]>>,
    pub fetch_error: Option<String>,
    pub last_fetched_at: String,
}

impl QuotaPool {
    /// Whether this pool authorizes `model`.
    ///
    /// Exact, case-insensitive match on the whole id only. Substring or alias
    /// matching is rejected on purpose: `glm-4.6` is a prefix of
    /// `glm-4.6-thinking-plus`, and treating the shorter bucket as covering the
    /// longer model routes requests into an entitlement that never paid for
    /// them.
    pub fn matches_model(&self, model: &str) -> bool {
        if model.trim().is_empty() {
            return false;
        }
        if self.model_ids.is_empty() {
            return self.source == QuotaSource::CodingPlan;
        }
        self.model_ids
            .iter()
            .any(|id| id.eq_ignore_ascii_case(model.trim()))
    }

    /// Parse an epoch-seconds timestamp in either representation we persist:
    /// bare integer seconds, or an ISO-8601 / SQLite datetime string.
    pub(crate) fn parse_epoch(raw: &str) -> Option<u64> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        if let Ok(secs) = trimmed.parse::<u64>() {
            return Some(secs);
        }
        if let Ok(secs) = trimmed.parse::<i64>() {
            return u64::try_from(secs).ok();
        }
        crate::timestamp::parse_timestamp(trimmed)
            .ok()
            .and_then(|dt| u64::try_from(dt.timestamp()).ok())
    }

    /// Fail-closed validity: a timestamp that is *present but unparseable*
    /// disqualifies the pool instead of being ignored, because an unreadable
    /// expiry is uncertainty about the window, and uncertainty must not
    /// authorize inference.
    fn within_validity_window(&self, now_secs: u64) -> bool {
        if let Some(starts) = self.starts_at.as_deref() {
            match Self::parse_epoch(starts) {
                Some(s) if s > now_secs => return false,
                Some(_) => {}
                None => return false,
            }
        }
        if let Some(expires) = self.expires_at.as_deref() {
            match Self::parse_epoch(expires) {
                Some(e) if e <= now_secs => return false,
                Some(_) => {}
                None => return false,
            }
        }
        true
    }

    fn has_known_exhaustion(&self) -> bool {
        if let Some(limit) = self.limit
            && let Some(used) = self.used
            && limit > 0
            && used >= limit
        {
            return true;
        }
        self.remaining.is_some_and(|r| r <= 0)
    }

    /// Whether this pool carries any positive budget claim at all.
    ///
    /// A pool that authorizes `model` but reports no `remaining` and no
    /// `limit`/`used` pair has made no budget claim whatsoever. For
    /// [`QuotaSource::CodingPlan`] that is the contract's own way of saying
    /// "entitled, unlimited or unreported", so it stays usable; for a starter
    /// bucket, which is always capability-scoped and always metered, an
    /// empty reading is a malformed payload and fails closed rather than
    /// authorizing inference on unknown data.
    fn claims_positive_budget(&self) -> bool {
        if self.remaining.is_some_and(|r| r > 0) {
            return true;
        }
        if self.limit.is_some_and(|l| l > 0) && self.used.is_some() {
            return true;
        }
        self.source == QuotaSource::CodingPlan
    }

    /// Only a readable, definitive negative can justify spending another source.
    /// Unknown models, timestamps or budget are never a negative verdict.
    pub fn is_known_ineligible_for_model(&self, now_secs: u64, model: &str) -> bool {
        if self.fetch_error.is_some() || self.status == QuotaPoolStatus::Unavailable {
            return false;
        }
        if matches!(
            self.status,
            QuotaPoolStatus::Absent | QuotaPoolStatus::Exhausted | QuotaPoolStatus::Expired
        ) {
            return true;
        }
        if self.source == QuotaSource::ZcodeStarter && self.model_ids.is_empty() {
            return false;
        }
        if !self.matches_model(model) {
            return true;
        }
        let starts = match self.starts_at.as_deref() {
            Some(raw) => match Self::parse_epoch(raw) {
                Some(v) => Some(v),
                None => return false,
            },
            None => None,
        };
        let expires = match self.expires_at.as_deref() {
            Some(raw) => match Self::parse_epoch(raw) {
                Some(v) => Some(v),
                None => return false,
            },
            None => None,
        };
        starts.is_some_and(|v| v > now_secs)
            || expires.is_some_and(|v| v <= now_secs)
            || self.has_known_exhaustion()
    }

    /// Whether this pool may serve a request for `model` at `now_secs`.
    ///
    /// Fails closed on [`QuotaPoolStatus::Unavailable`]: an unreadable pool
    /// never becomes eligible just because the remaining counter is stale.
    pub fn is_usable_for_model(&self, now_secs: u64, model: &str) -> bool {
        if !self.matches_model(model) {
            return false;
        }
        if self.status != QuotaPoolStatus::Active || self.fetch_error.is_some() {
            return false;
        }
        if !self.within_validity_window(now_secs) {
            return false;
        }
        if self.remaining.is_some_and(|r| r <= 0) {
            return false;
        }
        if !self.claims_positive_budget() {
            return false;
        }
        !self.has_known_exhaustion()
    }

    /// Whether this pool is authoritative about `model` *and* reports no
    /// budget.
    ///
    /// Distinct from [`Self::is_usable_for_model`], which also requires budget
    /// to remain: a readable `active`/`exhausted`/`absent`/`expired` verdict is
    /// a known negative, while [`QuotaPoolStatus::Unavailable`] is unknown data
    /// and never reads as zero.
    pub fn reports_exhaustion_for_model(&self, now_secs: u64, model: &str) -> bool {
        if !self.matches_model(model) || self.fetch_error.is_some() {
            return false;
        }
        match self.status {
            QuotaPoolStatus::Absent | QuotaPoolStatus::Exhausted | QuotaPoolStatus::Expired => true,
            QuotaPoolStatus::Active => {
                self.within_validity_window(now_secs) && self.has_known_exhaustion()
            }
            QuotaPoolStatus::Unavailable => false,
        }
    }
}

/// Fraction of an **eligible** pool's own budget that is left.
///
/// Only called for a pool that [`QuotaPool::is_usable_for_model`] accepted, so
/// it has no known `used >= limit` and no `remaining <= 0`. When `remaining`
/// is absent but a `limit`/`used` pair exists, the difference is the budget —
/// falling back to the full `limit` would overstate it by everything already
/// spent. With no ceiling at all the pool claimed positive budget without a
/// size, which reads as fully available rather than as zero.
fn usable_fraction(pool: &QuotaPool) -> f64 {
    let Some(limit) = pool.limit.filter(|limit| *limit > 0) else {
        return 1.0;
    };
    let remaining = match pool.remaining {
        Some(r) => r,
        None => match pool.used {
            Some(used) => limit.saturating_sub(used).max(0),
            None => limit,
        },
    };
    (remaining.clamp(0, limit) as f64 / limit as f64).clamp(0.0, 1.0)
}

/// Remaining fraction of the *best eligible pool* for `model`.
///
/// Alternatives, not a shared budget: an eligible
/// [`QuotaSource::ZcodeStarter`] pool wins outright, otherwise the eligible
/// [`QuotaSource::CodingPlan`] pool is used. Their numbers are never compared
/// against each other, and a `min` across sources is never computed.
///
/// Uncertainty outranks exhaustion: if *any* pool that mentions `model` is
/// [`QuotaPoolStatus::Unavailable`] while another one is merely exhausted, the
/// result is `None`, because retiring the account on a read failure would
/// discard a credential whose plan may still be live. Callers must not treat
/// `None` as zero.
///
/// * `Some(fraction)` — an entitlement authorizes `model` and reports itself
///   usable; `fraction` is that pool's own budget split.
/// * `Some(0.0)` — every pool that mentions `model` is known and none has
///   budget left (exhausted, absent or expired).
/// * `None` — nothing usable or known: only unavailable pools, or pools that
///   never mention `model`.
pub fn zai_remaining_fraction(pools: &[QuotaPool], model: &str, now_secs: u64) -> Option<f64> {
    for source in [QuotaSource::ZcodeStarter, QuotaSource::CodingPlan] {
        if source == QuotaSource::CodingPlan
            && (!pools.iter().any(|p| p.source == QuotaSource::ZcodeStarter)
                || pools.iter().any(|p| {
                    p.source == QuotaSource::ZcodeStarter
                        && !p.is_known_ineligible_for_model(now_secs, model)
                }))
        {
            return None;
        }
        if let Some(pool) = pools
            .iter()
            .find(|p| p.source == source && p.is_usable_for_model(now_secs, model))
        {
            return Some(usable_fraction(pool));
        }
    }

    // Any unreadable verdict on `model` makes the whole account uncertain:
    // report unknown instead of collapsing to the exhausted reading.
    if pools.iter().any(|p| {
        (p.matches_model(model)
            || (p.source == QuotaSource::ZcodeStarter && p.model_ids.is_empty()))
            && (p.fetch_error.is_some() || p.status == QuotaPoolStatus::Unavailable)
    }) {
        return None;
    }

    for source in [QuotaSource::ZcodeStarter, QuotaSource::CodingPlan] {
        if pools
            .iter()
            .any(|p| p.source == source && p.reports_exhaustion_for_model(now_secs, model))
        {
            return Some(0.0);
        }
    }

    None
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelQuotaDetail {
    pub model_id: String,
    pub session_used: i64,
    pub session_limit: i64,
    pub session_reset_at: Option<String>,
    pub remaining_fraction: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountQuota {
    pub session_used: Option<i64>,
    pub session_limit: Option<i64>,
    pub session_reset_at: Option<String>,
    pub weekly_used: Option<i64>,
    pub weekly_limit: Option<i64>,
    pub weekly_reset_at: Option<String>,
    pub plan_name: Option<String>,
    pub last_fetched_at: String,
    pub fetch_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_details: Option<Box<[ModelQuotaDetail]>>,
    /// Independently accounted entitlements (Z.ai Coding Plan vs ZCode
    /// starter buckets). Additive and absent on every pre-existing row, so
    /// deserializing legacy JSON yields `None` rather than an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pools: Option<Box<[QuotaPool]>>,
}

impl AccountQuota {
    /// Whether the snapshot carries no signal at all.
    ///
    /// A starter-only account has no legacy aggregate numbers — every
    /// `quota_session_*`/`quota_weekly_*` field is `None` by contract — so the
    /// legacy fields alone would report it as empty and callers would discard a
    /// perfectly good pool snapshot. A non-empty `pools` is therefore signal
    /// in its own right.
    pub fn is_empty(&self) -> bool {
        self.session_used.is_none()
            && self.weekly_used.is_none()
            && self.fetch_error.is_none()
            && self.pools.as_ref().is_none_or(|p| p.is_empty())
    }

    /// Merge a fresh snapshot over a stored one, preserving good pools.
    ///
    /// A top-level `fetch_error` only invalidates what we actually failed to
    /// read, so the previous snapshot survives when the new one carries no
    /// pools — but the carried pools are **not** presented as fresh: an
    /// `Unavailable` pool keeps its unreadable status and its `fetch_error`,
    /// and its `last_fetched_at` is stamped with this merge instead of the
    /// successful read it never was. Re-labelling stale data as `Active` would
    /// make a dead credential look live at the next routing decision.
    pub fn merge_over_previous(self, previous: Option<&AccountQuota>) -> Self {
        let fetched_at = self.last_fetched_at.clone();
        match (self.pools, previous.and_then(|p| p.pools.clone())) {
            (Some(new), Some(previous)) if !new.is_empty() => {
                let mut merged = Vec::new();
                for pool in new.into_vec() {
                    if pool.status == QuotaPoolStatus::Unavailable || pool.fetch_error.is_some() {
                        let prior: Vec<_> = previous
                            .iter()
                            .filter(|p| {
                                p.source == pool.source
                                    && (p.id == pool.id
                                        || !previous
                                            .iter()
                                            .any(|p| p.source == pool.source && p.id == pool.id))
                            })
                            .collect();
                        if !prior.is_empty() {
                            for old in prior {
                                let mut kept = old.clone();
                                kept.status = QuotaPoolStatus::Unavailable;
                                kept.fetch_error = pool.fetch_error.clone();
                                kept.last_fetched_at = fetched_at.clone();
                                merged.push(kept);
                            }
                            continue;
                        }
                    }
                    merged.push(pool);
                }
                Self {
                    pools: Some(merged.into_boxed_slice()),
                    ..self
                }
            }
            (Some(new), _) => Self {
                pools: Some(new),
                ..self
            },
            (None, Some(mut kept)) if self.fetch_error.is_some() => {
                for pool in &mut kept {
                    pool.status = QuotaPoolStatus::Unavailable;
                    pool.fetch_error = self.fetch_error.clone();
                    pool.last_fetched_at = fetched_at.clone();
                }
                Self {
                    pools: Some(kept),
                    ..self
                }
            }
            (None, kept) => Self {
                pools: kept,
                ..self
            },
        }
    }

    pub fn empty() -> Self {
        Self {
            session_used: None,
            session_limit: None,
            session_reset_at: None,
            weekly_used: None,
            weekly_limit: None,
            weekly_reset_at: None,
            plan_name: None,
            last_fetched_at: now_unix_secs_str(),
            fetch_error: None,
            model_details: None,
            pools: None,
        }
    }

    pub fn with_error(err: impl Into<String>) -> Self {
        Self {
            fetch_error: Some(err.into()),
            ..Self::empty()
        }
    }

    pub fn with_plan(plan_name: impl Into<String>) -> Self {
        Self {
            plan_name: Some(plan_name.into()),
            ..Self::empty()
        }
    }
}

impl Default for AccountQuota {
    fn default() -> Self {
        Self::empty()
    }
}

pub fn now_unix_secs_str() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    secs.to_string()
}
fn parse_duration_unit(unit: u8, val: f64) -> f64 {
    match unit {
        b'h' => val * 3600.0,
        b'm' => val * 60.0,
        b's' => val,
        _ => 0.0,
    }
}

fn parse_compound_duration(s: &str) -> Option<u64> {
    let mut total_secs = 0.0;
    let mut num_range: Option<(usize, usize)> = None;
    for (i, b) in s.bytes().enumerate() {
        if b.is_ascii_digit() || b == b'.' {
            num_range = Some((num_range.map_or(i, |(start, _)| start), i + 1));
        } else if matches!(b, b'h' | b'm' | b's')
            && let Some((start, end)) = num_range.take()
        {
            let val = s[start..end].parse::<f64>().unwrap_or(0.0);
            total_secs += parse_duration_unit(b, val);
        }
    }
    let total = total_secs.ceil() as u64;
    if total > 0 { Some(total) } else { None }
}

pub fn parse_reset_time(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Ok(secs) = s.parse::<u64>() {
        return Some(secs);
    }
    if let Ok(secs_f) = s.parse::<f64>() {
        return Some(secs_f.ceil() as u64);
    }
    parse_compound_duration(s)
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod tests;
