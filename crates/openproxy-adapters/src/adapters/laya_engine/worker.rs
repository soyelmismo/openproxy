//! Length-prefixed, bounded JSON IPC over private anonymous pipes.
use super::{CoreError, SystemOneRequest, lifecycle::Settings, load_engine};
use crate::adapters::native_engine_common::{
    IpcWorkerClient, encode_ipc_frame, ipc_internal, run_ipc_worker_loop,
};
#[cfg(test)]
use crate::adapters::native_engine_common::{read_ipc_frame, write_ipc_frame};
#[cfg(test)]
use std::io::{Read, Write};
use std::time::Duration;

const FRAME_LIMIT: usize = 4 * 1024 * 1024;
const IPC_LABEL: &str = "Laya";
const INVALID_FRAME_MSG: &str = "Invalid Laya IPC frame size";
pub const WORKER_ARGUMENT: &str = "--laya-worker";

unsafe extern "C" {
    fn worker_sandbox(
        model: *const std::ffi::c_char,
        tokenizer: *const std::ffi::c_char,
        config: *const std::ffi::c_char,
        allow_model_dir: std::ffi::c_int,
    ) -> std::ffi::c_int;
}

fn internal(e: impl std::fmt::Display) -> CoreError {
    ipc_internal(IPC_LABEL, e)
}

pub(super) fn validate_request(req: &SystemOneRequest) -> Result<(), CoreError> {
    encode_ipc_frame(req, FRAME_LIMIT, IPC_LABEL, INVALID_FRAME_MSG)?;
    if req.questions.len() > 64 {
        return Err(CoreError::Validation(
            "Laya supports at most 64 questions per batch".into(),
        ));
    }
    for (name, q) in &req.questions {
        let count = super::render_options(q).len();
        if count == 0 || count > 128 {
            return Err(CoreError::Validation(format!(
                "Laya question '{name}' ({}) resolved {count} options; each question requires 1..=128",
                q.question_type.as_str(),
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
fn read_frame(reader: &mut impl Read) -> Result<Vec<u8>, CoreError> {
    read_ipc_frame(reader, FRAME_LIMIT, IPC_LABEL, INVALID_FRAME_MSG)
}

#[cfg(test)]
fn write_frame(writer: &mut impl Write, value: &impl serde::Serialize) -> Result<(), CoreError> {
    write_ipc_frame(writer, value, FRAME_LIMIT, IPC_LABEL, INVALID_FRAME_MSG)
}

/// Must run before config, database, credentials, telemetry or Tokio are initialized.
pub fn run_worker() -> Result<(), CoreError> {
    run_ipc_worker_loop(
        FRAME_LIMIT,
        IPC_LABEL,
        INVALID_FRAME_MSG,
        initialize,
        |engine, request: &SystemOneRequest| {
            validate_request(request).and_then(|()| engine.classify(request))
        },
    )
}

fn initialize(settings: &Settings) -> Result<super::LayaEngine, CoreError> {
    let model = super::resolve_model_path(settings.model.as_deref());
    let tokenizer = super::resolve_tokenizer_path(settings.tokenizer.as_deref());
    let config = super::resolve_config_path(settings.config.as_deref());
    let paths = [&model, &tokenizer, &config].map(|p| {
        std::path::absolute(p)
            .map_err(internal)
            .and_then(|p| std::ffi::CString::new(p.to_string_lossy().as_bytes()).map_err(internal))
    });
    let [model_c, tokenizer_c, config_c] = paths;
    let (model_c, tokenizer_c, config_c) = (model_c?, tokenizer_c?, config_c?);
    let rc =
        unsafe { worker_sandbox(model_c.as_ptr(), tokenizer_c.as_ptr(), config_c.as_ptr(), 0) };
    if rc != 0 {
        return Err(internal(format!(
            "sandbox unavailable (code {rc}); refusing native inference"
        )));
    }
    load_engine(
        Some(&model),
        Some(&tokenizer),
        Some(&config),
        settings.threads,
    )
}

pub(super) struct Client(pub(super) IpcWorkerClient);

impl Client {
    pub async fn start(settings: &Settings) -> Result<Self, CoreError> {
        let inner = IpcWorkerClient::start(
            settings,
            WORKER_ARGUMENT,
            &[
                "HOME",
                "OPENPROXY_ONNX_LIB",
                "OPENPROXY_LAYA_MODEL",
                "OPENPROXY_LAYA_TOKENIZER",
                "OPENPROXY_LAYA_CONFIG",
                "OPENPROXY_LAYA_THREADS",
            ],
            Duration::from_secs(120),
            FRAME_LIMIT,
            IPC_LABEL,
            INVALID_FRAME_MSG,
        )
        .await?;
        Ok(Self(inner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::adapters::native_engine_common::define_ipc_frame_tests!();

    #[test]
    fn request_rejects_empty_and_excessive_options() {
        let mut request: SystemOneRequest = serde_json::from_value(serde_json::json!({
            "state": "x", "questions": {"q": {"type":"choice", "instructions":"Choose", "options":[]}}
        })).unwrap();
        assert!(validate_request(&request).is_err());
        request.questions.get_mut("q").unwrap().options = Some(vec!["x".into(); 129]);
        assert!(validate_request(&request).is_err());
        request.questions.get_mut("q").unwrap().options = Some(vec!["x".into()]);
        assert!(validate_request(&request).is_ok());
        let question = request.questions["q"].clone();
        request.questions = (0..65).map(|n| (n.to_string(), question.clone())).collect();
        assert!(validate_request(&request).is_err());
    }
}
