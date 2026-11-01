//! Length-prefixed, bounded JSON IPC over private anonymous pipes.
//!
//! The worker process runs the same binary with `--embed-worker`: it receives
//! a settings frame, loads the engine inside the sandbox, and answers one
//! embedding request per frame.

use super::{CoreError, EmbedRequest, EmbedResponse, Settings, load_engine};
use std::io::{Read, Write};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout},
};

const FRAME_LIMIT: usize = 64 * 1024 * 1024;
pub const WORKER_ARGUMENT: &str = "--embed-worker";

fn internal(e: impl std::fmt::Display) -> CoreError {
    CoreError::Internal(format!("embed worker IPC: {e}"))
}

pub(super) fn validate_request(req: &EmbedRequest) -> Result<(), CoreError> {
    encode(req)?;
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

fn encode(value: &impl serde::Serialize) -> Result<Vec<u8>, CoreError> {
    let bytes = serde_json::to_vec(value).map_err(internal)?;
    validate_length(bytes.len())?;
    Ok(bytes)
}

fn validate_length(len: usize) -> Result<(), CoreError> {
    if len == 0 || len > FRAME_LIMIT {
        return Err(CoreError::Validation("Invalid embed IPC frame size".into()));
    }
    Ok(())
}

fn read_frame(reader: &mut impl Read) -> Result<Vec<u8>, CoreError> {
    let mut size = [0u8; 4];
    reader.read_exact(&mut size).map_err(internal)?;
    let len = u32::from_be_bytes(size) as usize;
    validate_length(len)?;
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes).map_err(internal)?;
    Ok(bytes)
}

fn write_frame(writer: &mut impl Write, value: &impl serde::Serialize) -> Result<(), CoreError> {
    let bytes = encode(value)?;
    writer
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .map_err(internal)?;
    writer.write_all(&bytes).map_err(internal)?;
    writer.flush().map_err(internal)
}

/// Must run before config, database, credentials, telemetry or Tokio are initialized.
pub fn run_worker() -> Result<(), CoreError> {
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let settings: Settings = serde_json::from_slice(&read_frame(&mut input)?).map_err(internal)?;
    let engine = match load_engine(
        settings.model.as_deref(),
        settings.tokenizer.as_deref(),
        settings.config.as_deref(),
        settings.threads,
    ) {
        Ok(engine) => engine,
        Err(error) => {
            write_frame(&mut output, &Result::<(), String>::Err(error.to_string()))?;
            return Err(error);
        }
    };
    write_frame(&mut output, &Result::<(), String>::Ok(()))?;
    loop {
        let Ok(frame) = read_frame(&mut input) else {
            return Ok(()); // Parent closed the pipe or sent an invalid frame.
        };
        let request: EmbedRequest = serde_json::from_slice(&frame).map_err(internal)?;
        let result = validate_request(&request)
            .and_then(|()| engine.embed(&request))
            .map_err(|e| e.to_string());
        write_frame(&mut output, &result)?;
    }
}

pub(super) struct Client {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

impl Client {
    pub async fn stop(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }

    pub async fn start(settings: &Settings) -> Result<Self, CoreError> {
        let executable = std::env::current_exe().map_err(internal)?;
        #[cfg(test)]
        let executable = executable
            .parent()
            .and_then(std::path::Path::parent)
            .ok_or_else(|| internal("missing test binary directory"))?
            .join("openproxy");
        let mut command = tokio::process::Command::new(executable);
        command
            .arg(WORKER_ARGUMENT)
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        // Never forward master keys, OAuth tokens, proxy settings or loader variables.
        for name in [
            "HOME",
            "OPENPROXY_ONNX_LIB",
            "OPENPROXY_EMBED_MODEL",
            "OPENPROXY_EMBED_TOKENIZER",
            "OPENPROXY_EMBED_CONFIG",
            "OPENPROXY_EMBED_THREADS",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = command.spawn().map_err(internal)?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| internal("missing stdin"))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| internal("missing stdout"))?;
        let mut client = Self {
            child,
            input,
            output,
        };
        // Cold artifact download (314 MB) plus session load: generous deadline.
        let ready: Result<(), String> = client.exchange(settings, Duration::from_secs(900)).await?;
        ready.map_err(internal)?;
        Ok(client)
    }

    pub async fn embed(&mut self, request: &EmbedRequest) -> Result<EmbedResponse, CoreError> {
        let response: Result<EmbedResponse, String> =
            self.exchange(request, Duration::from_secs(120)).await?;
        response.map_err(internal)
    }

    async fn exchange<T: serde::de::DeserializeOwned>(
        &mut self,
        value: &impl serde::Serialize,
        deadline: Duration,
    ) -> Result<T, CoreError> {
        let bytes = encode(value)?;
        tokio::time::timeout(deadline, async {
            self.input
                .write_u32(bytes.len() as u32)
                .await
                .map_err(internal)?;
            self.input.write_all(&bytes).await.map_err(internal)?;
            self.input.flush().await.map_err(internal)?;
            let len = self.output.read_u32().await.map_err(internal)? as usize;
            validate_length(len)?;
            let mut response = vec![0; len];
            self.output
                .read_exact(&mut response)
                .await
                .map_err(internal)?;
            serde_json::from_slice(&response).map_err(internal)
        })
        .await
        .map_err(|_| internal("deadline exceeded"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::embed_engine::EmbedTask;

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
