use futures::FutureExt;
use openproxy_adapters::upstream::CancellationToken;
use std::sync::Arc;

use super::BackgroundService;

/// State container for registered background tasks.
pub(super) struct SupervisorRegistry {
    pub(super) tasks: Vec<tokio::task::JoinHandle<()>>,
    closed: bool,
}

impl SupervisorRegistry {
    fn prune_finished(&mut self) {
        self.tasks.retain_mut(|handle| {
            if handle.is_finished()
                && let Some(res) = (&mut *handle).now_or_never()
            {
                if let Err(e) = res
                    && !e.is_cancelled()
                {
                    tracing::error!("background task failed: {e}");
                }
                return false;
            }
            true
        });
    }
}

/// Supervisor for background services managing cancellation and deterministic task drain.
#[derive(Clone)]
pub struct BackgroundSupervisor {
    cancel: CancellationToken,
    pub(super) registry: Arc<parking_lot::Mutex<SupervisorRegistry>>,
    pub(super) drain_lock: Arc<tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl Default for BackgroundSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundSupervisor {
    /// Create a new supervisor with an un-cancelled token and empty task registry.
    pub fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            registry: Arc::new(parking_lot::Mutex::new(SupervisorRegistry {
                tasks: Vec::new(),
                closed: false,
            })),
            drain_lock: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
    }

    /// Obtain a clone of the cancellation token.
    pub fn token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Signal cancellation to all supervised background services (synchronous API).
    /// Marks the supervisor as closed under the registry lock so subsequent spawns are rejected.
    pub fn shutdown(&self) {
        self.cancel.cancel();
        let mut reg = self.registry.lock();
        reg.closed = true;
    }

    /// Signal cancellation and await all registered background tasks until they drain.
    ///
    /// Safe against cancellation: pending tasks are taken into the async drain lock
    /// under the synchronous registry lock, and awaited in place via `drain.last_mut()`,
    /// popping each handle only upon completion. If an awaiting caller is cancelled,
    /// in-flight and pending handles remain in `drain_lock` so subsequent callers continue
    /// draining without task detachment or deadlock.
    pub async fn shutdown_and_wait(&self) {
        self.cancel.cancel();
        let mut drain = self.drain_lock.lock().await;
        {
            let mut reg = self.registry.lock();
            reg.closed = true;
            if !reg.tasks.is_empty() {
                drain.extend(std::mem::take(&mut reg.tasks));
            }
        }

        while let Some(handle) = drain.last_mut() {
            if let Err(e) = handle.await
                && !e.is_cancelled()
            {
                tracing::error!("background service task failed on shutdown: {e}");
            }
            drain.pop();
        }
    }

    fn admit(
        &self,
        name: &'static str,
        spawn_fn: impl FnOnce() -> tokio::task::JoinHandle<()>,
    ) -> bool {
        let mut reg = self.registry.lock();
        reg.prune_finished();
        if reg.closed || self.cancel.is_cancelled() {
            tracing::warn!(service = name, "supervisor is closed; rejecting new task");
            return false;
        }
        let handle = spawn_fn();
        reg.tasks.push(handle);
        true
    }

    /// Spawn a [`BackgroundService`] under this supervisor's cancellation scope.
    /// Returns `true` if admitted, or `false` if rejected because the supervisor is closed.
    pub fn spawn<S: BackgroundService>(&self, service: S) -> bool {
        let name = service.name();
        let cancel = self.cancel.clone();
        self.admit(name, move || {
            tokio::spawn(async move {
                tracing::debug!(service = name, "background service started");
                service.run(cancel).await;
                tracing::debug!(service = name, "background service stopped");
            })
        })
    }

    /// Spawn a one-shot task that stops at shutdown instead of running to
    /// completion. Use it for work that is safe to abandon midway (each step
    /// persists its own result), such as proxy validation passes.
    pub fn spawn_cancellable_one_shot(
        &self,
        name: &'static str,
        future: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> bool {
        let cancel = self.cancel.clone();
        self.spawn_one_shot(name, async move {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::debug!(task = name, "one-shot background task cancelled by shutdown");
                }
                () = future => {}
            }
        })
    }

    /// Spawn a one-shot background task under this supervisor.
    /// Admitted work runs to completion, including during shutdown; it is not
    /// interrupted by cancellation and remains owned until drained or reaped.
    /// Returns `true` if admitted, or `false` if rejected because the supervisor is closed.
    pub fn spawn_one_shot(
        &self,
        name: &'static str,
        future: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> bool {
        self.admit(name, move || {
            tokio::spawn(async move {
                tracing::debug!(task = name, "one-shot background task started");
                future.await;
                tracing::debug!(task = name, "one-shot background task finished");
            })
        })
    }

    /// Count of currently registered tasks (pruning finished tasks first).
    #[cfg(test)]
    pub(crate) fn task_count(&self) -> usize {
        let mut reg = self.registry.lock();
        reg.prune_finished();
        reg.tasks.len()
    }
}
