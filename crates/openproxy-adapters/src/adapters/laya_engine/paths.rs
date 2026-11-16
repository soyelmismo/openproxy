//! Resolution of model, tokenizer and config artifact paths.

use crate::adapters::native_engine_common::resolve_engine_candidate_path;

fn resolve_candidate_path(
    explicit: Option<&str>,
    env_var: &str,
    filename: &str,
    extra_subpaths: &[&str],
) -> String {
    resolve_engine_candidate_path("laya", explicit, env_var, filename, extra_subpaths, true)
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
    )
}

pub fn resolve_tokenizer_path(opt: Option<&str>) -> String {
    resolve_candidate_path(
        opt,
        "OPENPROXY_LAYA_TOKENIZER",
        "tokenizer.json",
        &["tokenizer/tokenizer.json"],
    )
}

pub fn resolve_config_path(opt: Option<&str>) -> String {
    resolve_candidate_path(
        opt,
        "OPENPROXY_LAYA_CONFIG",
        "rl_agent_config.json",
        &["onnx_config.json"],
    )
}

pub fn is_model_installed() -> bool {
    let p = resolve_model_path(None);
    std::path::Path::new(&p).exists()
}
