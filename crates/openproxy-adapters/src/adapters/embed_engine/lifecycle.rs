//! Gateway-side supervisor for the embed worker.
//!
//! Uses `EngineSupervisor` from `native_engine_common`: a bounded job queue, a
//! resident worker spawned on demand, and an idle timeout that kills the child
//! and releases native memory. Nothing is loaded at startup.

use super::worker::{self, Client};
use super::{CoreError, EmbedRequest, EmbedResponse, EmbedTask, Settings, split_rows};

crate::adapters::native_engine_common::define_engine_lifecycle!(
    label: "embed",
    thread_name: "embed-supervisor",
    idle_env: "OPENPROXY_EMBED_IDLE_TIMEOUT_MINUTES",
    recv_timeout_secs: 1800,
    call_timeout_secs: 120,
    unavail_msg: "embed worker unavailable",
    req_ty: EmbedRequest,
    resp_ty: EmbedResponse,
    settings_ty: Settings,
    ensure_fn: super::assets::ensure,
    client_ty: Client
);

/// Embed one multimodal batch. `dimensions` 0 means "model dimension".
///
/// Returns one normalized vector per input, in input order.
pub async fn execute_embedding(
    inputs: Vec<super::EmbedInput>,
    task: EmbedTask,
    dimensions: usize,
) -> Result<Vec<Vec<f32>>, CoreError> {
    let expected = inputs.len();
    let request = super::build_request(inputs, task, dimensions);
    worker::validate_request(&request)?;
    let response = tokio::task::spawn_blocking(move || submit(Some(request), Settings::default()))
        .await
        .map_err(|e| CoreError::Internal(format!("embed_engine join error: {e}")))?
        .map_err(|e| CoreError::Internal(format!("embed_engine execution error: {e}")))?
        .ok_or_else(|| CoreError::Internal("embed worker returned no response".into()))?;
    split_rows(&response, expected)
}

/// Convenience wrapper for the common text-only case.
pub async fn execute_text_embedding(
    texts: Vec<String>,
    task: EmbedTask,
    dimensions: usize,
) -> Result<Vec<Vec<f32>>, CoreError> {
    let inputs = texts
        .into_iter()
        .map(|text| super::EmbedInput::Text { text })
        .collect();
    execute_embedding(inputs, task, dimensions).await
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::adapters::native_engine_common::define_supervisor_tests!(
        EmbedRequest,
        EmbedResponse,
        Settings,
        "requires local embeddinggemma weights, server binary and Linux Landlock ABI 3"
    );

    #[test]
    fn task_prefixes_match_sentence_transformers_config() {
        assert_eq!(
            EmbedTask::Query.as_prefix(),
            "task: search result | query: "
        );
        assert_eq!(EmbedTask::Document.as_prefix(), "title: none | text: ");
    }
}
