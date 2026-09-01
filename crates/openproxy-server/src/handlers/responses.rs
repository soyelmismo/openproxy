//! `POST /v1/responses` — the Responses-protocol public entry point.
//!
//! Spec §3.3 describes the contract:
//! 1. Parse the incoming JSON as an [`ResponsesRequest`].
//! 2. Translate to internal [`OpenAIRequest`](openproxy_types::OpenAIRequest)
//!    (the pipeline's lingua franca).
//! 3. Resolve the routing plan from the `model` field via the same
//!    middlewares as `/v1/chat/completions`.
//! 4. Drive the standard [`Pipeline`] path.
//! 5. Return a Responses-shaped response (not a chat-completion shape).
//!
//! See `docs/specs/antigravity-gaps-p2.md` §3 (GAP-2) for full spec.

use axum::{Router, extract::State, http::HeaderMap, routing::post};

use crate::{
    disconnect::CancelWatch, error::ApiError, extractors::ValidatedToken,
    middleware::auth::ParsedChatRequest, middleware::routing::ResolvedRoute,
    services::PipelineRunner, state::AppState,
};

/// Same middlewares as `chat_endpoint`: client_disconnect + rate_limit
/// + auth + routing. The auth_middleware detects `/v1/responses` and
///   translates the Responses body into a `ParsedChatRequest`, so the
///   routing middleware resolves the route correctly (P2-1 / P2-2
///   patches from the spec).
pub fn router(state: &AppState) -> Router<AppState> {
    use axum::middleware;
    Router::new().route(
        "/responses",
        post(responses_completions)
            .route_layer(middleware::from_fn(
                crate::disconnect::client_disconnect_middleware,
            ))
            .route_layer(middleware::from_fn_with_state(
                state.clone(),
                crate::middleware::rate_limit::rate_limit_middleware,
            ))
            .route_layer(middleware::from_fn_with_state(
                state.clone(),
                crate::middleware::routing::routing_middleware,
            ))
            .route_layer(middleware::from_fn_with_state(
                state.clone(),
                crate::middleware::auth::auth_middleware,
            )),
    )
}

pub async fn responses_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    cancel_watch: Option<axum::Extension<CancelWatch>>,
    ValidatedToken(auth_token): ValidatedToken,
    axum::Extension(parsed_req): axum::Extension<ParsedChatRequest>,
    axum::Extension(resolved_route): axum::Extension<ResolvedRoute>,
) -> Result<axum::response::Response, ApiError> {
    let cancel = cancel_watch
        .map(|axum::Extension(cw)| cw)
        .unwrap_or_default();

    let token_inner = auth_token;
    let api_key_id: Option<openproxy_types::ids::ApiKeyId> =
        token_inner.as_ref().map(|r| r.key_id);

    let pipeline = PipelineRunner::build_pipeline(&state);
    let is_stream = resolved_route.openai_req.stream;

    let prepared =
        PipelineRunner::prepare_request(crate::services::pipeline_runner::PrepareRequestParams {
            state: &state,
            headers: &headers,
            cancel,
            openai_req: resolved_route.openai_req,
            raw_request_body: parsed_req.bytes,
            api_key_id,
            combo_id: resolved_route.combo_id,
            combo_override: resolved_route.combo_override,
            targets_override: resolved_route.targets_override,
            endpoint_kind: openproxy_types::EndpointKind::Chat,
        });

    // CRITICAL (N1): non-streaming Responses path MUST use
    // `handle_sync_response_responses` (not `handle_sync_response`)
    // to emit the Responses-shaped envelope.
    if is_stream {
        return Ok(crate::handlers::chat::handle_streaming_response(
            pipeline,
            prepared.req,
            prepared.done_tx,
            prepared.stream_rx,
        ));
    }
    crate::handlers::chat::handle_sync_response_responses(
        pipeline,
        prepared.req,
        prepared.done_tx,
    )
    .await
}

#[cfg(test)]
mod tests {
    use openproxy_types::{ResponsesContent, ResponsesInputItem, ResponsesRequest};
    use serde_json::json;

    #[test]
    fn translate_responses_to_openai_with_one_message() {
        let req = ResponsesRequest {
            model: "gpt-x".to_string(),
            instructions: None,
            input: vec![ResponsesInputItem::Message {
                role: "user".to_string(),
                content: ResponsesContent::Plain("hi".to_string()),
            }],
            tools: None,
            tool_choice: None,
            stream: false,
            previous_response_id: None,
            extra: serde_json::Map::new(),
        };

        let openai = crate::middleware::auth::translate_responses_to_openai(&req);
        assert_eq!(openai.model, "gpt-x");
        assert_eq!(openai.messages.len(), 1);
        assert_eq!(openai.messages[0].role, "user");
        assert_eq!(
            openai.messages[0].content.as_ref().and_then(|v| v.as_str()),
            Some("hi")
        );
    }

    #[test]
    fn translate_responses_to_openai_with_function_call_output() {
        let req = ResponsesRequest {
            model: "gpt-x".to_string(),
            instructions: None,
            input: vec![ResponsesInputItem::FunctionCallOutput {
                call_id: "c1".to_string(),
                output: "pong".to_string(),
            }],
            tools: None,
            tool_choice: None,
            stream: false,
            previous_response_id: None,
            extra: serde_json::Map::new(),
        };

        let openai = crate::middleware::auth::translate_responses_to_openai(&req);
        assert_eq!(openai.messages.len(), 1);
        assert_eq!(openai.messages[0].role, "tool");
        assert_eq!(
            openai.messages[0].content.as_ref().and_then(|v| v.as_str()),
            Some("pong")
        );
        assert_eq!(
            openai.messages[0].tool_call_id.as_deref(),
            Some("c1")
        );
    }

    #[test]
    fn translate_responses_to_openai_prepends_instructions() {
        let req = ResponsesRequest {
            model: "gpt-x".to_string(),
            instructions: Some("be brief".to_string()),
            input: vec![ResponsesInputItem::Message {
                role: "user".to_string(),
                content: ResponsesContent::Plain("hi".to_string()),
            }],
            tools: None,
            tool_choice: None,
            stream: false,
            previous_response_id: None,
            extra: serde_json::Map::new(),
        };

        let openai = crate::middleware::auth::translate_responses_to_openai(&req);
        assert_eq!(openai.messages.len(), 2);
        assert_eq!(openai.messages[0].role, "system");
        assert_eq!(
            openai.messages[0].content.as_ref().and_then(|v| v.as_str()),
            Some("be brief")
        );
        assert_eq!(openai.messages[1].role, "user");
    }

    #[test]
    fn translate_responses_to_openai_drops_unknown_items() {
        // Unknown type → messages should be empty
        let payload = json!({
            "model": "gpt-x",
            "input": [{"type": "reasoning"}]
        });
        let responses_req: ResponsesRequest =
            serde_json::from_value(payload).expect("deserialization should succeed");
        let openai = crate::middleware::auth::translate_responses_to_openai(&responses_req);
        assert!(openai.messages.is_empty());
    }

    #[test]
    fn responses_request_parses_minimal_payload() {
        let payload = json!({
            "model": "gpt-x",
            "input": []
        });
        let req: ResponsesRequest =
            serde_json::from_value(payload).expect("deserialization should succeed");
        assert_eq!(req.model, "gpt-x");
        assert!(req.input.is_empty());
        assert!(!req.stream);
        assert!(req.previous_response_id.is_none());
    }

    #[test]
    fn translate_function_call_creates_assistant_with_tool_calls() {
        let req = ResponsesRequest {
            model: "gpt-x".to_string(),
            instructions: None,
            input: vec![ResponsesInputItem::FunctionCall {
                call_id: "call_abc".to_string(),
                name: "get_weather".to_string(),
                arguments: r#"{"city":"NYC"}"#.to_string(),
            }],
            tools: None,
            tool_choice: None,
            stream: false,
            previous_response_id: None,
            extra: serde_json::Map::new(),
        };

        let openai = crate::middleware::auth::translate_responses_to_openai(&req);
        assert_eq!(openai.messages.len(), 1);
        assert_eq!(openai.messages[0].role, "assistant");
        let tool_calls = openai.messages[0]
            .tool_calls
            .as_ref()
            .expect("tool_calls present");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_abc");
        assert_eq!(tool_calls[0]["function"]["name"], "get_weather");
    }

    #[test]
    fn translate_empty_instructions_not_prepended() {
        let req = ResponsesRequest {
            model: "gpt-x".to_string(),
            instructions: Some(String::new()),
            input: vec![ResponsesInputItem::Message {
                role: "user".to_string(),
                content: ResponsesContent::Plain("hi".to_string()),
            }],
            tools: None,
            tool_choice: None,
            stream: false,
            previous_response_id: None,
            extra: serde_json::Map::new(),
        };

        let openai = crate::middleware::auth::translate_responses_to_openai(&req);
        // Empty instructions should NOT produce a system message
        assert_eq!(openai.messages.len(), 1);
        assert_eq!(openai.messages[0].role, "user");
    }
}