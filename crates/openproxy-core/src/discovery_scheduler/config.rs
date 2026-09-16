//! Configuration and scheduler handle definitions.

use tokio_util::sync::CancellationToken;

/// Per-provider refresh cadence. Spec: 1 hour. Bumped in tests via
/// [`DiscoverySchedulerConfig::interval_secs`].
pub const DISCOVERY_INTERVAL_SECS: u64 = 3_600;

/// TTL written to the `expires_at` column on each upsert.
pub const DISCOVERY_TTL_SECONDS: i64 = 3_600;

/// Default initial stagger in seconds. 15 seconds allows quick discovery
/// on startup without swamping the network immediately.
pub const DEFAULT_INITIAL_STAGGER_SECS: u64 = 15;

/// Configuration knobs exposed to the caller.
#[derive(Debug, Clone)]
pub struct DiscoverySchedulerConfig {
    /// Per-provider tick cadence in seconds.
    pub interval_secs: u64,
    /// Upper bound (inclusive) of the uniform initial stagger in seconds.
    pub initial_stagger_secs: u64,
}

impl Default for DiscoverySchedulerConfig {
    fn default() -> Self {
        Self {
            interval_secs: DISCOVERY_INTERVAL_SECS,
            initial_stagger_secs: DEFAULT_INITIAL_STAGGER_SECS,
        }
    }
}

/// Handle to the background discovery scheduler.
pub struct DiscoveryScheduler {
    pub(crate) cancel: CancellationToken,
    pub task_count: usize,
}

impl DiscoveryScheduler {
    /// Signal all per-provider tasks to stop. Idempotent.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl std::fmt::Debug for DiscoveryScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveryScheduler")
            .field("task_count", &self.task_count)
            .finish_non_exhaustive()
    }
}
