use axum::{extract::State, http::HeaderMap, response::IntoResponse};
use openproxy_pipeline::translation::{
    AnthropicRequest, OpenAIToAnthropicSseStream, anthropic_request_to_openai,
    openai_response_to_anthropic,
};
use openproxy_types::TargetFormat;
use openproxy_types::ids::ApiKeyId;
use std::sync::Arc;

use crate::{
    disconnect::CancelWatch, error::ApiError, middleware::auth::ParsedChatRequest,
    services::PipelineRunner, state::AppState,
};

pub fn router(state: &AppState) -> axum::Router<AppState> {
    axum::Router::new().route(
        "/",
        super::chat_endpoint(state, anthropic_messages),
    )
}

pub async fn anthropic_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    cancel_watch: Option<axum::Extension<CancelWatch>>,
    axum::Extension(parsed_req): axum::Extension<ParsedChatRequest>,
    crate::extractors::ValidatedToken(auth_token): crate::extractors::ValidatedToken,
    axum::Extension(mut resolved_route): axum::Extension<crate::middleware::routing::ResolvedRoute>,
) -> Result<axum::response::Response, ApiError> {
    let anthropic_req: AnthropicRequest =
        serde_json::from_slice(&parsed_req.bytes).map_err(|e| {
            ApiError(openproxy_types::error::CoreError::Validation(format!(
                "Invalid Anthropic Request: {e}"
            )))
        })?;

    let openai_req = Arc::new(anthropic_request_to_openai(anthropic_req));
    resolved_route.openai_req = Arc::clone(&openai_req);

    let cancel = cancel_watch
        .map(|axum::Extension(cw)| cw)
        .unwrap_or_default();
    let api_key_id: Option<ApiKeyId> = auth_token.as_ref().map(|r| r.key_id);

    let pipeline = PipelineRunner::build_pipeline(&state);
    let is_stream = openai_req.stream;
    let model = openai_req.model.clone();

    let prepared =
        PipelineRunner::prepare_request(crate::services::pipeline_runner::PrepareRequestParams {
            state: &state,
            headers: &headers,
            cancel,
            openai_req,
            raw_request_body: parsed_req.bytes,
            api_key_id,
            combo_id: resolved_route.combo_id,
            combo_override: resolved_route.combo_override,
            targets_override: resolved_route.targets_override,
            endpoint_kind: openproxy_types::EndpointKind::Chat,
        });

    let request_id = prepared.req.request_id;

    if is_stream {
        let merged = PipelineRunner::spawn_streaming_bridge(
            pipeline,
            prepared.req,
            prepared.done_tx,
            prepared.stream_rx,
            TargetFormat::Anthropic,
        );

        let sse_stream =
            OpenAIToAnthropicSseStream::new(merged, format!("msg_{request_id}"), model);

        let body = axum::body::Body::from_stream(sse_stream);
        Ok((
            [(
                axum::http::header::CONTENT_TYPE,
                "text/event-stream; charset=utf-8",
            )],
            body,
        )
            .into_response())
    } else {
        let result = pipeline.run(prepared.req).await;
        let _ = prepared.done_tx.send(());
        if let Some(err) = result.error {
            return Err(ApiError(err));
        }
        let body_value = match result.final_response {
            Some(resp) => {
                let anthropic_resp = openai_response_to_anthropic(resp);
                serde_json::to_value(&anthropic_resp).unwrap_or_else(|e| {
                    let err = ApiError(openproxy_types::CoreError::Internal(e.to_string()));
                    serde_json::json!({"error": {"message": err.sanitized_message()}})
                })
            }
            None => serde_json::json!({"error": {"message": "no response"}}),
        };
        Ok(axum::Json(body_value).into_response())
    }
}
