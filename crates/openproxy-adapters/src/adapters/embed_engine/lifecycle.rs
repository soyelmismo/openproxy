//! Gateway-side supervisor for the embed worker.
//!
//! Same shape as the laya supervisor: a bounded job queue, a resident worker
//! spawned on demand, and an idle timeout that kills the child and releases
//! native memory. Nothing is loaded at startup.

use super::worker::{self, Client};
use super::{CoreError, EmbedRequest, EmbedResponse, EmbedTask, Settings, split_rows};
use parking_lot::Mutex;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};
use tokio::sync::mpsc as queue;
use tokio_util::sync::CancellationToken;

static ENABLED: AtomicBool = AtomicBool::new(false);
static RESIDENT: AtomicUsize = AtomicUsize::new(0);
static MANAGER: Mutex<Option<Manager>> = Mutex::new(None);

struct Manager {
    sender: queue::Sender<Job>,
    cancel: CancellationToken,
    thread: std::thread::JoinHandle<()>,
}

struct Job {
    request: Option<EmbedRequest>,
    reply: mpsc::SyncSender<Result<Option<EmbedResponse>, CoreError>>,
}

/// Resident state, distinct from whether lazy inference is enabled.
pub fn is_available() -> bool {
    RESIDENT.load(Ordering::Acquire) != 0
}

pub fn is_enabled() -> bool {
    cfg!(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )) && ENABLED.load(Ordering::Acquire)
}

pub fn shutdown() {
    ENABLED.store(false, Ordering::Release);
    let manager = MANAGER.lock().take();
    if let Some(manager) = manager {
        manager.cancel.cancel();
        let _ = manager.thread.join();
    }
}

/// Enable lazy inference; activating a provider does not download or load it.
pub fn spawn_init_background() {
    ENABLED.store(true, Ordering::Release);
}

/// Explicit warm-up for callers that need it.
pub fn init(
    model: Option<&str>,
    tokenizer: Option<&str>,
    config: Option<&str>,
    threads: Option<usize>,
) -> Result<(), CoreError> {
    spawn_init_background();
    submit(
        None,
        Settings {
            model: model.map(str::to_owned),
            tokenizer: tokenizer.map(str::to_owned),
            config: config.map(str::to_owned),
            threads,
        },
    )?;
    Ok(())
}

/// Embed one multimodal batch. `dimensions` 0 means "model dimension".
///
/// Returns one normalized vector per input, in input order.
pub async fn execute_embedding(
    inputs: Vec<super::EmbedInput>,
    task: EmbedTask,
    dimensions: usize,
) -> Result<Vec<Vec<f32>>, CoreError> {
    let expected = inputs.len();
    let request = super::build_request(inputs, task, dimensions);
    worker::validate_request(&request)?;
    let response = tokio::task::spawn_blocking(move || submit(Some(request), Settings::default()))
        .await
        .map_err(|e| CoreError::Internal(format!("embed_engine join error: {e}")))?
        .map_err(|e| CoreError::Internal(format!("embed_engine execution error: {e}")))?
        .ok_or_else(|| CoreError::Internal("embed worker returned no response".into()))?;
    split_rows(&response, expected)
}

/// Convenience wrapper for the common text-only case.
pub async fn execute_text_embedding(
    texts: Vec<String>,
    task: EmbedTask,
    dimensions: usize,
) -> Result<Vec<Vec<f32>>, CoreError> {
    let inputs = texts
        .into_iter()
        .map(|text| super::EmbedInput::Text { text })
        .collect();
    execute_embedding(inputs, task, dimensions).await
}

fn submit(
    request: Option<EmbedRequest>,
    settings: Settings,
) -> Result<Option<EmbedResponse>, CoreError> {
    let (reply, receiver) = mpsc::sync_channel(1);
    {
        let mut manager = MANAGER.lock();
        if !is_enabled() {
            return Err(CoreError::Internal("embed engine is disabled".into()));
        }
        if manager.as_ref().is_none_or(|m| m.sender.is_closed()) {
            *manager = Some(start_manager(settings)?);
        }
        let sender = &manager
            .as_ref()
            .ok_or_else(|| CoreError::Internal("embed supervisor unavailable".into()))?
            .sender;
        sender
            .try_send(Job { request, reply })
            .map_err(|e| CoreError::Internal(format!("embed queue full or closed: {e}")))?;
    }
    // Cold artifact download (314 MB over slow links) plus session load.
    receiver
        .recv_timeout(Duration::from_secs(1800))
        .map_err(|e| CoreError::Internal(format!("embed worker stopped or timed out: {e}")))?
}

fn start_manager(settings: Settings) -> Result<Manager, CoreError> {
    let (sender, receiver) = queue::channel(8);
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let thread = std::thread::Builder::new()
        .name("embed-supervisor".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => {
                    runtime.block_on(supervise(receiver, token, settings, idle_timeout()))
                }
                Err(e) => tracing::error!(error = %e, "Cannot start embed supervisor runtime"),
            }
        })
        .map_err(|e| CoreError::Internal(format!("Cannot start embed supervisor: {e}")))?;
    Ok(Manager {
        sender,
        cancel,
        thread,
    })
}

struct Resident(Client);

impl Drop for Resident {
    fn drop(&mut self) {
        RESIDENT.fetch_sub(1, Ordering::AcqRel);
    }
}

fn idle_timeout() -> Duration {
    let minutes = std::env::var("OPENPROXY_EMBED_IDLE_TIMEOUT_MINUTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| (1..=1440).contains(v))
        .unwrap_or(5);
    Duration::from_secs(minutes * 60)
}

async fn supervise(
    mut receiver: queue::Receiver<Job>,
    cancel: CancellationToken,
    settings: Settings,
    idle: Duration,
) {
    let mut resident: Option<Resident> = None;
    loop {
        let job = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            job = tokio::time::timeout(idle, receiver.recv()) => match job {
                Ok(Some(job)) => job,
                Ok(None) => break,
                Err(_) => {
                    if resident.is_some() {
                        tracing::info!("embed idle timeout: stopping worker and releasing memory");
                        stop_resident(&mut resident).await;
                    }
                    continue;
                }
            }
        };
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            result = run_job(&mut resident, &settings, job.request.as_ref()) => result,
        };
        if result.is_err() {
            stop_resident(&mut resident).await;
        }
        let _ = job.reply.send(result);
    }
    stop_resident(&mut resident).await;
}

async fn stop_resident(resident: &mut Option<Resident>) {
    if let Some(mut worker) = resident.take() {
        worker.0.stop().await;
    }
}

async fn run_job(
    resident: &mut Option<Resident>,
    settings: &Settings,
    request: Option<&EmbedRequest>,
) -> Result<Option<EmbedResponse>, CoreError> {
    if resident.is_none() {
        super::assets::ensure(settings).await?;
        let client = Client::start(settings).await?;
        *resident = Some(Resident(client));
        RESIDENT.fetch_add(1, Ordering::AcqRel);
    }
    let Some(request) = request else {
        return Ok(None);
    };
    let client = &mut resident
        .as_mut()
        .ok_or_else(|| CoreError::Internal("embed worker unavailable".into()))?
        .0;
    client.embed(request).await.map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn supervisor_cancellation_closes_queue_without_loading() {
        let (sender, receiver) = queue::channel(8);
        let cancel = CancellationToken::new();
        cancel.cancel();
        supervise(
            receiver,
            cancel,
            Settings::default(),
            Duration::from_secs(300),
        )
        .await;
        assert!(sender.is_closed());
    }

    #[test]
    fn queue_is_bounded_and_disconnected_replies_do_not_panic() {
        let (sender, _receiver) = queue::channel(8);
        for _ in 0..8 {
            let (reply, _) = mpsc::sync_channel(1);
            assert!(
                sender
                    .try_send(Job {
                        request: None,
                        reply
                    })
                    .is_ok()
            );
        }
        let (reply, _) = mpsc::sync_channel(1);
        assert!(
            sender
                .try_send(Job {
                    request: None,
                    reply
                })
                .is_err()
        );
    }

    #[tokio::test]
    #[ignore = "requires local embeddinggemma weights, server binary and Linux Landlock ABI 3"]
    async fn idle_unloads_and_next_request_reloads() {
        let (sender, receiver) = queue::channel(8);
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let supervisor = tokio::spawn(supervise(
            receiver,
            token,
            Settings::default(),
            Duration::from_millis(100),
        ));
        for _ in 0..2 {
            let (reply, response) = mpsc::sync_channel(1);
            sender
                .send(Job {
                    request: None,
                    reply,
                })
                .await
                .unwrap();
            let result = tokio::task::spawn_blocking(move || response.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(result.is_ok(), "{result:?}");
            assert!(is_available());
            tokio::time::timeout(Duration::from_secs(10), async {
                while is_available() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
        }
        cancel.cancel();
        supervisor.await.unwrap();
        assert!(sender.is_closed());
    }

    #[test]
    fn task_prefixes_match_sentence_transformers_config() {
        assert_eq!(
            EmbedTask::Query.as_prefix(),
            "task: search result | query: "
        );
        assert_eq!(EmbedTask::Document.as_prefix(), "title: none | text: ");
    }
}
