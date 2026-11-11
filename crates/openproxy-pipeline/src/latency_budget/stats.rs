//! Pure statistics engine of the latency predictor: samples, token buckets,
//! weighted mean/sd/p99 and the budget formula
//! `max(mean + 3σ, empirical p99) + margin`, floored. No I/O: loaders feed
//! it rows and the cache (`super`) applies it.

use std::sync::atomic::Ordering;

use super::{CENSORED_RELAX_MULT, TargetStats};

/// Minimum samples a bucket needs before its statistics are trusted.
pub(super) const MIN_BUCKET_SAMPLES: usize = 6;
/// The budget is `max(mean + SIGMA_MULT*σ, p99) + MARGIN_MS`: the learned
/// standard deviation IS the grace period, never a hard-coded margin.
pub(super) const SIGMA_MULT: f64 = 3.0;
/// Small fixed margin on top of the statistical bound.
pub(super) const MARGIN_MS: u64 = 1_000;
/// Never cut a request earlier than this, no matter how fast the target is.
pub(super) const FLOOR_MS: u64 = 5_000;

/// Logarithmic prompt-token buckets. Index by `tokens_bucket(est)` and widen
/// upwards when a bucket lacks samples, never downwards: a wider bucket is
/// the more conservative (slower) population.
pub(crate) fn tokens_bucket(prompt_tokens: u32) -> usize {
    let t = prompt_tokens.max(1);
    // Boundaries: <1k, 1-4k, 4-16k, 16-64k, 64-256k, >256k
    match t {
        0..=999 => 0,
        1_000..=3_999 => 1,
        4_000..=15_999 => 2,
        16_000..=63_999 => 3,
        64_000..=255_999 => 4,
        _ => 5,
    }
}

pub(crate) const BUCKET_COUNT: usize = 6;

#[derive(Debug, Clone, Copy)]
pub(super) struct BucketStats {
    mean: f64,
    sd: f64,
    p99: f64,
}

impl BucketStats {
    /// Statistical budget of the bucket: the spread bound and the empirical
    /// tail bound, whichever is larger (correction 2), plus the fixed
    /// margin, floored.
    pub(super) fn budget_ms(&self) -> u64 {
        self.budget_ms_with(FLOOR_MS, MARGIN_MS)
    }

    /// Same formula with caller-owned floor/margin: the gap distribution
    /// needs a much higher floor than TTFT (long-tailed pacing) and a wider
    /// margin (see `chunk_budget`).
    pub(super) fn budget_ms_with(&self, floor_ms: u64, margin_ms: u64) -> u64 {
        let spread_bound = self.mean + SIGMA_MULT * self.sd;
        let stat = spread_bound.max(self.p99) + margin_ms as f64;
        (stat.max(floor_ms as f64)) as u64
    }
}

/// One latency sample feeding the weighted bucket statistics: a successful
/// TTFT (target/session norm) or a survived inter-chunk gap (idle-chunk
/// norm, see `chunk_budget`). Identical math, different measured quantity.
pub(super) struct Sample {
    pub(super) value_ms: u64,
    pub(super) prompt_tokens: u64,
    pub(super) estimated: bool,
    pub(super) age_hours: f64,
}

/// Builds per-bucket statistics with recency weighting (correction 6) and
/// the estimated-token preference (correction 5).
pub(super) fn build_buckets(
    rows: &[Sample],
    half_life_h: f64,
) -> [Option<BucketStats>; BUCKET_COUNT] {
    // Prefer rows whose prompt tokens were estimated the same way dispatch
    // estimates them (comparable buckets). Fall back to all rows only when
    // the estimated population is too thin to trust a bucket.
    let population: Vec<&Sample> = {
        let estimated: Vec<&Sample> = rows.iter().filter(|r| r.estimated).collect();
        if estimated.len() >= MIN_BUCKET_SAMPLES {
            estimated
        } else {
            rows.iter().collect()
        }
    };
    let mut buckets: [Option<BucketStats>; BUCKET_COUNT] = Default::default();
    for (b, bucket) in buckets.iter_mut().enumerate() {
        let members: Vec<(u64, f64)> = population
            .iter()
            .filter(|r| tokens_bucket(r.prompt_tokens.max(1) as u32) == b)
            .map(|r| (r.value_ms, 0.5f64.powf(r.age_hours / half_life_h)))
            .collect();
        if members.len() < MIN_BUCKET_SAMPLES {
            continue;
        }
        *bucket = Some(weighted_stats(&members));
    }
    buckets
}

/// Recency-weighted mean/sd/p99 over `(value_ms, weight)` pairs (corrections
/// 2 and 6). Shared by the per-token TTFT buckets and the pooled gap
/// distribution of `chunk_budget`.
pub(super) fn weighted_stats(weighted: &[(u64, f64)]) -> BucketStats {
    let n = weighted.len();
    let wsum: f64 = weighted.iter().map(|(_, w)| w).sum();
    let mean: f64 = weighted.iter().map(|(v, w)| *v as f64 * w).sum::<f64>() / wsum;
    let var: f64 = weighted
        .iter()
        .map(|(v, w)| {
            let d = *v as f64 - mean;
            d * d * w
        })
        .sum::<f64>()
        / wsum;
    // Sample correction: the weighted population variance underestimates
    // the spread by roughly n/(n-1) for near-uniform weights.
    let sd = (var * (n as f64) / ((n - 1) as f64).max(1.0)).sqrt();
    // Weighted empirical p99 (correction 2): sort by value, walk the
    // cumulative weight to the 99% quantile.
    let mut order: Vec<(u64, f64)> = weighted.to_vec();
    order.sort_by_key(|(v, _)| *v);
    let target_cum = 0.99 * wsum;
    let mut cum = 0.0;
    let mut p99 = order.last().map_or(0.0, |(v, _)| *v as f64);
    for (v, w) in &order {
        cum += w;
        if cum >= target_cum {
            p99 = *v as f64;
            break;
        }
    }
    BucketStats { mean, sd, p99 }
}

/// Resolves the learned budget of one level for the request's token bucket,
/// or `None` when the level has no trustworthy data or its budget already
/// reaches the caller's ceiling (nothing left to shorten).
pub(super) fn budget_from_stats(
    stats: &TargetStats,
    est_prompt_tokens: u32,
    ceiling_ms: u64,
) -> Option<u64> {
    // Widen upward: exact bucket first, then the next populated ones.
    let start = tokens_bucket(est_prompt_tokens);
    let mut budget_ms: Option<u64> = None;
    for b in start..BUCKET_COUNT {
        if let Some(bs) = stats.buckets[b] {
            budget_ms = Some(bs.budget_ms());
            break;
        }
    }
    let mut budget_ms = budget_ms?;
    // Anti-starvation relax (correction 1): every cut raises the floor of
    // the next budget. When the relax reaches the resolved timeout the
    // predictor turns itself off for this level.
    let censored = stats.censored_max_ms.load(Ordering::Relaxed);
    if censored > 0 {
        let relaxed = (censored as f64 * CENSORED_RELAX_MULT) as u64;
        budget_ms = budget_ms.max(relaxed);
    }
    if budget_ms >= ceiling_ms {
        return None;
    }
    Some(budget_ms)
}
