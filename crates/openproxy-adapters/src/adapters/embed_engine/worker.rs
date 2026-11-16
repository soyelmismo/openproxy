//! Length-prefixed, bounded JSON IPC over private anonymous pipes.
//!
//! The worker process runs the same binary with `--embed-worker`: it receives
//! a settings frame, loads the engine inside the sandbox, and answers one
//! embedding request per frame.

use super::{CoreError, EmbedRequest, Settings, load_engine};
use crate::adapters::native_engine_common::{
    IpcWorkerClient, encode_ipc_frame, run_ipc_worker_loop,
};
#[cfg(test)]
use crate::adapters::native_engine_common::{read_ipc_frame, write_ipc_frame};
#[cfg(test)]
use std::io::{Read, Write};
use std::time::Duration;

const FRAME_LIMIT: usize = 64 * 1024 * 1024;
const IPC_LABEL: &str = "embed";
const INVALID_FRAME_MSG: &str = "Invalid embed IPC frame size";
pub const WORKER_ARGUMENT: &str = "--embed-worker";

pub(super) fn validate_request(req: &EmbedRequest) -> Result<(), CoreError> {
    encode_ipc_frame(req, FRAME_LIMIT, IPC_LABEL, INVALID_FRAME_MSG)?;
    if req.inputs.is_empty() || req.inputs.len() > 64 {
        return Err(CoreError::Validation(
            "embed accepts 1..=64 inputs per request".into(),
        ));
    }
    for input in &req.inputs {
        match input {
            super::EmbedInput::Text { text } => {
                if text.len() > 256 * 1024 {
                    return Err(CoreError::Validation("embed text exceeds 256 KiB".into()));
                }
            }
            super::EmbedInput::Image { width, height, rgb } => {
                let expected = *width as usize * *height as usize * 3;
                if rgb.len() != expected {
                    return Err(CoreError::Validation(format!(
                        "image buffer holds {} bytes, expected {expected}",
                        rgb.len()
                    )));
                }
                if width * height > 64 * 1024 * 1024 {
                    return Err(CoreError::Validation(
                        "image exceeds the 64 megapixel budget".into(),
                    ));
                }
            }
            super::EmbedInput::Audio { samples } => {
                if samples.len() > super::preprocess::AUDIO_MAX_SAMPLES {
                    return Err(CoreError::Validation(format!(
                        "audio exceeds the {} sample limit",
                        super::preprocess::AUDIO_MAX_SAMPLES
                    )));
                }
            }
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
        |settings: &Settings| {
            load_engine(
                settings.model.as_deref(),
                settings.tokenizer.as_deref(),
                settings.config.as_deref(),
                settings.threads,
            )
        },
        |engine, request: &EmbedRequest| {
            validate_request(request).and_then(|()| engine.embed(request))
        },
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
                "OPENPROXY_EMBED_MODEL",
                "OPENPROXY_EMBED_TOKENIZER",
                "OPENPROXY_EMBED_CONFIG",
                "OPENPROXY_EMBED_THREADS",
            ],
            Duration::from_secs(900),
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
    use crate::adapters::embed_engine::EmbedTask;

    crate::adapters::native_engine_common::define_ipc_frame_tests!();

    #[test]
    fn request_rejects_empty_and_oversized_batches() {
        assert!(
            validate_request(&EmbedRequest {
                inputs: vec![],
                task: EmbedTask::Query,
                dimensions: 0,
            })
            .is_err()
        );
        let many: Vec<super::super::EmbedInput> = (0..65)
            .map(|n| super::super::EmbedInput::Text {
                text: format!("t{n}"),
            })
            .collect();
        assert!(
            validate_request(&EmbedRequest {
                inputs: many,
                task: EmbedTask::Query,
                dimensions: 0,
            })
            .is_err()
        );
    }

    #[test]
    fn request_rejects_oversized_text() {
        assert!(
            validate_request(&EmbedRequest {
                inputs: vec![super::super::EmbedInput::Text {
                    text: "a".repeat(300 * 1024),
                }],
                task: EmbedTask::Document,
                dimensions: 0,
            })
            .is_err()
        );
    }

    #[test]
    fn request_rejects_image_buffer_size_mismatch() {
        assert!(
            validate_request(&EmbedRequest {
                inputs: vec![super::super::EmbedInput::Image {
                    width: 10,
                    height: 10,
                    rgb: vec![0u8; 5],
                }],
                task: EmbedTask::Document,
                dimensions: 0,
            })
            .is_err()
        );
    }

    #[test]
    fn request_rejects_oversize_audio() {
        assert!(
            validate_request(&EmbedRequest {
                inputs: vec![super::super::EmbedInput::Audio {
                    samples: vec![0.0; super::super::preprocess::AUDIO_MAX_SAMPLES + 1],
                }],
                task: EmbedTask::Document,
                dimensions: 0,
            })
            .is_err()
        );
    }
}
