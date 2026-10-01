//! Configuration and scheduler handle definitions.

use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Per-provider refresh cadence. Spec: 1 hour. Bumped in tests via
/// [`DiscoverySchedulerConfig::interval_secs`].
pub const DISCOVERY_INTERVAL_SECS: u64 = 3_600;

/// TTL written to the `expires_at` column on each upsert.
pub const DISCOVERY_TTL_SECONDS: i64 = 3_600;

/// Default initial stagger in seconds. 180 seconds allows smooth discovery
/// on startup without swamping the CPU or network immediately.
pub const DEFAULT_INITIAL_STAGGER_SECS: u64 = 180;

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
    pub(crate) handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl DiscoveryScheduler {
    /// Signal all per-provider tasks to stop. Idempotent.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Signal all background tasks to stop and await their completion.
    ///
    /// This method is cancel-safe and idempotent for concurrent callers.
    pub async fn shutdown_and_wait(&self) {
        self.cancel();
        let mut handles = self.handles.lock().await;
        while let Some(handle) = handles.last_mut() {
            if let Err(error) = handle.await {
                tracing::error!(%error, "discovery scheduler task failed during shutdown");
            }
            handles.pop();
        }
    }
}

impl std::fmt::Debug for DiscoveryScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveryScheduler")
            .field("task_count", &self.task_count)
            .finish_non_exhaustive()
    }
}
