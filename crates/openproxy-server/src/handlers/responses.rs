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

use axum::{Router, extract::State, http::HeaderMap, response::IntoResponse, routing::post};

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
        let model = prepared.req.openai_request.model.clone();
        let request_id = prepared.req.request_id;
        let merged = PipelineRunner::spawn_streaming_bridge(
            pipeline,
            prepared.req,
            prepared.done_tx,
            prepared.stream_rx,
            openproxy_types::TargetFormat::Responses,
        );

        let sse_stream = openproxy_pipeline::translation::OpenAIToResponsesSseStream::new(
            merged,
            format!("resp_{request_id}"),
            model,
        );

        let body = axum::body::Body::from_stream(sse_stream);
        return Ok((
            [(
                axum::http::header::CONTENT_TYPE,
                "text/event-stream; charset=utf-8",
            )],
            body,
        )
            .into_response());
    }
    crate::handlers::chat::handle_sync_response_responses(pipeline, prepared.req, prepared.done_tx)
        .await
}

#[cfg(test)]
mod tests {
    use openproxy_types::{ResponsesContent, ResponsesInputItem, ResponsesRequest};
    use serde_json::json;

    fn make_req(inst: Option<&str>, input: Vec<ResponsesInputItem>) -> ResponsesRequest {
        ResponsesRequest {
            model: "gpt-x".into(),
            instructions: inst.map(|s| s.to_string()),
            input,
            tools: None,
            tool_choice: None,
            stream: false,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            previous_response_id: None,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn test_responses_translation_and_instructions() {
        // One message
        let r1 = make_req(
            None,
            vec![ResponsesInputItem::Message {
                role: "user".into(),
                content: ResponsesContent::Plain("hi".into()),
            }],
        );
        let o1 = crate::middleware::auth::translate_responses_to_openai(&r1);
        assert_eq!(o1.model, "gpt-x");
        assert_eq!(o1.messages[0].role, "user");
        assert_eq!(
            o1.messages[0].content.as_ref().and_then(|v| v.as_str()),
            Some("hi")
        );

        // Function call output
        let r2 = make_req(
            None,
            vec![ResponsesInputItem::FunctionCallOutput {
                call_id: "c1".into(),
                output: "pong".into(),
            }],
        );
        let o2 = crate::middleware::auth::translate_responses_to_openai(&r2);
        assert_eq!(o2.messages[0].role, "tool");
        assert_eq!(
            o2.messages[0].content.as_ref().and_then(|v| v.as_str()),
            Some("pong")
        );
        assert_eq!(o2.messages[0].tool_call_id.as_deref(), Some("c1"));

        // Instructions prepending & empty instruction
        let r3 = make_req(
            Some("be brief"),
            vec![ResponsesInputItem::Message {
                role: "user".into(),
                content: ResponsesContent::Plain("hi".into()),
            }],
        );
        let o3 = crate::middleware::auth::translate_responses_to_openai(&r3);
        assert_eq!(o3.messages.len(), 2);
        assert_eq!(o3.messages[0].role, "system");
        assert_eq!(
            o3.messages[0].content.as_ref().and_then(|v| v.as_str()),
            Some("be brief")
        );

        let r4 = make_req(
            Some(""),
            vec![ResponsesInputItem::Message {
                role: "user".into(),
                content: ResponsesContent::Plain("hi".into()),
            }],
        );
        assert_eq!(
            crate::middleware::auth::translate_responses_to_openai(&r4)
                .messages
                .len(),
            1
        );

        // Function call assistant mapping
        let r5 = make_req(
            None,
            vec![ResponsesInputItem::FunctionCall {
                call_id: "call_abc".into(),
                name: "get_weather".into(),
                arguments: r#"{"city":"NYC"}"#.into(),
            }],
        );
        let o5 = crate::middleware::auth::translate_responses_to_openai(&r5);
        assert_eq!(o5.messages[0].role, "assistant");
        let tc = o5.messages[0].tool_calls.as_ref().unwrap();
        assert_eq!(tc[0]["id"], "call_abc");
        assert_eq!(tc[0]["function"]["name"], "get_weather");
    }

    #[test]
    fn test_responses_payload_deserialization_and_forwarding() {
        let p1 = json!({"model": "gpt-x", "input": []});
        let req1: ResponsesRequest = serde_json::from_value(p1).unwrap();
        assert_eq!(req1.model, "gpt-x");
        assert!(req1.input.is_empty());
        assert!(!req1.stream);

        assert!(serde_json::from_value::<ResponsesRequest>(json!({"input": []})).is_err());
        assert!(
            serde_json::from_value::<ResponsesRequest>(
                json!({"model": "gpt-x", "input": [], "previous_response_id": 12345})
            )
            .is_err()
        );

        let p2 = json!({"model": "gpt-x", "input": [], "stream": true, "previous_response_id": "not-a-uuid-!!!@#$%"});
        let req2: ResponsesRequest = serde_json::from_value(p2).unwrap();
        assert!(req2.stream);
        assert_eq!(
            req2.previous_response_id.as_deref(),
            Some("not-a-uuid-!!!@#$%")
        );
        assert!(crate::middleware::auth::translate_responses_to_openai(&req2).stream);
    }

    #[test]
    fn test_responses_adversarial_items_and_ordering() {
        // Unknown items dropped
        let p_unk = json!({"model": "gpt-x", "input": [{"type": "reasoning"}, {"type": "image_generation"}, {"type": "weird"}]});
        let req_unk: ResponsesRequest = serde_json::from_value(p_unk).unwrap();
        assert!(
            crate::middleware::auth::translate_responses_to_openai(&req_unk)
                .messages
                .is_empty()
        );

        // Parts content preserved
        let p_parts = json!({"model": "gpt-x", "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}, {"type": "input_image", "url": "https://example.com/img.png"}]}]});
        let req_parts: ResponsesRequest = serde_json::from_value(p_parts).unwrap();
        let o_parts = crate::middleware::auth::translate_responses_to_openai(&req_parts);
        assert_eq!(
            o_parts.messages[0]
                .content
                .as_ref()
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );

        // Mixed types preserve order
        let p_mix = json!({
            "model": "gpt-x",
            "input": [
                {"type": "message", "role": "user", "content": "q1"},
                {"type": "function_call", "call_id": "c1", "name": "fn1", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "a1"},
                {"type": "message", "role": "user", "content": "q2"}
            ]
        });
        let req_mix: ResponsesRequest = serde_json::from_value(p_mix).unwrap();
        let o_mix = crate::middleware::auth::translate_responses_to_openai(&req_mix);
        let roles: Vec<_> = o_mix.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["user", "assistant", "tool", "user"]);

        // Duplicate call outputs
        let r_dup = make_req(
            None,
            vec![
                ResponsesInputItem::FunctionCall {
                    call_id: "c1".into(),
                    name: "do".into(),
                    arguments: "{}".into(),
                },
                ResponsesInputItem::FunctionCallOutput {
                    call_id: "c1".into(),
                    output: "1".into(),
                },
                ResponsesInputItem::FunctionCallOutput {
                    call_id: "c1".into(),
                    output: "2".into(),
                },
            ],
        );
        let o_dup = crate::middleware::auth::translate_responses_to_openai(&r_dup);
        assert_eq!(o_dup.messages.len(), 3);

        // Orphan call output
        let r_orph = make_req(
            None,
            vec![ResponsesInputItem::FunctionCallOutput {
                call_id: "orphan".into(),
                output: "res".into(),
            }],
        );
        assert_eq!(
            crate::middleware::auth::translate_responses_to_openai(&r_orph).messages[0]
                .tool_call_id
                .as_deref(),
            Some("orphan")
        );

        // Long instructions
        let big = "A".repeat(100_000);
        let r_big = make_req(Some(&big), vec![]);
        assert_eq!(
            crate::middleware::auth::translate_responses_to_openai(&r_big).messages[0]
                .content
                .as_ref()
                .unwrap()
                .as_str()
                .unwrap()
                .len(),
            100_000
        );

        // Large input count
        let input: Vec<_> = (0..1000)
            .map(|i| json!({"type": "message", "role": "user", "content": format!("msg {i}")}))
            .collect();
        let req_large: ResponsesRequest =
            serde_json::from_value(json!({"model": "gpt-x", "input": input})).unwrap();
        assert_eq!(
            crate::middleware::auth::translate_responses_to_openai(&req_large)
                .messages
                .len(),
            1000
        );
    }

    #[test]
    fn test_deepseek_harness_translation_and_tool_normalization() {
        let p = json!({
            "model": "nerd",
            "input": [
                {
                    "role": "system",
                    "content": "Create a concise title."
                },
                {
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hello world"}]
                }
            ],
            "tools": [
                {
                    "type": "function",
                    "name": "exec_cmd",
                    "description": "run a shell command",
                    "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}
                }
            ],
            "stream": true,
            "max_output_tokens": 128,
            "prompt_cache_key": "sess-xyz"
        });

        let req: ResponsesRequest = serde_json::from_value(p).unwrap();
        let openai_req = crate::middleware::auth::translate_responses_to_openai(&req);

        assert_eq!(openai_req.model, "nerd");
        assert_eq!(openai_req.max_tokens, Some(128));
        assert!(openai_req.stream);
        assert_eq!(openai_req.messages.len(), 2);
        assert_eq!(openai_req.messages[0].role, "system");
        assert_eq!(
            openai_req.messages[0].content,
            Some(json!("Create a concise title."))
        );
        assert_eq!(openai_req.messages[1].role, "user");
        assert_eq!(openai_req.messages[1].content, Some(json!("hello world")));

        // Verify tools normalized to nested function
        let tools = openai_req.tools.expect("tools present");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "exec_cmd");
        assert_eq!(tools[0]["function"]["description"], "run a shell command");
        assert!(tools[0]["function"]["parameters"].is_object());

        // Verify extra fields cleaned
        assert!(!openai_req.extra.contains_key("input"));
        assert!(!openai_req.extra.contains_key("max_output_tokens"));
        assert_eq!(
            openai_req.extra.get("prompt_cache_key"),
            Some(&json!("sess-xyz"))
        );
    }
}
