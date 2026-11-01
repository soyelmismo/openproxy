//! Native ONNX embeddings engine (EmbeddingGemma 2).
//!
//! Mirrors the laya engine: an isolated worker process (`--embed-worker`)
//! holds the ONNX Runtime sessions under landlock + seccomp confinement, and
//! the gateway talks to it over a length-prefixed JSON IPC channel. Memory is
//! lazy: the worker only exists while requests keep arriving, and an idle
//! timeout releases it.
//!
//! Unlike laya, the model is multimodal. The vision and audio towers emit
//! 512-dim features that the text backbone consumes as extra inputs, so one
//! embedding space covers text, images and audio. The exported graphs perform
//! mean pooling and L2 normalization internally: `sentence_embedding` is
//! `[batch, 768]` and is the final embedding.

use openproxy_types::error::CoreError;
mod assets;
mod lifecycle;
pub mod preprocess;
mod worker;
pub use lifecycle::{
    execute_embedding, execute_text_embedding, init, is_available, is_enabled, shutdown,
    spawn_init_background,
};
pub use worker::run_worker;

pub use preprocess::{AudioSettings, ImageSettings};
use std::{ffi::CString, sync::OnceLock};
use tokenizers::Tokenizer;

#[repr(C)]
struct EmbedSessionOpaque {
    _private: [u8; 0],
}

#[repr(C)]
struct VisionSessionOpaque {
    _private: [u8; 0],
}

#[repr(C)]
struct AudioSessionOpaque {
    _private: [u8; 0],
}

/// Feature width emitted by the vision and audio towers and consumed by the
/// text backbone. Kept in sync with `EMBED_FEATURE_DIM` in onnx_bridge.h.
const FEATURE_DIM: usize = 512;

unsafe extern "C" {
    fn embed_session_create(
        onnx_lib_path: *const std::ffi::c_char,
        model_path: *const std::ffi::c_char,
        num_threads: std::ffi::c_int,
        err_buf: *mut std::ffi::c_char,
        err_buf_len: usize,
    ) -> *mut EmbedSessionOpaque;

    fn embed_session_destroy(session: *mut EmbedSessionOpaque);

    fn embed_session_embedding_dim(session: *mut EmbedSessionOpaque) -> std::ffi::c_int;

    fn embed_session_run(
        session: *mut EmbedSessionOpaque,
        batch_size: i64,
        seq_len: i64,
        input_ids: *const i64,
        attention_mask: *const i64,
        image_features: *const f32,
        image_feature_rows: i64,
        video_features: *const f32,
        video_feature_rows: i64,
        audio_features: *const f32,
        audio_feature_rows: i64,
        out_embeddings: *mut f32,
        err_buf: *mut std::ffi::c_char,
        err_buf_len: usize,
    ) -> std::ffi::c_int;

    fn vision_session_create(
        onnx_lib_path: *const std::ffi::c_char,
        model_path: *const std::ffi::c_char,
        num_threads: std::ffi::c_int,
        err_buf: *mut std::ffi::c_char,
        err_buf_len: usize,
    ) -> *mut VisionSessionOpaque;

    fn vision_session_destroy(session: *mut VisionSessionOpaque);

    fn vision_session_run(
        session: *mut VisionSessionOpaque,
        batch_size: i64,
        num_patches: i64,
        pixel_values: *const f32,
        position_ids: *const i64,
        out_features: *mut f32,
        out_rows: *mut i64,
        err_buf: *mut std::ffi::c_char,
        err_buf_len: usize,
    ) -> std::ffi::c_int;

    fn audio_session_create(
        onnx_lib_path: *const std::ffi::c_char,
        model_path: *const std::ffi::c_char,
        num_threads: std::ffi::c_int,
        err_buf: *mut std::ffi::c_char,
        err_buf_len: usize,
    ) -> *mut AudioSessionOpaque;

    fn audio_session_destroy(session: *mut AudioSessionOpaque);

    fn audio_session_run(
        session: *mut AudioSessionOpaque,
        batch_size: i64,
        seq_len: i64,
        input_features: *const f32,
        input_features_mask: *const u8,
        out_features: *mut f32,
        out_rows: *mut i64,
        err_buf: *mut std::ffi::c_char,
        err_buf_len: usize,
    ) -> std::ffi::c_int;

    /// Shared worker confinement. `allow_model_dir` grants read access to the
    /// model's parent directory (external `.onnx_data` weights beside the
    /// graph files).
    fn worker_sandbox(
        model: *const std::ffi::c_char,
        tokenizer: *const std::ffi::c_char,
        config: *const std::ffi::c_char,
        allow_model_dir: std::ffi::c_int,
    ) -> std::ffi::c_int;
}

/// Task prefix per EmbeddingGemma's `config_sentence_transformers.json`.
/// Queries and documents must be embedded with their own prompt, or
/// retrieval quality collapses. Prefixes apply to text only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbedTask {
    /// `task: search result | query: ` — retrieval/search-side text.
    Query,
    /// `title: none | text: ` — corpus/document-side text.
    Document,
}

impl EmbedTask {
    pub fn as_prefix(self) -> &'static str {
        match self {
            EmbedTask::Query => "task: search result | query: ",
            EmbedTask::Document => "title: none | text: ",
        }
    }
}

/// One item of a multimodal batch. Images arrive as decoded RGB8 and audio as
/// mono 16 kHz PCM, so the worker never runs an image or audio decoder.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EmbedInput {
    Text {
        text: String,
    },
    Image {
        width: u32,
        height: u32,
        /// Row-major RGB, `width * height * 3` bytes.
        rgb: Vec<u8>,
    },
    Audio {
        /// Mono samples at 16 kHz, nominally in [-1, 1].
        samples: Vec<f32>,
    },
}

/// IPC request: a batch of inputs to embed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EmbedRequest {
    pub inputs: Vec<EmbedInput>,
    pub task: EmbedTask,
    /// Truncate to this many dimensions (Matryoshka). 0 or absent = full.
    #[serde(default)]
    pub dimensions: usize,
}

/// IPC response: flat row-major embeddings plus the active dimension.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EmbedResponse {
    pub embedding_dim: usize,
    pub embeddings: Vec<f32>,
}

/// Settings forwarded to the worker on spawn.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Settings {
    pub model: Option<String>,
    pub tokenizer: Option<String>,
    pub config: Option<String>,
    pub threads: Option<usize>,
}

/// Build an IPC request.
pub(crate) fn build_request(
    inputs: Vec<EmbedInput>,
    task: EmbedTask,
    dimensions: usize,
) -> EmbedRequest {
    EmbedRequest {
        inputs,
        task,
        dimensions,
    }
}

/// Map a flat embedding vector list back into per-input vectors.
pub(crate) fn split_rows(
    response: &EmbedResponse,
    expected: usize,
) -> Result<Vec<Vec<f32>>, CoreError> {
    let dim = response.embedding_dim;
    if dim == 0 || response.embeddings.len() != dim * expected {
        return Err(CoreError::Internal(format!(
            "embed engine returned {} floats for {expected} inputs at dim {dim}",
            response.embeddings.len()
        )));
    }
    Ok(response
        .embeddings
        .chunks_exact(dim)
        .map(<[f32]>::to_vec)
        .collect())
}

struct SafeEmbed(*mut EmbedSessionOpaque);
struct SafeVision(*mut VisionSessionOpaque);
struct SafeAudio(*mut AudioSessionOpaque);

unsafe impl Send for SafeEmbed {}
unsafe impl Sync for SafeEmbed {}
unsafe impl Send for SafeVision {}
unsafe impl Sync for SafeVision {}
unsafe impl Send for SafeAudio {}
unsafe impl Sync for SafeAudio {}

impl Drop for SafeEmbed {
    fn drop(&mut self) {
        unsafe { embed_session_destroy(self.0) };
    }
}

impl Drop for SafeVision {
    fn drop(&mut self) {
        unsafe { vision_session_destroy(self.0) };
    }
}

impl Drop for SafeAudio {
    fn drop(&mut self) {
        unsafe { audio_session_destroy(self.0) };
    }
}

/// Loaded engine: tokenizer plus one ORT session per modality tower.
pub struct EmbedEngine {
    text: SafeEmbed,
    vision: SafeVision,
    audio: SafeAudio,
    tokenizer: Tokenizer,
    pad_id: u32,
    max_len: usize,
    image: ImageSettings,
    audio_settings: AudioSettings,
}

fn resolve_candidate_path(
    explicit: Option<&str>,
    env_var: &str,
    filename: &str,
    subpaths: &[&str],
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
            .map(|h| format!("{h}/.openproxy/models/embeddinggemma2"))
            .unwrap_or_default(),
        "./models/embeddinggemma2".to_string(),
        "/var/lib/openproxy/models/embeddinggemma2".to_string(),
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
        if std::path::Path::new(cand).exists() {
            return cand.clone();
        }
    }
    format!(
        "{}/models/embeddinggemma2/{filename}",
        std::env::var("HOME").unwrap_or_else(|_| ".".into())
    )
}

pub fn resolve_model_path(opt: Option<&str>) -> String {
    resolve_candidate_path(
        opt,
        "OPENPROXY_EMBED_MODEL",
        "model_q4f16.onnx",
        &[
            "model.onnx",
            "model_quantized.onnx",
            "model_fp16.onnx",
            "model_q4.onnx",
        ],
    )
}

pub fn resolve_tokenizer_path(opt: Option<&str>) -> String {
    resolve_candidate_path(opt, "OPENPROXY_EMBED_TOKENIZER", "tokenizer.json", &[])
}

pub fn resolve_config_path(opt: Option<&str>) -> String {
    // Optional: engine tuning (max_len). Absence is fine.
    resolve_candidate_path(opt, "OPENPROXY_EMBED_CONFIG", "engine_config.json", &[])
}

/// Vision and audio towers live beside the text graph in the same cache root.
pub fn resolve_vision_path(opt: Option<&str>) -> String {
    resolve_sibling_path(
        opt,
        "OPENPROXY_EMBED_VISION_MODEL",
        "vision_encoder_q4f16.onnx",
    )
}

pub fn resolve_audio_path(opt: Option<&str>) -> String {
    resolve_sibling_path(
        opt,
        "OPENPROXY_EMBED_AUDIO_MODEL",
        "audio_encoder_q4f16.onnx",
    )
}

/// Resolve a modality tower that ships beside the text graph.
///
/// `opt` names the *text* model file. The vision and audio towers are separate
/// files in the same directory, so handing `opt` straight to
/// `resolve_candidate_path` would load the text graph three times: the tower
/// session then has no modality-specific input and ORT fails the first Run
/// with "Invalid input name". Derive the tower path from the text model's
/// directory instead, and fall back to the normal search when it is absent.
fn resolve_sibling_path(explicit: Option<&str>, env_var: &str, filename: &str) -> String {
    if let Some(p) = std::env::var(env_var).ok().filter(|s| !s.is_empty()) {
        return p;
    }
    if let Some(parent) = explicit
        .filter(|s| !s.is_empty())
        .and_then(|p| std::path::Path::new(p).parent())
        .filter(|p| !p.as_os_str().is_empty())
    {
        let candidate = parent.join(filename);
        if candidate.exists() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    resolve_candidate_path(None, env_var, filename, &[])
}

pub fn is_model_installed() -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(|| std::path::Path::new(&resolve_model_path(None)).exists())
}

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct EmbedConfig {
    #[serde(default = "default_max_len")]
    pub max_len: usize,
    #[serde(default)]
    pub image: Option<ImageSettings>,
    #[serde(default)]
    pub audio: Option<AudioSettings>,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            max_len: default_max_len(),
            image: None,
            audio: None,
        }
    }
}

fn default_max_len() -> usize {
    2048
}

pub(crate) fn load_engine(
    model_path_opt: Option<&str>,
    tokenizer_path_opt: Option<&str>,
    config_path_opt: Option<&str>,
    num_threads_opt: Option<usize>,
) -> Result<EmbedEngine, CoreError> {
    let model_path = resolve_model_path(model_path_opt);
    let vision_path = resolve_vision_path(model_path_opt);
    let audio_path = resolve_audio_path(model_path_opt);
    let tokenizer_path = resolve_tokenizer_path(tokenizer_path_opt);
    let config_path = resolve_config_path(config_path_opt);

    let num_threads = num_threads_opt
        .or_else(|| {
            std::env::var("OPENPROXY_EMBED_THREADS")
                .ok()
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(2, |n| n.get().clamp(1, 4)));

    tracing::info!(
        model = %model_path,
        vision = %vision_path,
        audio = %audio_path,
        tokenizer = %tokenizer_path,
        threads = num_threads,
        "Initializing multimodal embed engine in isolated worker"
    );

    let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
        CoreError::Internal(format!(
            "Failed to load tokenizer from {tokenizer_path}: {e}"
        ))
    })?;
    let pad_id = tokenizer
        .token_to_id("<pad>")
        .or_else(|| tokenizer.token_to_id("[PAD]"))
        .unwrap_or(0);

    let config: EmbedConfig = if let Ok(content) = std::fs::read_to_string(&config_path) {
        serde_json::from_str(&content)
            .map_err(|e| CoreError::Validation(format!("Invalid embed config: {e}")))?
    } else {
        EmbedConfig::default()
    };
    if !(16..=8192).contains(&config.max_len) {
        return Err(CoreError::Validation(
            "embed max_len must be in 16..=8192".into(),
        ));
    }
    let image = config.image.unwrap_or_default();
    let audio_settings = config.audio.unwrap_or_default();
    let num_threads = std::ffi::c_int::try_from(num_threads)
        .ok()
        .filter(|n| (1..=64).contains(n))
        .ok_or_else(|| CoreError::Validation("embed threads must be in 1..=64".into()))?;

    let c_model = CString::new(model_path)
        .map_err(|e| CoreError::Validation(format!("Invalid model path: {e}")))?;
    let c_vision = CString::new(vision_path)
        .map_err(|e| CoreError::Validation(format!("Invalid vision path: {e}")))?;
    let c_audio = CString::new(audio_path)
        .map_err(|e| CoreError::Validation(format!("Invalid audio path: {e}")))?;
    let c_tokenizer = CString::new(tokenizer_path)
        .map_err(|e| CoreError::Validation(format!("Invalid tokenizer path: {e}")))?;

    let mut err_buf = vec![0u8; 1024];
    let text_ptr = unsafe {
        embed_session_create(
            std::ptr::null(),
            c_model.as_ptr(),
            num_threads,
            err_buf.as_mut_ptr().cast::<std::ffi::c_char>(),
            err_buf.len(),
        )
    };
    if text_ptr.is_null() {
        return Err(CoreError::Internal(format!(
            "Failed to initialize embed ONNX session: {}",
            ffi_error(&err_buf)
        )));
    }

    let vision_ptr = unsafe {
        vision_session_create(
            std::ptr::null(),
            c_vision.as_ptr(),
            num_threads,
            err_buf.as_mut_ptr().cast::<std::ffi::c_char>(),
            err_buf.len(),
        )
    };
    if vision_ptr.is_null() {
        unsafe { embed_session_destroy(text_ptr) };
        return Err(CoreError::Internal(format!(
            "Failed to initialize vision ONNX session: {}",
            ffi_error(&err_buf)
        )));
    }

    let audio_ptr = unsafe {
        audio_session_create(
            std::ptr::null(),
            c_audio.as_ptr(),
            num_threads,
            err_buf.as_mut_ptr().cast::<std::ffi::c_char>(),
            err_buf.len(),
        )
    };
    if audio_ptr.is_null() {
        unsafe { embed_session_destroy(text_ptr) };
        unsafe { vision_session_destroy(vision_ptr) };
        return Err(CoreError::Internal(format!(
            "Failed to initialize audio ONNX session: {}",
            ffi_error(&err_buf)
        )));
    }

    let engine = EmbedEngine {
        text: SafeEmbed(text_ptr),
        vision: SafeVision(vision_ptr),
        audio: SafeAudio(audio_ptr),
        tokenizer,
        pad_id,
        max_len: config.max_len.min(8192),
        image,
        audio_settings,
    };
    if engine.embedding_dim() == 0 {
        return Err(CoreError::Internal(
            "embed engine could not discover the embedding dimension".into(),
        ));
    }

    // Sandbox AFTER session creation: ORT dlopen()s the runtime and reads its
    // config during create, so the dynamic loader needs those paths. Once the
    // sessions exist the worker only needs the model directory (external
    // `.onnx_data` weights) for the weight mapping.
    let c_config = CString::new(config_path)
        .map_err(|e| CoreError::Validation(format!("Invalid config path: {e}")))?;
    let rc =
        unsafe { worker_sandbox(c_model.as_ptr(), c_tokenizer.as_ptr(), c_config.as_ptr(), 1) };
    if rc != 0 {
        return Err(CoreError::Internal(format!(
            "sandbox unavailable (code {rc}); refusing native inference"
        )));
    }

    Ok(engine)
}

fn ffi_error(buffer: &[u8]) -> String {
    let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}

/// Prepared tensors for one batch.
pub(crate) struct Prepared {
    pub input_ids: Vec<i64>,
    pub attention_mask: Vec<i64>,
    pub batch: usize,
    pub seq_len: usize,
    pub image_features: Vec<f32>,
    pub audio_features: Vec<f32>,
}

impl EmbedEngine {
    /// Full embedding dimension of the loaded model.
    pub fn embedding_dim(&self) -> usize {
        let dim = unsafe { embed_session_embedding_dim(self.text.0) };
        dim.max(0) as usize
    }

    /// Turn a multimodal batch into the tensors the backbone consumes.
    ///
    /// Each image and audio item expands into its placeholder token run, and
    /// its features are appended in the same order, which is what keeps
    /// modalities aligned inside a mixed batch.
    pub(crate) fn prepare(&self, request: &EmbedRequest) -> Result<Prepared, CoreError> {
        let batch = request.inputs.len();
        if batch == 0 || batch > 64 {
            return Err(CoreError::Validation(
                "embed batch must be 1..=64 inputs".into(),
            ));
        }
        if self.embedding_dim() == 0 {
            return Err(CoreError::Internal("embed dimension unknown".into()));
        }

        let mut sequences: Vec<Vec<u32>> = Vec::with_capacity(batch);
        let mut image_features: Vec<f32> = Vec::new();
        let mut audio_features: Vec<f32> = Vec::new();

        for input in &request.inputs {
            let mut ids: Vec<u32> = Vec::new();
            match input {
                EmbedInput::Text { text } => {
                    let prefix = request.task.as_prefix();
                    let encoding = self
                        .tokenizer
                        .encode(format!("{prefix}{text}").as_str(), false)
                        .map_err(|e| CoreError::Internal(format!("tokenizer encode error: {e}")))?;
                    ids.extend(encoding.get_ids().iter().take(self.max_len).copied());
                    if ids.is_empty() {
                        return Err(CoreError::Validation(
                            "text produced no tokens; refusing to embed empty input".into(),
                        ));
                    }
                }
                EmbedInput::Image { width, height, rgb } => {
                    let (pixels, positions, soft_tokens) =
                        preprocess::image_tensors(self.image, *width, *height, rgb)?;
                    // The graph is fed exactly the patches the preprocessing
                    // produced, so the row count must come from the tensor.
                    let patches = pixels.len() / preprocess::IMAGE_PATCH_PIXELS;
                    let mut features = vec![0.0f32; soft_tokens * FEATURE_DIM];
                    let mut rows: i64 = 0;
                    let mut err_buf = vec![0u8; 1024];
                    let rc = unsafe {
                        vision_session_run(
                            self.vision.0,
                            1,
                            patches as i64,
                            pixels.as_ptr(),
                            positions.as_ptr(),
                            features.as_mut_ptr(),
                            &raw mut rows,
                            err_buf.as_mut_ptr().cast::<std::ffi::c_char>(),
                            err_buf.len(),
                        )
                    };
                    if rc != 0 {
                        return Err(CoreError::Internal(format!(
                            "vision inference failed (code {rc}): {}",
                            ffi_error(&err_buf)
                        )));
                    }
                    let rows = usize::try_from(rows)
                        .map_err(|_| CoreError::Internal("negative vision row count".into()))?;
                    if rows == 0 || rows > soft_tokens {
                        return Err(CoreError::Internal(format!(
                            "vision tower returned {rows} feature rows for {soft_tokens} soft tokens"
                        )));
                    }
                    features.truncate(rows * FEATURE_DIM);
                    ids.extend(preprocess::image_placeholders(rows));
                    image_features.extend_from_slice(&features);
                }
                EmbedInput::Audio { samples } => {
                    let (spectrogram, mask, frames) =
                        preprocess::audio_features(self.audio_settings, samples)?;
                    // The tower's row count must equal the placeholder run the
                    // reference processor derives from the mask, otherwise the
                    // backbone receives misaligned modalities. Checked here,
                    // before the mask is narrowed to bytes for ORT.
                    let predicted = preprocess::audio_soft_tokens(&mask);
                    // ORT models a bool tensor as one byte per element.
                    let mask: Vec<u8> = mask.iter().map(|&flag| u8::from(flag)).collect();
                    let mut features = vec![0.0f32; preprocess::MAX_AUDIO_TOKENS * FEATURE_DIM];
                    let mut rows: i64 = 0;
                    let mut err_buf = vec![0u8; 1024];
                    let rc = unsafe {
                        audio_session_run(
                            self.audio.0,
                            1,
                            frames as i64,
                            spectrogram.as_ptr(),
                            mask.as_ptr(),
                            features.as_mut_ptr(),
                            &raw mut rows,
                            err_buf.as_mut_ptr().cast::<std::ffi::c_char>(),
                            err_buf.len(),
                        )
                    };
                    if rc != 0 {
                        return Err(CoreError::Internal(format!(
                            "audio inference failed (code {rc}): {}",
                            ffi_error(&err_buf)
                        )));
                    }
                    let rows = usize::try_from(rows)
                        .map_err(|_| CoreError::Internal("negative audio row count".into()))?;
                    if rows == 0 {
                        return Err(CoreError::Internal(
                            "audio tower returned no feature rows".into(),
                        ));
                    }
                    if predicted != rows {
                        return Err(CoreError::Internal(format!(
                            "audio tower returned {rows} feature rows but the mask implies {predicted}"
                        )));
                    }
                    features.truncate(rows * FEATURE_DIM);
                    ids.extend(preprocess::audio_placeholders(rows));
                    audio_features.extend_from_slice(&features);
                }
            }
            sequences.push(ids);
        }

        let longest = sequences.iter().map(Vec::len).max().unwrap_or(0);
        let seq_len = (longest + 2).min(self.max_len + 2);
        if seq_len == 0 || seq_len > self.max_len + 2 {
            return Err(CoreError::Validation(format!(
                "tokenized sequence length {seq_len} outside 1..={}",
                self.max_len + 2
            )));
        }

        // Pad to a rectangular batch; the mask marks real tokens so the graph's
        // pooling ignores padding.
        let mut input_ids: Vec<i64> = Vec::with_capacity(batch * seq_len);
        let mut attention_mask: Vec<i64> = Vec::with_capacity(batch * seq_len);
        for ids in &sequences {
            let mut row: Vec<i64> = Vec::with_capacity(seq_len);
            row.push(preprocess::BOS as i64);
            row.extend(ids.iter().take(seq_len - 2).map(|&x| x as i64));
            row.push(preprocess::EOS as i64);
            let real = row.len();
            row.extend(std::iter::repeat_n(self.pad_id as i64, seq_len - real));
            input_ids.extend_from_slice(&row);
            attention_mask.extend(std::iter::repeat_n(1, real));
            attention_mask.extend(std::iter::repeat_n(0, seq_len - real));
        }

        Ok(Prepared {
            input_ids,
            attention_mask,
            batch,
            seq_len,
            image_features,
            audio_features,
        })
    }

    /// Run the backbone over prepared tensors.
    pub(crate) fn run(&self, prepared: &Prepared) -> Result<EmbedResponse, CoreError> {
        let dim = self.embedding_dim();
        let mut out = vec![0.0f32; prepared.batch * dim];
        let mut err_buf = vec![0u8; 1024];
        let image_rows = (prepared.image_features.len() / FEATURE_DIM) as i64;
        let audio_rows = (prepared.audio_features.len() / FEATURE_DIM) as i64;
        let rc = unsafe {
            embed_session_run(
                self.text.0,
                prepared.batch as i64,
                prepared.seq_len as i64,
                prepared.input_ids.as_ptr(),
                prepared.attention_mask.as_ptr(),
                prepared.image_features.as_ptr(),
                image_rows,
                std::ptr::null(),
                0,
                prepared.audio_features.as_ptr(),
                audio_rows,
                out.as_mut_ptr(),
                err_buf.as_mut_ptr().cast::<std::ffi::c_char>(),
                err_buf.len(),
            )
        };
        if rc != 0 {
            return Err(CoreError::Internal(format!(
                "embed inference failed (code {rc}): {}",
                ffi_error(&err_buf)
            )));
        }
        Ok(EmbedResponse {
            embedding_dim: dim,
            embeddings: out,
        })
    }

    /// Embed one multimodal batch. Returns one normalized vector per input.
    pub(crate) fn embed(&self, request: &EmbedRequest) -> Result<EmbedResponse, CoreError> {
        let prepared = self.prepare(request)?;
        let response = self.run(&prepared)?;
        let dim = response.embedding_dim;
        let requested = if request.dimensions == 0 || request.dimensions > dim {
            dim
        } else {
            request.dimensions
        };

        // Matryoshka truncation + renormalization: the graph normalized the
        // full vector, and a truncated prefix is no longer unit length.
        let mut embeddings = Vec::with_capacity(prepared.batch * requested);
        for row in 0..prepared.batch {
            let base = row * dim;
            let slice = &response.embeddings[base..base + requested];
            if requested < dim {
                let norm: f32 = slice.iter().map(|v| v * v).sum::<f32>().sqrt();
                if norm > 0.0 {
                    embeddings.extend(slice.iter().map(|v| v / norm));
                    continue;
                }
            }
            embeddings.extend_from_slice(slice);
        }

        Ok(EmbedResponse {
            embedding_dim: requested,
            embeddings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_prefixes_match_sentence_transformers_config() {
        assert_eq!(
            EmbedTask::Query.as_prefix(),
            "task: search result | query: "
        );
        assert_eq!(EmbedTask::Document.as_prefix(), "title: none | text: ");
    }

    #[test]
    fn split_rows_rejects_shape_mismatch() {
        let response = EmbedResponse {
            embedding_dim: 3,
            embeddings: vec![1.0, 2.0],
        };
        assert!(split_rows(&response, 1).is_err());
        let ok = EmbedResponse {
            embedding_dim: 2,
            embeddings: vec![1.0, 2.0, 3.0, 4.0],
        };
        let rows = split_rows(&ok, 2).unwrap();
        assert_eq!(rows, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
    }

    #[test]
    fn build_request_roundtrips_through_json() {
        let request = build_request(
            vec![EmbedInput::Text {
                text: "hola".into(),
            }],
            EmbedTask::Query,
            256,
        );
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"kind\":\"text\""));
        let parsed: EmbedRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.dimensions, 256);
        assert_eq!(parsed.inputs.len(), 1);
    }

    #[test]
    fn image_and_audio_inputs_roundtrip_through_json() {
        let request = build_request(
            vec![
                EmbedInput::Image {
                    width: 2,
                    height: 2,
                    rgb: vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
                },
                EmbedInput::Audio {
                    samples: vec![0.0, 0.5, -0.5],
                },
            ],
            EmbedTask::Document,
            0,
        );
        let json = serde_json::to_string(&request).unwrap();
        let parsed: EmbedRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.inputs.len(), 2);
        match &parsed.inputs[0] {
            EmbedInput::Image { width, height, rgb } => {
                assert_eq!((*width, *height), (2, 2));
                assert_eq!(rgb.len(), 12);
            }
            other => panic!("expected image, got {other:?}"),
        }
        match &parsed.inputs[1] {
            EmbedInput::Audio { samples } => assert_eq!(samples.len(), 3),
            other => panic!("expected audio, got {other:?}"),
        }
    }

    #[test]
    fn config_defaults_and_validation_bounds() {
        let config = EmbedConfig::default();
        assert_eq!(config.max_len, 2048);
        let parsed: EmbedConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.max_len, 2048);
        assert!(parsed.image.is_none());
        assert!(parsed.audio.is_none());
    }

    #[test]
    fn vision_and_audio_paths_default_to_the_cache_root() {
        assert!(resolve_vision_path(None).ends_with("vision_encoder_q4f16.onnx"));
        assert!(resolve_audio_path(None).ends_with("audio_encoder_q4f16.onnx"));
    }

    #[test]
    fn towers_resolve_beside_an_explicit_text_model_not_to_it() {
        // A caller naming the text graph must not make the towers load the same
        // file: that leaves the tower session without its modality input and
        // the first Run dies with "Invalid input name".
        let dir = std::env::temp_dir().join("openproxy_tower_resolution_test");
        std::fs::create_dir_all(&dir).unwrap();
        let text = dir.join("model_q4f16.onnx");
        std::fs::write(&text, b"stub").unwrap();
        for tower in ["vision_encoder_q4f16.onnx", "audio_encoder_q4f16.onnx"] {
            std::fs::write(dir.join(tower), b"stub").unwrap();
        }

        let explicit = text.to_str().unwrap();
        let vision = resolve_vision_path(Some(explicit));
        let audio = resolve_audio_path(Some(explicit));
        assert_eq!(
            vision,
            dir.join("vision_encoder_q4f16.onnx").to_str().unwrap()
        );
        assert_eq!(
            audio,
            dir.join("audio_encoder_q4f16.onnx").to_str().unwrap()
        );
        assert_ne!(vision, explicit);
        assert_ne!(audio, explicit);

        std::fs::remove_dir_all(&dir).ok();
    }
}
