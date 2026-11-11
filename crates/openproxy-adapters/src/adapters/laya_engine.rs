use openproxy_types::{
    error::CoreError,
    systemone::{SystemOneRequest, SystemOneResponse},
};
mod assets;
mod lifecycle;
mod worker;
pub use lifecycle::{
    execute_decision, init, is_available, is_enabled, shutdown, spawn_init_background,
};
pub use worker::run_worker;
mod config;
mod engine;
mod paths;
pub use config::LayaConfig;
pub(crate) use config::validate_config;
pub use engine::LayaEngine;
#[cfg(test)]
pub(crate) use engine::confidence_from_probs;
pub(crate) use engine::{load_engine, render_options};
pub use paths::{
    is_model_installed, resolve_config_path, resolve_model_path, resolve_tokenizer_path,
};

#[repr(C)]
struct LayaSessionOpaque {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn laya_session_create(
        onnx_lib_path: *const std::ffi::c_char,
        model_path: *const std::ffi::c_char,
        num_threads: std::ffi::c_int,
        err_buf: *mut std::ffi::c_char,
        err_buf_len: usize,
    ) -> *mut LayaSessionOpaque;

    fn laya_session_destroy(session: *mut LayaSessionOpaque);

    fn laya_session_run(
        session: *mut LayaSessionOpaque,
        batch_size: i64,
        seq_len: i64,
        max_markers: i64,
        input_ids: *const i64,
        attention_mask: *const i64,
        marker_pos: *const i64,
        marker_mask: *const u8,
        qtype: *const i64,
        out_logits: *mut f32,
        err_buf: *mut std::ffi::c_char,
        err_buf_len: usize,
    ) -> std::ffi::c_int;
}

struct SafeSession(*mut LayaSessionOpaque);

impl Drop for SafeSession {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { laya_session_destroy(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}

fn ffi_error(buffer: &[u8]) -> String {
    let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}

#[cfg(test)]
mod tests;
