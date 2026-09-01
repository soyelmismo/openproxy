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
    let api_key_id: Option<openproxy_types::ids::ApiKeyId> = token_inner.as_ref().map(|r| r.key_id);

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
    crate::handlers::chat::handle_sync_response_responses(pipeline, prepared.req, prepared.done_tx)
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
        assert_eq!(openai.messages[0].tool_call_id.as_deref(), Some("c1"));
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

// ============================================================
// GAP-2: Adversarial tests for Responses handler translation
// ============================================================
#[cfg(test)]
mod responses_adversarial_tests {
    use openproxy_types::{ResponsesInputItem, ResponsesRequest};
    use serde_json::json;

    // --- Unknown input item types ---

    #[test]
    fn adv_unknown_item_type_is_dropped() {
        let payload = json!({
            "model": "gpt-x",
            "input": [
                {"type": "reasoning"},
                {"type": "image_generation"},
                {"type": "weird_new_type", "data": "something"}
            ]
        });
        let req: ResponsesRequest =
            serde_json::from_value(payload).expect("deserialization should succeed");
        let openai = crate::middleware::auth::translate_responses_to_openai(&req);
        assert!(
            openai.messages.is_empty(),
            "all unknown items must be dropped"
        );
    }

    #[test]
    fn adv_10000_input_items_all_produce_messages() {
        // 10000 items — a memory/CPU stress test.
        let input: Vec<serde_json::Value> = (0..10_000)
            .map(|i| {
                json!({
                    "type": "message",
                    "role": "user",
                    "content": format!("msg {i}")
                })
            })
            .collect();
        let payload = json!({
            "model": "gpt-x",
            "input": input
        });
        let req: ResponsesRequest =
            serde_json::from_value(payload).expect("deserialization should succeed");
        let openai = crate::middleware::auth::translate_responses_to_openai(&req);
        assert_eq!(openai.messages.len(), 10_000);
    }

    // --- Instructions edge cases ---

    #[test]
    fn adv_very_long_instructions_preserved() {
        // 10MB of instructions text.
        let big = "A".repeat(10_000_000);
        let req = ResponsesRequest {
            model: "gpt-x".to_string(),
            instructions: Some(big),
            input: vec![],
            tools: None,
            tool_choice: None,
            stream: false,
            previous_response_id: None,
            extra: json!({}).as_object().unwrap().clone(),
        };
        let openai = crate::middleware::auth::translate_responses_to_openai(&req);
        assert_eq!(openai.messages.len(), 1);
        assert_eq!(openai.messages[0].role, "system");
        let content = openai.messages[0]
            .content
            .as_ref()
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(content.len(), 10_000_000);
    }

    // --- previous_response_id edge cases ---

    #[test]
    fn adv_previous_response_id_non_uuid_preserved_in_extra() {
        // previous_response_id is a string, so it will be parsed into
        // ResponsesRequest. The handler logs a warning but proceeds.
        let payload = json!({
            "model": "gpt-x",
            "input": [],
            "previous_response_id": "not-a-uuid-!!!@#$%"
        });
        let req: ResponsesRequest =
            serde_json::from_value(payload).expect("deserialization should succeed");
        assert_eq!(
            req.previous_response_id.as_deref(),
            Some("not-a-uuid-!!!@#$%")
        );
    }

    #[test]
    fn adv_previous_response_id_number_type() {
        // previous_response_id as a number (non-string) — serde should
        // fail because the field type is Option<String>.
        let payload = json!({
            "model": "gpt-x",
            "input": [],
            "previous_response_id": 12345
        });
        let result = serde_json::from_value::<ResponsesRequest>(payload);
        assert!(
            result.is_err(),
            "numeric previous_response_id must fail to deserialize"
        );
    }

    // --- function_call without preceding FunctionCall ---

    #[test]
    fn adv_function_call_output_without_matching_function_call() {
        // FunctionCallOutput without a preceding FunctionCall with matching call_id.
        // The auth middleware's sanitize_tool_calls will drop the orphan tool message.
        let req = ResponsesRequest {
            model: "gpt-x".to_string(),
            instructions: None,
            input: vec![ResponsesInputItem::FunctionCallOutput {
                call_id: "call_orphan".to_string(),
                output: "result".to_string(),
            }],
            tools: None,
            tool_choice: None,
            stream: false,
            previous_response_id: None,
            extra: json!({}).as_object().unwrap().clone(),
        };
        let openai = crate::middleware::auth::translate_responses_to_openai(&req);
        // The translate function produces the tool message; sanitize_tool_calls
        // in the middleware is responsible for dropping orphans.
        // In the raw translation, the tool message is produced.
        assert_eq!(openai.messages.len(), 1);
        assert_eq!(openai.messages[0].role, "tool");
        assert_eq!(
            openai.messages[0].tool_call_id.as_deref(),
            Some("call_orphan")
        );
    }

    // --- Duplicate function_call_output with same call_id ---

    #[test]
    fn adv_duplicate_function_call_output_same_call_id() {
        // Two FunctionCallOutput with the same call_id → both translated.
        // sanitize_tool_calls will keep only the first match.
        let req = ResponsesRequest {
            model: "gpt-x".to_string(),
            instructions: None,
            input: vec![
                ResponsesInputItem::FunctionCall {
                    call_id: "call_1".to_string(),
                    name: "do_thing".to_string(),
                    arguments: "{}".to_string(),
                },
                ResponsesInputItem::FunctionCallOutput {
                    call_id: "call_1".to_string(),
                    output: "first".to_string(),
                },
                ResponsesInputItem::FunctionCallOutput {
                    call_id: "call_1".to_string(),
                    output: "second".to_string(),
                },
            ],
            tools: None,
            tool_choice: None,
            stream: false,
            previous_response_id: None,
            extra: json!({}).as_object().unwrap().clone(),
        };
        let openai = crate::middleware::auth::translate_responses_to_openai(&req);
        // The translation produces all 3 messages (1 assistant + 2 tool).
        // The sanitize step in middleware will handle the duplicates.
        assert_eq!(openai.messages.len(), 3);
        assert_eq!(openai.messages[0].role, "assistant");
        assert_eq!(openai.messages[1].role, "tool");
        assert_eq!(openai.messages[2].role, "tool");
    }

    // --- Content as Parts (array of objects) ---

    #[test]
    fn adv_message_content_parts_preserved() {
        let payload = json!({
            "model": "gpt-x",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "hello"},
                    {"type": "input_image", "url": "https://example.com/img.png"}
                ]
            }]
        });
        let req: ResponsesRequest =
            serde_json::from_value(payload).expect("deserialization should succeed");
        let openai = crate::middleware::auth::translate_responses_to_openai(&req);
        assert_eq!(openai.messages.len(), 1);
        // Parts are preserved as a JSON array
        let content = openai.messages[0].content.as_ref().unwrap();
        assert!(content.is_array(), "Parts content must be a JSON array");
        assert_eq!(content.as_array().unwrap().len(), 2);
    }

    // --- Deserialization of minimal payload ---

    #[test]
    fn adv_deserialization_minimal_payload() {
        let payload = json!({
            "model": "gpt-x",
            "input": []
        });
        let req: ResponsesRequest =
            serde_json::from_value(payload).expect("minimal payload must deserialize");
        assert_eq!(req.model, "gpt-x");
        assert!(req.input.is_empty());
        assert!(!req.stream);
        assert!(req.previous_response_id.is_none());
        assert!(req.tools.is_none());
        assert!(req.tool_choice.is_none());
    }

    #[test]
    fn adv_deserialization_missing_model_fails() {
        let payload = json!({"input": []});
        let result = serde_json::from_value::<ResponsesRequest>(payload);
        assert!(
            result.is_err(),
            "missing model field must fail deserialization"
        );
    }

    // --- stream flag forwarded ---

    #[test]
    fn adv_stream_true_forwarded() {
        let payload = json!({
            "model": "gpt-x",
            "input": [],
            "stream": true
        });
        let req: ResponsesRequest = serde_json::from_value(payload).expect("deserialize");
        assert!(req.stream);
        let openai = crate::middleware::auth::translate_responses_to_openai(&req);
        assert!(openai.stream);
    }

    // --- Mixed input types ---

    #[test]
    fn adv_mixed_input_types_preserve_order() {
        let payload = json!({
            "model": "gpt-x",
            "input": [
                {"type": "message", "role": "user", "content": "q1"},
                {"type": "function_call", "call_id": "c1", "name": "fn1", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "a1"},
                {"type": "message", "role": "user", "content": "q2"}
            ]
        });
        let req: ResponsesRequest = serde_json::from_value(payload).expect("deserialize");
        let openai = crate::middleware::auth::translate_responses_to_openai(&req);
        assert_eq!(openai.messages.len(), 4);
        assert_eq!(openai.messages[0].role, "user");
        assert_eq!(openai.messages[1].role, "assistant");
        assert_eq!(openai.messages[2].role, "tool");
        assert_eq!(openai.messages[3].role, "user");
    }
}
