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
mod engine;
mod lifecycle;
mod paths;
pub mod preprocess;
mod types;
mod worker;
pub use lifecycle::{
    execute_embedding, execute_text_embedding, init, is_available, is_enabled, shutdown,
    spawn_init_background,
};
pub use worker::run_worker;

pub use engine::EmbedEngine;
pub(crate) use engine::load_engine;
pub use paths::{
    is_model_installed, resolve_audio_path, resolve_config_path, resolve_model_path,
    resolve_tokenizer_path, resolve_vision_path,
};
pub use preprocess::{AudioSettings, ImageSettings};
pub(crate) use types::{EmbedConfig, Settings, build_request, split_rows};
pub use types::{EmbedInput, EmbedRequest, EmbedResponse, EmbedTask};

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

fn ffi_error(buffer: &[u8]) -> String {
    let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}

#[cfg(test)]
mod tests;
