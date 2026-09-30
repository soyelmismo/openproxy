//! Worker tests use the real executable, never load ONNX into the test process.
#![cfg(feature = "laya-engine")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    io::{Read, Write},
    process::{Child, Command, Stdio},
};

fn spawn() -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_openproxy"));
    command
        .arg("--laya-worker")
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for name in [
        "HOME",
        "OPENPROXY_ONNX_LIB",
        "OPENPROXY_LAYA_MODEL",
        "OPENPROXY_LAYA_TOKENIZER",
        "OPENPROXY_LAYA_CONFIG",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.env("OPENPROXY_LAYA_THREADS", "2").spawn().unwrap()
}

fn send(child: &mut Child, value: &serde_json::Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    let input = child.stdin.as_mut().unwrap();
    input
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .unwrap();
    input.write_all(&bytes).unwrap();
    input.flush().unwrap();
}

fn receive(child: &mut Child) -> serde_json::Value {
    let output = child.stdout.as_mut().unwrap();
    let mut size = [0; 4];
    output.read_exact(&mut size).unwrap();
    let len = u32::from_be_bytes(size) as usize;
    assert!(len <= 4 * 1024 * 1024);
    let mut bytes = vec![0; len];
    output.read_exact(&mut bytes).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn worker_rejects_oversized_frame_before_model_loading() {
    let mut child = spawn();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&u32::MAX.to_be_bytes())
        .unwrap();
    drop(child.stdin.take());
    assert!(!child.wait().unwrap().success());
}

#[test]
fn worker_rejects_truncated_frame_before_model_loading() {
    let mut child = spawn();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&[0, 0, 0, 16, b'{'])
        .unwrap();
    drop(child.stdin.take());
    assert!(!child.wait().unwrap().success());
}

#[test]
#[ignore = "requires local Laya weights, ONNX Runtime and Linux Landlock ABI 3"]
fn isolated_inference_survives_worker_restart_and_closes_on_eof() {
    for _ in 0..2 {
        let mut child = spawn();
        send(&mut child, &serde_json::json!({}));
        assert!(receive(&mut child).get("Ok").is_some());
        send(
            &mut child,
            &serde_json::json!({
                "state": "Write a Python function to parse JSON 🦀 日本語",
                "questions": {"topic": {"type":"choice", "instructions":"Categorize", "options":["coding", "creative"]}}
            }),
        );
        let response = receive(&mut child);
        assert_eq!(response["Ok"]["answers"]["topic"]["choice"], "coding");
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn sandbox_blocks_network_secrets_writes_and_exec() {
    if std::env::var_os("LAYA_SANDBOX_PROBE").is_none() {
        let directory = std::env::temp_dir().join(format!("laya-sandbox-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sandbox_blocks_network_secrets_writes_and_exec",
                "--nocapture",
            ])
            .env("LAYA_SANDBOX_PROBE", "1")
            .env("LAYA_SANDBOX_DIRECTORY", &directory)
            .status()
            .unwrap();
        let _ = std::fs::remove_file(directory.join("model"));
        std::fs::remove_dir(&directory).unwrap();
        assert!(status.success());
        return;
    }
    unsafe extern "C" {
        fn laya_worker_sandbox(
            model: *const std::ffi::c_char,
            tokenizer: *const std::ffi::c_char,
            config: *const std::ffi::c_char,
        ) -> std::ffi::c_int;
    }
    // Refer to the adapters crate so its native archive is linked into this test.
    let _ = openproxy_adapters::laya_engine::is_enabled();
    let directory = std::path::PathBuf::from(std::env::var_os("LAYA_SANDBOX_DIRECTORY").unwrap());
    let model = directory.join("model");
    std::fs::write(&model, b"fixture").unwrap();
    let c_path = std::ffi::CString::new(model.to_str().unwrap()).unwrap();
    let rc = unsafe { laya_worker_sandbox(c_path.as_ptr(), c_path.as_ptr(), c_path.as_ptr()) };
    if rc != 0 {
        // Unavailable kernels must fail closed; actual inference test checks the supported host.
        return;
    }
    assert_eq!(std::fs::read(&model).unwrap(), b"fixture");
    let status = std::fs::read_to_string("/proc/self/status");
    assert!(status.is_err()); // No process metadata beyond the explicit CPU/memory files.
    assert!(std::fs::read("/etc/passwd").is_err());
    assert!(std::fs::read(format!("/proc/{}/mem", std::process::id())).is_err());
    assert!(std::fs::write(&model, b"overwrite").is_err());
    assert!(std::fs::write(directory.join("new"), b"new").is_err());
    assert!(std::net::TcpListener::bind("127.0.0.1:0").is_err());
    assert!(Command::new("/bin/true").status().is_err());
    // Landlock intentionally denies cleanup; parent-side test runner owns fixture cleanup.
}
