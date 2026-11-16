//! Shared infrastructure for sandboxed native ONNX engines (`embed_engine` and `laya_engine`):
//! - Pinned SHA-256 artifact verification, atomic `.part` download, and HTTPS redirect streaming
//! - Candidate model/tokenizer/config path resolution across user, local, and container roots
//! - Length-prefixed bounded JSON IPC framing and child worker process client
//! - On-demand gateway supervisor with idle timeout and native memory release

use crate::adapters::CoreError;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::{
    future::Future,
    io::{Read, Write},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout},
    sync::mpsc as queue,
};
use tokio_util::sync::CancellationToken;

pub(crate) struct Artifact {
    pub name: &'static str,
    pub size: u64,
    pub hash: &'static str,
}

#[cfg(any(feature = "upstream-hyper", test))]
pub(crate) struct TemporaryFile(pub PathBuf);

#[cfg(any(feature = "upstream-hyper", test))]
impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub(crate) fn artifact_error(prefix: &str, e: impl std::fmt::Display) -> CoreError {
    CoreError::Internal(format!("{prefix}: {e}"))
}

pub(crate) async fn verify_artifact(
    path: &Path,
    artifact: &Artifact,
    err_prefix: &str,
) -> Result<(), CoreError> {
    let err = |e: & dyn std::fmt::Display| artifact_error(err_prefix, e);
    let mut file = tokio::fs::File::open(path).await.map_err(|e| err(&e))?;
    if file.metadata().await.map_err(|e| err(&e))?.len() != artifact.size {
        return Err(artifact_error(
            err_prefix,
            format!("{} has an unexpected size", path.display()),
        ));
    }
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let len = file.read(&mut buffer).await.map_err(|e| err(&e))?;
        if len == 0 {
            break;
        }
        hash.update(&buffer[..len]);
    }
    verify_digest(hash, artifact, err_prefix)
}

pub(crate) fn verify_digest(
    hash: Sha256,
    artifact: &Artifact,
    err_prefix: &str,
) -> Result<(), CoreError> {
    let actual: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if actual != artifact.hash {
        return Err(artifact_error(
            err_prefix,
            format!("SHA-256 mismatch for {}; refusing to load", artifact.name),
        ));
    }
    Ok(())
}

#[cfg(feature = "upstream-hyper")]
#[allow(clippy::too_many_arguments)] // 8 params: all semantically distinct, grouping would obscure the call sites
pub(crate) async fn download_artifact(
    path: &Path,
    artifact: &Artifact,
    remote_url: String,
    auto_download_env: &str,
    disabled_msg: &str,
    engine_label: &str,
    revision: &str,
    err_prefix: &str,
) -> Result<(), CoreError> {
    if std::env::var(auto_download_env).is_ok_and(|v| v == "0" || v == "false") {
        return Err(artifact_error(err_prefix, disabled_msg));
    }
    let parent = path
        .parent()
        .ok_or_else(|| artifact_error(err_prefix, "missing cache directory"))?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|e| artifact_error(err_prefix, e))?;
    let temporary = TemporaryFile(parent.join(format!(
        ".{}.{}.part",
        artifact.name,
        uuid::Uuid::new_v4()
    )));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temporary.0)
        .await
        .map_err(|e| artifact_error(err_prefix, e))?;
    tracing::info!(
        artifact = artifact.name,
        revision = revision,
        "Downloading {engine_label} on demand"
    );
    tokio::time::timeout(Duration::from_secs(600), async {
        let mut response = fetch_artifact_stream(remote_url, err_prefix).await?;
        let mut hash = Sha256::new();
        let mut size = 0u64;
        while let Some(chunk) = response
            .body
            .next_chunk()
            .await
            .map_err(|e| artifact_error(err_prefix, e))?
        {
            size = size
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| artifact_error(err_prefix, "size overflow"))?;
            if size > artifact.size {
                return Err(artifact_error(err_prefix, "download exceeds pinned size"));
            }
            hash.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|e| artifact_error(err_prefix, e))?;
        }
        if size != artifact.size {
            return Err(artifact_error(err_prefix, "incomplete artifact download"));
        }
        verify_digest(hash, artifact, err_prefix)?;
        file.sync_all()
            .await
            .map_err(|e| artifact_error(err_prefix, e))?;
        drop(file);
        tokio::fs::hard_link(&temporary.0, path)
            .await
            .map_err(|e| artifact_error(err_prefix, e))?;
        Ok(())
    })
    .await
    .map_err(|_| artifact_error(err_prefix, "download timed out"))?
}

#[cfg(not(feature = "upstream-hyper"))]
pub(crate) async fn download_artifact(
    _path: &Path,
    _artifact: &Artifact,
    _remote_url: String,
    _auto_download_env: &str,
    _disabled_msg: &str,
    _engine_label: &str,
    _revision: &str,
    err_prefix: &str,
) -> Result<(), CoreError> {
    Err(artifact_error(
        err_prefix,
        "automatic downloads require upstream-hyper",
    ))
}

#[cfg(feature = "upstream-hyper")]
async fn fetch_artifact_stream(
    mut url: String,
    err_prefix: &str,
) -> Result<crate::upstream::UpstreamResponse, CoreError> {
    use crate::upstream::{
        CancellationToken, ResolvedTimeouts, TimeoutProfile, UpstreamClient, UpstreamRequest,
    };
    let client = UpstreamClient::new();
    for _ in 0..6 {
        let uri: http::Uri = url.parse().map_err(|e| artifact_error(err_prefix, e))?;
        if uri.scheme_str() != Some("https") {
            return Err(artifact_error(err_prefix, "insecure artifact redirect"));
        }
        let mut req = UpstreamRequest::get(&url);
        req.is_streaming = true;
        let response = client
            .call(
                req,
                TimeoutProfile::Custom(ResolvedTimeouts {
                    headers_ms: 30000,
                    body_chunk_ms: 60000,
                    total_ms: 600000,
                    ..ResolvedTimeouts::SYSTEM_DEFAULTS
                }),
                CancellationToken::new(),
            )
            .await
            .map_err(|e| artifact_error(err_prefix, e))?;
        if response.status.is_success() {
            return Ok(response);
        }
        if !response.status.is_redirection() {
            return Err(artifact_error(
                err_prefix,
                format!("download HTTP {}", response.status),
            ));
        }
        let location = response
            .headers
            .get(http::header::LOCATION)
            .ok_or_else(|| artifact_error(err_prefix, "redirect without location"))?
            .to_str()
            .map_err(|e| artifact_error(err_prefix, e))?;
        url = if location.starts_with('/') {
            format!(
                "https://{}{location}",
                uri.authority()
                    .ok_or_else(|| artifact_error(err_prefix, "missing host"))?
            )
        } else {
            location.to_owned()
        };
    }
    Err(artifact_error(err_prefix, "too many artifact redirects"))
}

pub(crate) fn resolve_engine_candidate_path(
    model_subdir: &str,
    explicit: Option<&str>,
    env_var: &str,
    filename: &str,
    subpaths: &[&str],
    default_in_dot_openproxy: bool,
) -> String {
    if let Some(p) = explicit.filter(|s| !s.is_empty()) {
        return p.to_string();
    }
    if let Some(p) = std::env::var(env_var).ok().filter(|s| !s.is_empty()) {
        return p;
    }
    let mut candidates = Vec::new();
    for base in [
        std::env::var("HOME")
            .map(|h| format!("{h}/.openproxy/models/{model_subdir}"))
            .unwrap_or_default(),
        format!("./models/{model_subdir}"),
        format!("/var/lib/openproxy/models/{model_subdir}"),
    ] {
        if base.is_empty() {
            continue;
        }
        candidates.push(format!("{base}/{filename}"));
        for sub in subpaths {
            candidates.push(format!("{base}/{sub}"));
        }
    }
    for cand in &candidates {
        if Path::new(cand).exists() {
            return cand.clone();
        }
    }
    if default_in_dot_openproxy {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/.openproxy/models/{model_subdir}/{filename}");
        }
        return format!("./models/{model_subdir}/{filename}");
    }
    format!(
        "{}/models/{model_subdir}/{filename}",
        std::env::var("HOME").unwrap_or_else(|_| ".".into())
    )
}

pub(crate) fn ipc_internal(label: &str, e: impl std::fmt::Display) -> CoreError {
    CoreError::Internal(format!("{label} worker IPC: {e}"))
}

pub(crate) fn validate_ipc_length(
    len: usize,
    frame_limit: usize,
    invalid_msg: &str,
) -> Result<(), CoreError> {
    if len == 0 || len > frame_limit {
        return Err(CoreError::Validation(invalid_msg.into()));
    }
    Ok(())
}

pub(crate) fn encode_ipc_frame(
    value: &impl serde::Serialize,
    frame_limit: usize,
    label: &str,
    invalid_msg: &str,
) -> Result<Vec<u8>, CoreError> {
    let bytes = serde_json::to_vec(value).map_err(|e| ipc_internal(label, e))?;
    validate_ipc_length(bytes.len(), frame_limit, invalid_msg)?;
    Ok(bytes)
}

pub(crate) fn read_ipc_frame(
    reader: &mut impl Read,
    frame_limit: usize,
    label: &str,
    invalid_msg: &str,
) -> Result<Vec<u8>, CoreError> {
    let mut size = [0u8; 4];
    reader
        .read_exact(&mut size)
        .map_err(|e| ipc_internal(label, e))?;
    let len = u32::from_be_bytes(size) as usize;
    validate_ipc_length(len, frame_limit, invalid_msg)?;
    let mut bytes = vec![0; len];
    reader
        .read_exact(&mut bytes)
        .map_err(|e| ipc_internal(label, e))?;
    Ok(bytes)
}

pub(crate) fn write_ipc_frame(
    writer: &mut impl Write,
    value: &impl serde::Serialize,
    frame_limit: usize,
    label: &str,
    invalid_msg: &str,
) -> Result<(), CoreError> {
    let bytes = encode_ipc_frame(value, frame_limit, label, invalid_msg)?;
    writer
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .map_err(|e| ipc_internal(label, e))?;
    writer
        .write_all(&bytes)
        .map_err(|e| ipc_internal(label, e))?;
    writer.flush().map_err(|e| ipc_internal(label, e))
}

pub(crate) fn run_ipc_worker_loop<S, Req, Resp, InitFn, HandleFn, E>(
    frame_limit: usize,
    label: &str,
    invalid_msg: &str,
    init_fn: InitFn,
    handle_fn: HandleFn,
) -> Result<(), CoreError>
where
    S: serde::de::DeserializeOwned,
    Req: serde::de::DeserializeOwned,
    Resp: serde::Serialize,
    InitFn: FnOnce(&S) -> Result<E, CoreError>,
    HandleFn: Fn(&E, &Req) -> Result<Resp, CoreError>,
{
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let settings_bytes = read_ipc_frame(&mut input, frame_limit, label, invalid_msg)?;
    let settings: S =
        serde_json::from_slice(&settings_bytes).map_err(|e| ipc_internal(label, e))?;
    let engine = match init_fn(&settings) {
        Ok(engine) => engine,
        Err(error) => {
            write_ipc_frame(
                &mut output,
                &Result::<(), String>::Err(error.to_string()),
                frame_limit,
                label,
                invalid_msg,
            )?;
            return Err(error);
        }
    };
    write_ipc_frame(
        &mut output,
        &Result::<(), String>::Ok(()),
        frame_limit,
        label,
        invalid_msg,
    )?;
    loop {
        let Ok(frame) = read_ipc_frame(&mut input, frame_limit, label, invalid_msg) else {
            return Ok(());
        };
        let request: Req = serde_json::from_slice(&frame).map_err(|e| ipc_internal(label, e))?;
        let result = handle_fn(&engine, &request).map_err(|e| e.to_string());
        write_ipc_frame(&mut output, &result, frame_limit, label, invalid_msg)?;
    }
}

pub(crate) struct IpcWorkerClient {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    frame_limit: usize,
    label: &'static str,
    invalid_msg: &'static str,
}

impl Drop for IpcWorkerClient {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

impl IpcWorkerClient {
    pub async fn stop(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }

    pub async fn start<S: serde::Serialize>(
        settings: &S,
        worker_arg: &str,
        env_whitelist: &[&str],
        startup_deadline: Duration,
        frame_limit: usize,
        label: &'static str,
        invalid_msg: &'static str,
    ) -> Result<Self, CoreError> {
        let executable = std::env::current_exe().map_err(|e| ipc_internal(label, e))?;
        #[cfg(test)]
        let executable = executable
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| ipc_internal(label, "missing test binary directory"))?
            .join("openproxy");
        let mut command = tokio::process::Command::new(executable);
        command
            .arg(worker_arg)
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        for &name in env_whitelist {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = command.spawn().map_err(|e| ipc_internal(label, e))?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| ipc_internal(label, "missing stdin"))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| ipc_internal(label, "missing stdout"))?;
        let mut client = Self {
            child,
            input,
            output,
            frame_limit,
            label,
            invalid_msg,
        };
        let ready: Result<(), String> = client.exchange(settings, startup_deadline).await?;
        ready.map_err(|e| ipc_internal(label, e))?;
        Ok(client)
    }

    pub async fn call<Req: serde::Serialize, Resp: serde::de::DeserializeOwned>(
        &mut self,
        request: &Req,
        deadline: Duration,
    ) -> Result<Resp, CoreError> {
        let response: Result<Resp, String> = self.exchange(request, deadline).await?;
        response.map_err(|e| ipc_internal(self.label, e))
    }

    pub async fn exchange<T: serde::de::DeserializeOwned>(
        &mut self,
        value: &impl serde::Serialize,
        deadline: Duration,
    ) -> Result<T, CoreError> {
        let bytes = encode_ipc_frame(value, self.frame_limit, self.label, self.invalid_msg)?;
        let label = self.label;
        let frame_limit = self.frame_limit;
        let invalid_msg = self.invalid_msg;
        tokio::time::timeout(deadline, async {
            self.input
                .write_u32(bytes.len() as u32)
                .await
                .map_err(|e| ipc_internal(label, e))?;
            self.input
                .write_all(&bytes)
                .await
                .map_err(|e| ipc_internal(label, e))?;
            self.input
                .flush()
                .await
                .map_err(|e| ipc_internal(label, e))?;
            let len = self
                .output
                .read_u32()
                .await
                .map_err(|e| ipc_internal(label, e))? as usize;
            validate_ipc_length(len, frame_limit, invalid_msg)?;
            let mut response = vec![0; len];
            self.output
                .read_exact(&mut response)
                .await
                .map_err(|e| ipc_internal(label, e))?;
            serde_json::from_slice(&response).map_err(|e| ipc_internal(label, e))
        })
        .await
        .map_err(|_| ipc_internal(label, "deadline exceeded"))?
    }
}

pub(crate) struct SupervisorJob<Req, Resp> {
    pub request: Option<Req>,
    pub reply: mpsc::SyncSender<Result<Option<Resp>, CoreError>>,
}

struct SupervisorManager<Req, Resp> {
    sender: queue::Sender<SupervisorJob<Req, Resp>>,
    cancel: CancellationToken,
    thread: std::thread::JoinHandle<()>,
}

pub(crate) struct ResidentWorker {
    pub client: IpcWorkerClient,
    resident_counter: &'static AtomicUsize,
}

impl ResidentWorker {
    pub fn new(client: IpcWorkerClient, resident_counter: &'static AtomicUsize) -> Self {
        resident_counter.fetch_add(1, Ordering::AcqRel);
        Self {
            client,
            resident_counter,
        }
    }
}

impl Drop for ResidentWorker {
    fn drop(&mut self) {
        self.resident_counter.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) struct EngineSupervisor<Req: 'static, Resp: 'static> {
    enabled: AtomicBool,
    pub resident: AtomicUsize,
    manager: Mutex<Option<SupervisorManager<Req, Resp>>>,
    label: &'static str,
    thread_name: &'static str,
    idle_env: &'static str,
    recv_timeout: Duration,
}

impl<Req: Send + 'static, Resp: Send + 'static> EngineSupervisor<Req, Resp> {
    pub const fn new(
        label: &'static str,
        thread_name: &'static str,
        idle_env: &'static str,
        recv_timeout: Duration,
    ) -> Self {
        Self {
            enabled: AtomicBool::new(false),
            resident: AtomicUsize::new(0),
            manager: Mutex::new(None),
            label,
            thread_name,
            idle_env,
            recv_timeout,
        }
    }

    pub fn is_available(&self) -> bool {
        self.resident.load(Ordering::Acquire) != 0
    }

    pub fn is_enabled(&self) -> bool {
        cfg!(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )) && self.enabled.load(Ordering::Acquire)
    }

    pub fn shutdown(&self) {
        self.enabled.store(false, Ordering::Release);
        let manager = self.manager.lock().take();
        if let Some(manager) = manager {
            manager.cancel.cancel();
            let _ = manager.thread.join();
        }
    }

    pub fn spawn_init_background(&self) {
        self.enabled.store(true, Ordering::Release);
    }

    pub fn idle_timeout(&self) -> Duration {
        let minutes = std::env::var(self.idle_env)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| (1..=1440).contains(v))
            .unwrap_or(5);
        Duration::from_secs(minutes * 60)
    }

    pub fn submit<S, RunFn>(
        &'static self,
        request: Option<Req>,
        settings: S,
        run_job_fn: RunFn,
    ) -> Result<Option<Resp>, CoreError>
    where
        S: Send + Sync + 'static,
        RunFn: for<'a> Fn(
                &'a mut Option<ResidentWorker>,
                &'a S,
                Option<&'a Req>,
            ) -> Pin<
                Box<dyn Future<Output = Result<Option<Resp>, CoreError>> + Send + 'a>,
            > + Send
            + Sync
            + Copy
            + 'static,
    {
        let label = self.label;
        let (reply, receiver) = mpsc::sync_channel(1);
        {
            let mut manager = self.manager.lock();
            if !self.is_enabled() {
                return Err(CoreError::Internal(format!("{label} engine is disabled")));
            }
            if manager.as_ref().is_none_or(|m| m.sender.is_closed()) {
                *manager = Some(self.start_manager(settings, run_job_fn)?);
            }
            let sender = &manager
                .as_ref()
                .ok_or_else(|| CoreError::Internal(format!("{label} supervisor unavailable")))?
                .sender;
            sender
                .try_send(SupervisorJob { request, reply })
                .map_err(|e| CoreError::Internal(format!("{label} queue full or closed: {e}")))?;
        }
        receiver.recv_timeout(self.recv_timeout).map_err(|e| {
            CoreError::Internal(format!("{label} worker stopped or timed out: {e}"))
        })?
    }

    fn start_manager<S, RunFn>(
        &'static self,
        settings: S,
        run_job_fn: RunFn,
    ) -> Result<SupervisorManager<Req, Resp>, CoreError>
    where
        S: Send + Sync + 'static,
        RunFn: for<'a> Fn(
                &'a mut Option<ResidentWorker>,
                &'a S,
                Option<&'a Req>,
            ) -> Pin<
                Box<dyn Future<Output = Result<Option<Resp>, CoreError>> + Send + 'a>,
            > + Send
            + Sync
            + Copy
            + 'static,
    {
        let label = self.label;
        let idle = self.idle_timeout();
        let (sender, receiver) = queue::channel(8);
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let thread = std::thread::Builder::new()
            .name(self.thread_name.into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => runtime.block_on(self.supervise(
                        receiver,
                        token,
                        settings,
                        idle,
                        run_job_fn,
                    )),
                    Err(e) => {
                        tracing::error!(error = %e, "Cannot start {label} supervisor runtime")
                    }
                }
            })
            .map_err(|e| CoreError::Internal(format!("Cannot start {label} supervisor: {e}")))?;
        Ok(SupervisorManager {
            sender,
            cancel,
            thread,
        })
    }

    pub async fn supervise<S, RunFn>(
        &'static self,
        mut receiver: queue::Receiver<SupervisorJob<Req, Resp>>,
        cancel: CancellationToken,
        settings: S,
        idle: Duration,
        run_job_fn: RunFn,
    ) where
        RunFn: for<'a> Fn(
            &'a mut Option<ResidentWorker>,
            &'a S,
            Option<&'a Req>,
        ) -> Pin<
            Box<dyn Future<Output = Result<Option<Resp>, CoreError>> + Send + 'a>,
        >,
    {
        let label = self.label;
        let mut resident: Option<ResidentWorker> = None;
        loop {
            let job = tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                job = tokio::time::timeout(idle, receiver.recv()) => match job {
                    Ok(Some(job)) => job,
                    Ok(None) => break,
                    Err(_) => {
                        if resident.is_some() {
                            tracing::info!("{label} idle timeout: stopping worker and releasing memory");
                            stop_resident_worker(&mut resident).await;
                        }
                        continue;
                    }
                }
            };
            let result = tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                result = run_job_fn(&mut resident, &settings, job.request.as_ref()) => result,
            };
            if result.is_err() {
                stop_resident_worker(&mut resident).await;
            }
            let _ = job.reply.send(result);
        }
        stop_resident_worker(&mut resident).await;
    }
}

async fn stop_resident_worker(resident: &mut Option<ResidentWorker>) {
    if let Some(mut worker) = resident.take() {
        worker.client.stop().await;
    }
}

macro_rules! define_engine_lifecycle {
    (
        label: $label:literal,
        thread_name: $thread_name:literal,
        idle_env: $idle_env:literal,
        recv_timeout_secs: $recv_timeout_secs:expr,
        call_timeout_secs: $call_timeout_secs:expr,
        unavail_msg: $unavail_msg:literal,
        req_ty: $req_ty:ty,
        resp_ty: $resp_ty:ty,
        settings_ty: $settings_ty:ty,
        ensure_fn: $ensure_fn:path,
        client_ty: $client_ty:path
    ) => {
        static SUPERVISOR: $crate::adapters::native_engine_common::EngineSupervisor<
            $req_ty,
            $resp_ty,
        > = $crate::adapters::native_engine_common::EngineSupervisor::new(
            $label,
            $thread_name,
            $idle_env,
            std::time::Duration::from_secs($recv_timeout_secs),
        );

        /// Resident state, distinct from whether lazy inference is enabled.
        pub fn is_available() -> bool {
            SUPERVISOR.is_available()
        }

        pub fn is_enabled() -> bool {
            SUPERVISOR.is_enabled()
        }

        pub fn shutdown() {
            SUPERVISOR.shutdown();
        }

        /// Enable lazy inference; activating a provider does not download or load it.
        pub fn spawn_init_background() {
            SUPERVISOR.spawn_init_background();
        }

        /// Explicit warm-up for callers that need it.
        pub fn init(
            model: Option<&str>,
            tokenizer: Option<&str>,
            config: Option<&str>,
            threads: Option<usize>,
        ) -> Result<(), CoreError> {
            spawn_init_background();
            // A `ty` fragment can't be followed by a struct literal; alias it first.
            type EngineSettings = $settings_ty;
            submit(
                None,
                EngineSettings {
                    model: model.map(str::to_owned),
                    tokenizer: tokenizer.map(str::to_owned),
                    config: config.map(str::to_owned),
                    threads,
                },
            )?;
            Ok(())
        }

        fn submit(
            request: Option<$req_ty>,
            settings: $settings_ty,
        ) -> Result<Option<$resp_ty>, CoreError> {
            SUPERVISOR.submit(request, settings, run_job_boxed)
        }

        fn run_job_boxed<'a>(
            resident: &'a mut Option<$crate::adapters::native_engine_common::ResidentWorker>,
            settings: &'a $settings_ty,
            request: Option<&'a $req_ty>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Option<$resp_ty>, CoreError>> + Send + 'a>,
        > {
            Box::pin(run_job(resident, settings, request))
        }

        async fn run_job(
            resident: &mut Option<$crate::adapters::native_engine_common::ResidentWorker>,
            settings: &$settings_ty,
            request: Option<&$req_ty>,
        ) -> Result<Option<$resp_ty>, CoreError> {
            if resident.is_none() {
                $ensure_fn(settings).await?;
                let client = <$client_ty>::start(settings).await?;
                *resident = Some(
                    $crate::adapters::native_engine_common::ResidentWorker::new(
                        client.0,
                        &SUPERVISOR.resident,
                    ),
                );
            }
            let Some(request) = request else {
                return Ok(None);
            };
            let worker = resident
                .as_mut()
                .ok_or_else(|| CoreError::Internal($unavail_msg.into()))?;
            worker
                .client
                .call(request, std::time::Duration::from_secs($call_timeout_secs))
                .await
                .map(Some)
        }
    };
}
pub(crate) use define_engine_lifecycle;

#[cfg(test)]
macro_rules! define_supervisor_tests {
    ($req_ty:ty, $resp_ty:ty, $settings_ty:ty, $ignore_msg:literal) => {
        #[tokio::test]
        async fn supervisor_cancellation_closes_queue_without_loading() {
            let (sender, receiver) = tokio::sync::mpsc::channel(8);
            let cancel = tokio_util::sync::CancellationToken::new();
            cancel.cancel();
            SUPERVISOR
                .supervise(
                    receiver,
                    cancel,
                    <$settings_ty>::default(),
                    std::time::Duration::from_secs(300),
                    run_job_boxed,
                )
                .await;
            assert!(sender.is_closed());
        }

        #[test]
        fn queue_is_bounded_and_disconnected_replies_do_not_panic() {
            use $crate::adapters::native_engine_common::SupervisorJob as Job;
            let (sender, _receiver) =
                tokio::sync::mpsc::channel::<Job<$req_ty, $resp_ty>>(8);
            for _ in 0..8 {
                let (reply, _) = std::sync::mpsc::sync_channel(1);
                assert!(sender.try_send(Job { request: None, reply }).is_ok());
            }
            let (reply, _) = std::sync::mpsc::sync_channel(1);
            assert!(sender.try_send(Job { request: None, reply }).is_err());
        }

        #[tokio::test]
        #[ignore = $ignore_msg]
        async fn idle_unloads_and_next_request_reloads() {
            use $crate::adapters::native_engine_common::SupervisorJob as Job;
            let (sender, receiver) = tokio::sync::mpsc::channel(8);
            let cancel = tokio_util::sync::CancellationToken::new();
            let token = cancel.clone();
            let supervisor = tokio::spawn(SUPERVISOR.supervise(
                receiver,
                token,
                <$settings_ty>::default(),
                std::time::Duration::from_millis(100),
                run_job_boxed,
            ));
            for _ in 0..2 {
                let (reply, response) = std::sync::mpsc::sync_channel(1);
                sender.send(Job { request: None, reply }).await.unwrap();
                let result = tokio::task::spawn_blocking(move || response.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(result.is_ok(), "{result:?}");
                assert!(is_available());
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    while is_available() {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap();
            }
            cancel.cancel();
            supervisor.await.unwrap();
            assert!(sender.is_closed());
        }
    };
}
#[cfg(test)]
pub(crate) use define_supervisor_tests;

#[cfg(test)]
macro_rules! define_ipc_frame_tests {
    () => {
        #[test]
        fn frames_reject_oversize_truncation_and_zero() {
            for len in [0, FRAME_LIMIT + 1] {
                assert!(read_frame(&mut &(len as u32).to_be_bytes()[..]).is_err());
            }
            assert!(read_frame(&mut &[0, 0, 0, 2, b'{'][..]).is_err());
        }

        #[test]
        fn frames_roundtrip_unicode() {
            let mut bytes = Vec::new();
            write_frame(&mut bytes, &"¿日本語 🦀?").unwrap();
            let frame = read_frame(&mut bytes.as_slice()).unwrap();
            assert_eq!(
                serde_json::from_slice::<String>(&frame).unwrap(),
                "¿日本語 🦀?"
            );
        }
    };
}
#[cfg(test)]
pub(crate) use define_ipc_frame_tests;
