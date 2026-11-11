//! Resolution of model, tokenizer, config and modality tower artifact paths.

use std::sync::OnceLock;

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
