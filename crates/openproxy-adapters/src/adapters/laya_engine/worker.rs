//! Length-prefixed, bounded JSON IPC over private anonymous pipes.
use super::{CoreError, SystemOneRequest, SystemOneResponse, lifecycle::Settings, load_engine};
use std::io::{Read, Write};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout},
};

const FRAME_LIMIT: usize = 4 * 1024 * 1024;
pub const WORKER_ARGUMENT: &str = "--laya-worker";

unsafe extern "C" {
    fn laya_worker_sandbox(
        model: *const std::ffi::c_char,
        tokenizer: *const std::ffi::c_char,
        config: *const std::ffi::c_char,
    ) -> std::ffi::c_int;
}

pub(super) fn validate_request(req: &SystemOneRequest) -> Result<(), CoreError> {
    encode(req)?;
    if req.questions.len() > 64 {
        return Err(CoreError::Validation(
            "Laya supports at most 64 questions per batch".into(),
        ));
    }
    for q in req.questions.values() {
        let count = super::render_options(q).len();
        if count == 0 || count > 128 {
            return Err(CoreError::Validation(
                "Laya questions require 1..=128 options".into(),
            ));
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
        return Err(CoreError::Validation("Invalid Laya IPC frame size".into()));
    }
    Ok(())
}

fn internal(e: impl std::fmt::Display) -> CoreError {
    CoreError::Internal(format!("Laya worker IPC: {e}"))
}

fn read_frame(reader: &mut impl Read) -> Result<Vec<u8>, CoreError> {
    let mut size = [0; 4];
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
    let engine = match initialize(&settings) {
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
        let request: SystemOneRequest = serde_json::from_slice(&frame).map_err(internal)?;
        let result = validate_request(&request)
            .and_then(|()| engine.classify(&request))
            .map_err(|e| e.to_string());
        write_frame(&mut output, &result)?;
    }
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
        unsafe { laya_worker_sandbox(model_c.as_ptr(), tokenizer_c.as_ptr(), config_c.as_ptr()) };
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
        // Unit-test harnesses cannot handle --laya-worker. Use the workspace's
        // server binary built by cargo test --workspace instead.
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
            "OPENPROXY_LAYA_MODEL",
            "OPENPROXY_LAYA_TOKENIZER",
            "OPENPROXY_LAYA_CONFIG",
            "OPENPROXY_LAYA_THREADS",
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
        let ready: Result<(), String> = client.exchange(settings, Duration::from_secs(120)).await?;
        ready.map_err(internal)?;
        Ok(client)
    }

    pub async fn classify(
        &mut self,
        request: &SystemOneRequest,
    ) -> Result<SystemOneResponse, CoreError> {
        let response: Result<SystemOneResponse, String> =
            self.exchange(request, Duration::from_secs(30)).await?;
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
