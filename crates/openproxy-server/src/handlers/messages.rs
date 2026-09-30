use axum::{extract::State, http::HeaderMap, response::IntoResponse};
use openproxy_pipeline::translation::{
    AnthropicRequest, OpenAIToAnthropicSseStream, anthropic_request_to_openai,
    openai_response_to_anthropic,
};
use openproxy_types::TargetFormat;
use std::sync::Arc;

use crate::{
    disconnect::CancelWatch, error::ApiError, middleware::auth::ParsedChatRequest,
    services::PipelineRunner, state::AppState,
};

pub fn router(state: &AppState) -> axum::Router<AppState> {
    axum::Router::new().route("/messages", super::chat_endpoint(state, anthropic_messages))
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

    let (pipeline, prepared) = PipelineRunner::prepare_from_handler(
        crate::services::pipeline_runner::PrepareFromHandlerParams {
            state: &state,
            headers: &headers,
            cancel_watch,
            auth_token,
            resolved_route,
            raw_request_body: parsed_req.bytes,
            endpoint_kind: openproxy_types::EndpointKind::Chat,
        },
    );

    let request_id = prepared.req.request_id;
    let is_stream = prepared.req.openai_request.stream;

    if is_stream {
        let model = prepared.req.openai_request.model.clone();
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
