//! Resolution of model, tokenizer and config artifact paths.

fn resolve_candidate_path(
    explicit: Option<&str>,
    env_var: &str,
    filename: &str,
    extra_subpaths: &[&str],
    dev_fallbacks: &[&str],
) -> String {
    if let Some(p) = explicit.filter(|s| !s.is_empty()) {
        return p.to_string();
    }
    if let Some(p) = std::env::var(env_var).ok().filter(|s| !s.is_empty()) {
        return p;
    }

    let mut candidates = Vec::new();

    // 1. ~/.openproxy/models/laya/<subpath>
    if let Ok(home) = std::env::var("HOME") {
        let base = format!("{home}/.openproxy/models/laya");
        candidates.push(format!("{base}/{filename}"));
        for sub in extra_subpaths {
            candidates.push(format!("{base}/{sub}"));
        }
    }

    // 2. ./models/laya/<subpath>
    candidates.push(format!("./models/laya/{filename}"));
    for sub in extra_subpaths {
        candidates.push(format!("./models/laya/{sub}"));
    }

    // 3. /var/lib/openproxy/models/laya/<subpath> (Docker container volume mount)
    let container_base = "/var/lib/openproxy/models/laya";
    candidates.push(format!("{container_base}/{filename}"));
    for sub in extra_subpaths {
        candidates.push(format!("{container_base}/{sub}"));
    }

    // 4. Dev fallbacks
    for cand in dev_fallbacks {
        candidates.push((*cand).to_string());
    }

    for cand in &candidates {
        if std::path::Path::new(cand).exists() {
            return cand.clone();
        }
    }

    // Default canonical path if none exist yet
    if let Ok(home) = std::env::var("HOME") {
        format!("{home}/.openproxy/models/laya/{filename}")
    } else {
        format!("./models/laya/{filename}")
    }
}

pub fn resolve_model_path(opt: Option<&str>) -> String {
    resolve_candidate_path(
        opt,
        "OPENPROXY_LAYA_MODEL",
        "model.onnx",
        &[
            "model-fp32/model.onnx",
            "model-int8/model.onnx",
            "model-fp32.onnx",
            "model-int8.onnx",
        ],
        &[],
    )
}

pub fn resolve_tokenizer_path(opt: Option<&str>) -> String {
    resolve_candidate_path(
        opt,
        "OPENPROXY_LAYA_TOKENIZER",
        "tokenizer.json",
        &["tokenizer/tokenizer.json"],
        &[],
    )
}

pub fn resolve_config_path(opt: Option<&str>) -> String {
    resolve_candidate_path(
        opt,
        "OPENPROXY_LAYA_CONFIG",
        "rl_agent_config.json",
        &["onnx_config.json"],
        &[],
    )
}

pub fn is_model_installed() -> bool {
    let p = resolve_model_path(None);
    std::path::Path::new(&p).exists()
}
