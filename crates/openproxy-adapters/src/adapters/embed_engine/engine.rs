//! Engine construction and the multimodal prepare/run/embed pipeline.

use openproxy_types::error::CoreError;
use std::ffi::CString;
use tokenizers::Tokenizer;

use super::paths::{
    resolve_audio_path, resolve_config_path, resolve_model_path, resolve_tokenizer_path,
    resolve_vision_path,
};
use super::preprocess::{self, AudioSettings, ImageSettings};
use super::types::{EmbedInput, EmbedRequest, EmbedResponse};
use super::{
    EmbedConfig, FEATURE_DIM, SafeAudio, SafeEmbed, SafeVision, audio_session_create,
    audio_session_run, embed_session_create, embed_session_destroy, embed_session_embedding_dim,
    embed_session_run, ffi_error, vision_session_create, vision_session_destroy,
    vision_session_run, worker_sandbox,
};

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
