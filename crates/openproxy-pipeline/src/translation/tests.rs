use openproxy_types::{OpenAIMessage, OpenAIRequest};
use serde_json::{Value, json};
use crate::translation::*;


    fn openai_req_with(messages: Vec<(&str, &str)>) -> OpenAIRequest {
        OpenAIRequest {
            model: "claude-test".to_string(),
            messages: messages
                .into_iter()
                .map(|(role, content)| OpenAIMessage {
                    role: role.to_string(),
                    content: Some(Value::String(content.to_string())),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: serde_json::Map::new(),
                })
                .collect(),
            stream: false,
            temperature: Some(0.5),
            max_tokens: None,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            top_k: None,
            user: None,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn openai_to_anthropic_extracts_system() {
        let req = openai_req_with(vec![
            ("system", "You are helpful."),
            ("system", "Be concise."),
            ("user", "Hi"),
            ("assistant", "Hello!"),
        ]);

        let out = openai_to_anthropic(&req, "claude-3-opus-20240229", &req.messages, req.stream);
        assert_eq!(
            out.system,
            Some(json!("You are helpful.\n\nBe concise."))
        );
        assert_eq!(out.messages.len(), 2);
        assert_eq!(out.messages[0].role, "user");
        assert_eq!(out.messages[0].content, "Hi");
        assert_eq!(out.messages[1].role, "assistant");
        assert_eq!(out.messages[1].content, "Hello!");
    }

    #[test]
    fn openai_to_anthropic_no_system() {
        let req = openai_req_with(vec![("user", "Hi"), ("assistant", "Hello!")]);
        let out = openai_to_anthropic(&req, "claude-3-opus-20240229", &req.messages, req.stream);
        assert!(out.system.is_none());
        assert_eq!(out.messages.len(), 2);
    }

    #[test]
    fn openai_to_anthropic_default_max_tokens() {
        let mut req = openai_req_with(vec![("user", "Hi")]);
        req.max_tokens = None;
        let out = openai_to_anthropic(&req, "claude-3-opus-20240229", &req.messages, req.stream);
        assert_eq!(out.max_tokens, DEFAULT_MAX_TOKENS);

        // When the client does provide max_tokens, it's preserved.
        let mut req = openai_req_with(vec![("user", "Hi")]);
        req.max_tokens = Some(123);
        let out = openai_to_anthropic(&req, "claude-3-opus-20240229", &req.messages, req.stream);
        assert_eq!(out.max_tokens, 123);
    }

    #[test]
    fn anthropic_to_openai_concat_text_blocks() {
        let resp = AnthropicResponse {
            id: "msg_1".to_string(),
            response_type: "message".to_string(),
            role: "assistant".to_string(),
            content: vec![
                json!({
                    "type": "text",
                    "text": "Hello, "
                }),
                json!({
                    "type": "text",
                    "text": "world!"
                }),
            ],
            model: "claude-test".to_string(),
            stop_reason: Some("end_turn".to_string()),
            usage: AnthropicUsage {
                input_tokens: 10,
                output_tokens: 5,
            },
        };

        let out = anthropic_to_openai(&resp);
        assert_eq!(out.id, "msg_1");
        assert_eq!(out.object, "chat.completion");
        assert_eq!(out.model, "claude-test");
        assert_eq!(out.choices.len(), 1);
        assert_eq!(out.choices[0].index, 0);
        assert_eq!(out.choices[0].message.role, "assistant");
        assert_eq!(
            out.choices[0]
                .message
                .content
                .as_ref()
                .and_then(Value::as_str),
            Some("Hello, world!")
        );
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn anthropic_to_openai_maps_usage() {
        let resp = AnthropicResponse {
            id: "msg_1".to_string(),
            response_type: "message".to_string(),
            role: "assistant".to_string(),
            content: vec![json!({
                "type": "text",
                "text": "ok",
            })],
            model: "claude-test".to_string(),
            stop_reason: Some("max_tokens".to_string()),
            usage: AnthropicUsage {
                input_tokens: 7,
                output_tokens: 11,
            },
        };

        let out = anthropic_to_openai(&resp);
        let usage = out.usage.expect("usage should be present");
        assert_eq!(usage.prompt_tokens, 7);
        assert_eq!(usage.completion_tokens, 11);
        assert_eq!(usage.total_tokens, 18);
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("length"));
    }

    #[test]
    fn parse_anthropic_sse_line_text_delta() {
        let line = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#;
        let event = parse_anthropic_sse_line(line)
            .expect("parse ok")
            .expect("event present");
        match event {
            AnthropicSseEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(index, 0);
                assert_eq!(delta.get("text").and_then(|v| v.as_str()), Some("hi"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn parse_anthropic_sse_line_message_start() {
        let line = r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"claude-test","stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}}"#;
        let event = parse_anthropic_sse_line(line)
            .expect("parse ok")
            .expect("event present");
        match event {
            AnthropicSseEvent::MessageStart { message } => {
                assert_eq!(message.id, "msg_1");
                assert_eq!(message.model, "claude-test");
                assert_eq!(message.usage.input_tokens, 1);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn parse_anthropic_sse_line_ignores_ping() {
        let line = r#"data: {"type":"ping"}"#;
        let res = parse_anthropic_sse_line(line).expect("parse ok");
        assert!(res.is_none());
    }

    #[test]
    fn anthropic_sse_to_openai_text_delta_produces_chunk() {
        let event = AnthropicSseEvent::ContentBlockDelta {
            index: 0,
            delta: json!({ "type": "text_delta", "text": "hello" }),
        };
        let chunks = anthropic_sse_to_openai_chunks(&event, "chunk-1", 1700000000, "claude-test");
        assert_eq!(chunks.len(), 1);

        let payload = chunks[0]
            .trim_start_matches("data: ")
            .trim_end()
            .trim_end_matches('\n');
        let v: serde_json::Value = serde_json::from_str(payload).expect("valid json");
        assert_eq!(v["id"], "chunk-1");
        assert_eq!(v["object"], "chat.completion.chunk");
        assert_eq!(v["created"], 1700000000u64);
        assert_eq!(v["model"], "claude-test");
        assert_eq!(v["choices"][0]["index"], 0);
        assert_eq!(v["choices"][0]["delta"]["content"], "hello");
        assert!(v["choices"][0]["finish_reason"].is_null());
    }

    #[test]
    fn anthropic_sse_to_openai_message_stop_produces_done() {
        let event = AnthropicSseEvent::MessageStop;
        let chunks = anthropic_sse_to_openai_chunks(&event, "chunk-1", 1700000000, "claude-test");

        // Last frame is the [DONE] sentinel.
        assert_eq!(chunks.last().map(String::as_str), Some("data: [DONE]\n\n"));

        // Some preceding chunk carries finish_reason=stop.
        let has_stop = chunks.iter().any(|c| {
            let payload = c
                .trim_start_matches("data: ")
                .trim_end()
                .trim_end_matches('\n');
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
                v["choices"][0]["finish_reason"] == "stop"
            } else {
                false
            }
        });
        assert!(has_stop, "expected a chunk with finish_reason=stop");
    }

    // ---- Gemini -------------------------------------------------------

    #[test]
    fn openai_to_gemini_extracts_system() {
        let req = openai_req_with(vec![
            ("system", "You are helpful."),
            ("system", "Be concise."),
            ("user", "Hi"),
            ("assistant", "Hello!"),
        ]);

        let out = openai_to_gemini(&req, &req.messages);
        let sys = out.system_instruction.as_ref().unwrap();
        assert_eq!(sys.role, "system");
        let text = sys.parts[0].text.as_ref().unwrap();
        assert_eq!(text, "You are helpful.\n\nBe concise.");
        assert_eq!(out.contents.len(), 2);
        assert_eq!(out.contents[0].role, "user");
        assert_eq!(out.contents[1].role, "model");
    }

    #[test]
    fn openai_to_gemini_no_system() {
        let req = openai_req_with(vec![("user", "Hi"), ("assistant", "Hello!")]);
        let out = openai_to_gemini(&req, &req.messages);
        assert!(out.system_instruction.is_none());
        assert_eq!(out.contents.len(), 2);
    }

    #[test]
    fn openai_to_gemini_default_max_output_tokens() {
        let mut req = openai_req_with(vec![("user", "Hi")]);
        req.max_tokens = None;
        let out = openai_to_gemini(&req, &req.messages);
        let gen_cfg = out.generation_config.as_ref().unwrap();
        assert_eq!(
            gen_cfg.max_output_tokens,
            Some(DEFAULT_GEMINI_MAX_OUTPUT_TOKENS)
        );

        // When the client does provide max_tokens, it's preserved.
        let mut req = openai_req_with(vec![("user", "Hi")]);
        req.max_tokens = Some(123);
        let out = openai_to_gemini(&req, &req.messages);
        let gen_cfg = out.generation_config.as_ref().unwrap();
        assert_eq!(gen_cfg.max_output_tokens, Some(123));
    }

    #[test]
    fn openai_to_gemini_temperature_and_top_p() {
        let mut req = openai_req_with(vec![("user", "Hi")]);
        req.temperature = Some(0.7);
        req.top_p = Some(0.9);
        let out = openai_to_gemini(&req, &req.messages);
        let gen_cfg = out.generation_config.as_ref().unwrap();
        assert_eq!(gen_cfg.temperature, Some(0.7));
        assert_eq!(gen_cfg.top_p, Some(0.9));
    }

    #[test]
    fn gemini_to_openai_extracts_content() {
        let resp = GeminiResponse {
            candidates: vec![GeminiCandidate {
                content: Some(GeminiContent {
                    role: "model".to_string(),
                    parts: vec![GeminiPart {
                        text: Some("Hello, world!".to_string()),
                        ..Default::default()
                    }],
                }),
                finish_reason: Some("STOP".to_string()),
            }],
            usage_metadata: Some(GeminiUsageMetadata {
                prompt_token_count: 10,
                candidates_token_count: 5,
                total_token_count: 15,
            }),
            response: None,
        };

        let out = gemini_to_openai(&resp);
        assert_eq!(out.choices.len(), 1);
        assert_eq!(out.choices[0].message.role, "assistant");
        assert_eq!(
            out.choices[0]
                .message
                .content
                .as_ref()
                .and_then(Value::as_str),
            Some("Hello, world!")
        );
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = out.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn gemini_to_openai_maps_finish_reason() {
        let resp = GeminiResponse {
            candidates: vec![GeminiCandidate {
                content: Some(GeminiContent {
                    role: "model".to_string(),
                    parts: vec![GeminiPart {
                        text: Some("ok".to_string()),
                        ..Default::default()
                    }],
                }),
                finish_reason: Some("MAX_TOKENS".to_string()),
            }],
            usage_metadata: None,
            response: None,
        };

        let out = gemini_to_openai(&resp);
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("length"));
    }

    #[test]
    fn gemini_to_openai_empty_response() {
        let resp = GeminiResponse {
            candidates: vec![],
            usage_metadata: None,
            response: None,
        };

        let out = gemini_to_openai(&resp);
        assert_eq!(out.choices.len(), 1);
        assert_eq!(
            out.choices[0]
                .message
                .content
                .as_ref()
                .and_then(Value::as_str),
            Some("")
        );
    }

    #[test]
    fn openai_message_preserves_tool_call_id() {
        let raw = r#"{"model":"test","messages":[{"role":"user","content":"call tool"},{"role":"tool","tool_call_id":"call_abc","content":"result"}],"stream":false}"#;
        let req: OpenAIRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.messages[1].tool_call_id.as_deref(), Some("call_abc"));
        let serialized = serde_json::to_value(&req).unwrap();
        assert_eq!(serialized["messages"][1]["tool_call_id"], "call_abc");
    }

    #[test]
    fn openai_message_preserves_null_content_and_tool_calls() {
        let raw = r#"{"model":"test","messages":[{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"foo","arguments":"{}"}}]}],"stream":false}"#;
        let req: OpenAIRequest = serde_json::from_str(raw).unwrap();
        let msg = &req.messages[0];
        assert!(msg.content.as_ref().map(|v| v.is_null()).unwrap_or(false));
        assert_eq!(msg.tool_calls.as_ref().map(|v| v.len()), Some(1));
        let serialized = serde_json::to_value(&req).unwrap();
        assert_eq!(
            serialized["messages"][0]["content"],
            serde_json::Value::Null
        );
        assert_eq!(
            serialized["messages"][0]["tool_calls"],
            serde_json::json!([{"id":"call_1","type":"function","function":{"name":"foo","arguments":"{}"}}])
        );
    }

    #[test]
    fn openai_message_preserves_content_array() {
        let raw = r#"{"model":"test","messages":[{"role":"user","content":[{"type":"text","text":"hello"},{"type":"image_url","image_url":{"url":"https://example.com/img.png"}}]}],"stream":false}"#;
        let req: OpenAIRequest = serde_json::from_str(raw).unwrap();
        let msg = &req.messages[0];
        assert!(msg.content.as_ref().map(|v| v.is_array()).unwrap_or(false));
        let serialized = serde_json::to_value(&req).unwrap();
        let arr = &serialized["messages"][0]["content"];
        assert!(arr.is_array());
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[1]["type"], "image_url");
    }

    // ---- H4 fix: function-calling fields TRANSLATED to Anthropic shape ----
    //
    // The original H4 fix passed `tools` and `tool_choice` through verbatim
    // in OpenAI shape. That was wrong: Anthropic (and MiniMax's Anthropic-
    // compatible API) expect a different shape, and reject OpenAI-shaped
    // tools with `(2013) function name or parameters is empty`. These
    // tests now assert the translation.

    #[test]
    fn h4_tools_array_translated_to_anthropic_shape() {
        // OpenAI shape: {type:"function", function:{name, description, parameters}}
        // Anthropic shape: {name, description, input_schema}
        let mut req = openai_req_with(vec![("user", "What is the weather in SF?")]);
        req.tools = Some(vec![json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Look up weather",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "location": {"type": "string"}
                    },
                    "required": ["location"]
                }
            }
        })]);
        let out = openai_to_anthropic(&req, "claude-3-opus-20240229", &req.messages, req.stream);
        let tools = out.tools.as_ref().expect("tools should be translated");
        assert_eq!(tools.len(), 1);
        // Top-level keys are Anthropic shape, NOT OpenAI shape.
        assert_eq!(tools[0]["name"], "get_weather");
        assert_eq!(tools[0]["description"], "Look up weather");
        assert_eq!(tools[0]["input_schema"]["type"], "object");
        assert_eq!(tools[0]["input_schema"]["required"][0], "location");
        // The OpenAI `function` wrapper must NOT be present.
        assert!(tools[0].get("function").is_none());
        assert!(tools[0].get("type").is_none());
    }

    #[test]
    fn h4_tool_choice_translated_to_anthropic_shape() {
        let mut req = openai_req_with(vec![("user", "go")]);

        // String "auto" → {"type":"auto"}
        req.tool_choice = Some(json!("auto"));
        let out = openai_to_anthropic(&req, "claude-3-opus-20240229", &req.messages, req.stream);
        assert_eq!(out.tool_choice.as_ref().unwrap(), &json!({"type": "auto"}));

        // String "none" → {"type":"none"}
        req.tool_choice = Some(json!("none"));
        let out = openai_to_anthropic(&req, "claude-3-opus-20240229", &req.messages, req.stream);
        assert_eq!(out.tool_choice.as_ref().unwrap(), &json!({"type": "none"}));

        // String "required" → {"type":"any"} (Anthropic's name for "force a tool call")
        req.tool_choice = Some(json!("required"));
        let out = openai_to_anthropic(&req, "claude-3-opus-20240229", &req.messages, req.stream);
        assert_eq!(out.tool_choice.as_ref().unwrap(), &json!({"type": "any"}));

        // Object form {type:"function", function:{name:"X"}}
        // → Anthropic {type:"tool", name:"X"}
        req.tool_choice = Some(json!({
            "type": "function",
            "function": {"name": "search"}
        }));
        let out = openai_to_anthropic(&req, "claude-3-opus-20240229", &req.messages, req.stream);
        let tc = out.tool_choice.as_ref().unwrap();
        assert_eq!(tc["type"], "tool");
        assert_eq!(tc["name"], "search");
        // The OpenAI `function` wrapper must NOT be present.
        assert!(tc.get("function").is_none());
    }

    #[test]
    fn minimax_tools_with_empty_name_are_filtered_out() {
        // MiniMax rejects tools with empty `name` with `(2013)`.
        // The translator must filter them out before sending.
        let mut req = openai_req_with(vec![("user", "go")]);
        req.tools = Some(vec![
            json!({
                "type": "function",
                "function": {
                    "name": "valid_tool",
                    "description": "This one is fine",
                    "parameters": {"type": "object"}
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "",
                    "description": "This one has an empty name",
                    "parameters": {"type": "object"}
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "description": "This one has no name at all",
                    "parameters": {"type": "object"}
                }
            }),
        ]);
        let out = openai_to_anthropic(&req, "claude-3-opus-20240229", &req.messages, req.stream);
        let tools = out.tools.as_ref().expect("tools should be present");
        assert_eq!(
            tools.len(),
            1,
            "only the tool with a non-empty name should survive"
        );
        assert_eq!(tools[0]["name"], "valid_tool");
    }

    #[test]
    fn minimax_assistant_tool_calls_translated_to_tool_use_blocks() {
        // OpenAI assistant message with `tool_calls` must be translated
        // to Anthropic `tool_use` content blocks. The `arguments` string
        // must be parsed to a JSON object for Anthropic's `input` field.
        let mut req = openai_req_with(vec![]);
        req.messages = vec![
            OpenAIMessage {
                role: "user".to_string(),
                content: Some(json!("What's the weather in Paris?")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::Value::Null),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_abc123",
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "arguments": "{\"city\": \"Paris\"}"
                    }
                })]),
                extra: serde_json::Map::new(),
            },
        ];
        let out = openai_to_anthropic(&req, "claude-3-opus-20240229", &req.messages, req.stream);
        assert_eq!(out.messages.len(), 2);
        // The assistant message should have an array content with a tool_use block.
        let assistant_msg = &out.messages[1];
        assert_eq!(assistant_msg.role, "assistant");
        let blocks = assistant_msg
            .content
            .as_array()
            .expect("content should be an array");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[0]["id"], "call_abc123");
        assert_eq!(blocks[0]["name"], "get_weather");
        // `arguments` string was parsed to a JSON object for `input`.
        assert_eq!(blocks[0]["input"]["city"], "Paris");
    }

    #[test]
    fn minimax_tool_role_message_translated_to_tool_result_block() {
        // OpenAI `tool`-role message must be translated to Anthropic
        // `tool_result` content block under a `user`-role message.
        let mut req = openai_req_with(vec![]);
        req.messages = vec![
            OpenAIMessage {
                role: "user".to_string(),
                content: Some(json!("What's the weather?")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::Value::Null),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_xyz",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
                })]),
                extra: serde_json::Map::new(),
            },
            OpenAIMessage {
                role: "tool".to_string(),
                content: Some(json!("{\"temp\": 18}")),
                name: None,
                tool_call_id: Some("call_xyz".to_string()),
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
        ];
        let out = openai_to_anthropic(&req, "claude-3-opus-20240229", &req.messages, req.stream);
        assert_eq!(out.messages.len(), 3);
        // The third message (OpenAI `tool`-role) should become a
        // `user`-role message with a `tool_result` content block.
        let tool_result_msg = &out.messages[2];
        assert_eq!(tool_result_msg.role, "user");
        let blocks = tool_result_msg
            .content
            .as_array()
            .expect("content should be an array");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "call_xyz");
        assert_eq!(blocks[0]["content"], "{\"temp\": 18}");
    }

    #[test]
    fn minimax_assistant_tool_calls_with_empty_name_are_skipped() {
        // A tool_call with an empty `name` would trigger MiniMax's
        // `(2013)` rejection. The translator must skip it.
        let mut req = openai_req_with(vec![]);
        req.messages = vec![
            OpenAIMessage {
                role: "user".to_string(),
                content: Some(json!("go")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(json!("Thinking...")),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![
                    json!({
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "", "arguments": "{}"}
                    }),
                    json!({
                        "id": "call_2",
                        "type": "function",
                        "function": {"name": "valid_tool", "arguments": "{\"x\":1}"}
                    }),
                ]),
                extra: serde_json::Map::new(),
            },
        ];
        let out = openai_to_anthropic(&req, "claude-3-opus-20240229", &req.messages, req.stream);
        let assistant_msg = &out.messages[1];
        let blocks = assistant_msg
            .content
            .as_array()
            .expect("content should be an array");
        // text block + 1 valid tool_use block (the empty-name one is skipped)
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "Thinking...");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["name"], "valid_tool");
    }

    #[test]
    fn minimax_tool_calls_with_empty_arguments_become_empty_object() {
        // Anthropic requires `input` to be present (even if empty).
        // Empty `arguments` string → `input: {}`.
        let mut req = openai_req_with(vec![]);
        req.messages = vec![OpenAIMessage {
            role: "assistant".to_string(),
            content: Some(serde_json::Value::Null),
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![json!({
                "id": "call_1",
                "type": "function",
                "function": {"name": "no_args_tool", "arguments": ""}
            })]),
            extra: serde_json::Map::new(),
        }];
        let out = openai_to_anthropic(&req, "claude-3-opus-20240229", &req.messages, req.stream);
        let assistant_msg = &out.messages[0];
        let blocks = assistant_msg
            .content
            .as_array()
            .expect("content should be an array");
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[0]["input"], json!({}));
    }

    #[test]
    fn minimax_full_tool_round_trip_request_shape() {
        // End-to-end: a complete OpenAI tool-calling conversation
        // translated to the Anthropic shape MiniMax expects. This is
        // the exact scenario that was failing with `(2013)`.
        let mut req = openai_req_with(vec![]);
        req.model = "MiniMax-M3".to_string();
        req.tools = Some(vec![json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get weather for a city",
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }
            }
        })]);
        req.tool_choice = Some(json!("auto"));
        req.messages = vec![
            OpenAIMessage {
                role: "user".to_string(),
                content: Some(json!("What's the weather in Paris?")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::Value::Null),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
                })]),
                extra: serde_json::Map::new(),
            },
            OpenAIMessage {
                role: "tool".to_string(),
                content: Some(json!("{\"temp\":18,\"unit\":\"c\"}")),
                name: None,
                tool_call_id: Some("call_1".to_string()),
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
        ];
        let out = openai_to_anthropic(&req, "claude-3-opus-20240229", &req.messages, req.stream);

        // Tools: Anthropic shape with name/description/input_schema.
        let tools = out.tools.as_ref().expect("tools present");
        assert_eq!(tools[0]["name"], "get_weather");
        assert_eq!(
            tools[0]["input_schema"]["properties"]["city"]["type"],
            "string"
        );

        // tool_choice: {"type":"auto"}
        assert_eq!(out.tool_choice.as_ref().unwrap(), &json!({"type": "auto"}));

        // Messages: 3 entries (user, assistant with tool_use, user with tool_result)
        assert_eq!(out.messages.len(), 3);
        assert_eq!(out.messages[0].role, "user");
        assert_eq!(
            out.messages[0].content,
            json!("What's the weather in Paris?")
        );

        let asst_blocks = out.messages[1].content.as_array().unwrap();
        assert_eq!(asst_blocks[0]["type"], "tool_use");
        assert_eq!(asst_blocks[0]["name"], "get_weather");
        assert_eq!(asst_blocks[0]["input"]["city"], "Paris");

        let tool_blocks = out.messages[2].content.as_array().unwrap();
        assert_eq!(tool_blocks[0]["type"], "tool_result");
        assert_eq!(tool_blocks[0]["tool_use_id"], "call_1");
        assert_eq!(tool_blocks[0]["content"], "{\"temp\":18,\"unit\":\"c\"}");

        // Serialize and verify the JSON shape matches what MiniMax expects.
        let serialized = serde_json::to_value(&out).unwrap();
        // No `function` wrapper anywhere in tools.
        assert!(serialized["tools"][0].get("function").is_none());
        // No `type:"function"` in tools.
        assert!(serialized["tools"][0].get("type").is_none());
        // tool_choice is the Anthropic object form.
        assert_eq!(serialized["tool_choice"]["type"], "auto");
    }

    #[test]
    fn h4_top_k_passes_through_to_anthropic() {
        let mut req = openai_req_with(vec![("user", "go")]);
        req.top_k = Some(40);
        let out = openai_to_anthropic(&req, "claude-3-opus-20240229", &req.messages, req.stream);
        assert_eq!(out.top_k, Some(40));
    }

    #[test]
    fn minimax_tool_result_then_user_merges_into_single_user_message() {
        // Regression test for MiniMax error (2013) "tool call result
        // does not follow tool call". Two bugs were fixed:
        //
        // 1. When a user message follows a tool_result, the translator
        //    emitted TWO consecutive user messages — merged into one.
        // 2. Consecutive assistant text messages were emitted as
        //    separate assistant messages — merged into one by joining
        //    text with newlines.
        //
        // Sequence (from real MiniMax-M3 failure):
        //   user("que falta?")
        //   assistant("text1") × 18 consecutive  ← BUG #2
        //   user("You've reached max iterations")
        //   assistant("text2") × 2 consecutive   ← BUG #2
        //   user(...)
        //   assistant(tool_calls=[A,B])
        //   tool(A) → tool(B)
        //   assistant(tool_calls=[C])  (empty content)
        //   tool(C)
        //   user("no se que mecanismo es...")    ← BUG #1
        let mut req = openai_req_with(vec![]);
        req.model = "MiniMax-M3".to_string();
        req.tools = Some(vec![json!({
            "type": "function",
            "function": {
                "name": "search_files",
                "description": "Search files",
                "parameters": {"type": "object", "properties": {}}
            }
        })]);
        req.messages = vec![
            // user asks
            OpenAIMessage {
                role: "user".to_string(),
                content: Some(json!("que falta?")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            // 3 consecutive assistant text messages (simulating the
            // 18-consecutive-assistant pattern from the real failure)
            OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(json!("Veo cómo se renderiza:")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(json!("Eso no es el render del bloque...")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(json!("El bloque core/html no tiene render_callback...")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            // user message after consecutive assistants
            OpenAIMessage {
                role: "user".to_string(),
                content: Some(json!(
                    "You've reached the maximum number of tool-calling iterations."
                )),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            // 2 consecutive assistant text messages
            OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(json!("**0 placeholders sin reemplazar.**")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(json!("Falta muy poco. Repaso el checklist.")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            // user
            OpenAIMessage {
                role: "user".to_string(),
                content: Some(json!("Seleccionar reporte_mensual...")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            // assistant with 2 tool_calls
            OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(json!("Diagnóstico")),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![
                    json!({"id":"call_A","type":"function","function":{"name":"search_files","arguments":"{}"}}),
                    json!({"id":"call_B","type":"function","function":{"name":"search_files","arguments":"{}"}}),
                ]),
                extra: serde_json::Map::new(),
            },
            // tool results A and B
            OpenAIMessage {
                role: "tool".to_string(),
                content: Some(json!("result A")),
                name: None,
                tool_call_id: Some("call_A".to_string()),
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            OpenAIMessage {
                role: "tool".to_string(),
                content: Some(json!("result B")),
                name: None,
                tool_call_id: Some("call_B".to_string()),
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            // assistant with 1 tool_call (empty content)
            OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::Value::Null),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![
                    json!({"id":"call_C","type":"function","function":{"name":"search_files","arguments":"{}"}}),
                ]),
                extra: serde_json::Map::new(),
            },
            // tool result C
            OpenAIMessage {
                role: "tool".to_string(),
                content: Some(json!("result C")),
                name: None,
                tool_call_id: Some("call_C".to_string()),
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            // user message — tool_result + user merge
            OpenAIMessage {
                role: "user".to_string(),
                content: Some(json!("no se que mecanismo es...")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
        ];
        let out = openai_to_anthropic(&req, "claude-3-opus-20240229", &req.messages, req.stream);

        // Expected Anthropic message sequence (after merging):
        //   [0] user("que falta?")
        //   [1] assistant("Veo...\n\nEso no es...\n\nEl bloque...")  ← 3 merged
        //   [2] user("You've reached...")
        //   [3] assistant("**0 placeholders...\n\nFalta muy poco...") ← 2 merged
        //   [4] user("Seleccionar reporte_mensual...")
        //   [5] assistant(text + tool_use A + tool_use B)
        //   [6] user(tool_result A + tool_result B)
        //   [7] assistant(tool_use C)
        //   [8] user(tool_result C + text "no se que...")  ← MERGED
        assert_eq!(
            out.messages.len(),
            9,
            "expected 9 messages after merging consecutive same-role, got {}",
            out.messages.len()
        );

        // CRITICAL: verify NO two consecutive messages have the same role.
        for i in 1..out.messages.len() {
            assert_ne!(
                out.messages[i].role,
                out.messages[i - 1].role,
                "consecutive messages [{}] and [{}] both have role '{}' — \
                 Anthropic/MiniMax rejects this with (2013)",
                i - 1,
                i,
                out.messages[i].role
            );
        }

        // Verify the merged assistant message [1] contains all 3 texts.
        let asst1 = &out.messages[1];
        assert_eq!(asst1.role, "assistant");
        let text1 = asst1.content.as_str().expect("string content");
        assert!(text1.contains("Veo cómo se renderiza"));
        assert!(text1.contains("Eso no es el render"));
        assert!(text1.contains("El bloque core/html"));

        // Verify the merged assistant message [3] contains both texts.
        let asst3 = &out.messages[3];
        assert_eq!(asst3.role, "assistant");
        let text3 = asst3.content.as_str().expect("string content");
        assert!(text3.contains("0 placeholders"));
        assert!(text3.contains("Falta muy poco"));

        // The last message (index 8) must be a user message with an
        // array content containing [tool_result, text].
        let last = &out.messages[8];
        assert_eq!(last.role, "user");
        let blocks = last.content.as_array().expect("last msg is array");
        assert_eq!(blocks.len(), 2, "tool_result + text");
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "call_C");
        assert_eq!(blocks[0]["content"], "result C");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "no se que mecanismo es...");
    }

    #[test]
    fn h4_user_field_maps_to_anthropic_metadata_user_id() {
        // OpenAI's `user` field is documented as an opaque end-user
        // identifier for abuse detection. Anthropic has no direct
        // equivalent but reserves `metadata.user_id` for the same
        // purpose. The translator should produce exactly that shape.
        let mut req = openai_req_with(vec![("user", "go")]);
        req.user = Some("user-abc-123".to_string());
        let out = openai_to_anthropic(&req, "claude-3-opus-20240229", &req.messages, req.stream);
        let metadata = out
            .metadata
            .as_ref()
            .expect("metadata set when user is set");
        assert_eq!(metadata["user_id"], "user-abc-123");
    }

    #[test]
    fn h4_absent_optional_fields_default_to_none() {
        // The fix must not regress existing behaviour: a request
        // that does not set tools / tool_choice / top_k / user must
        // still serialise to a valid Anthropic request with those
        // fields absent (serde skip_serializing_if = "Option::is_none").
        let req = openai_req_with(vec![("user", "hi")]);
        let out = openai_to_anthropic(&req, "claude-3-opus-20240229", &req.messages, req.stream);
        assert!(out.tools.is_none());
        assert!(out.tool_choice.is_none());
        assert!(out.top_k.is_none());
        assert!(out.metadata.is_none());
    }

    #[test]
    fn parse_image_url_to_inline_data_extracts_base64() {
        let part_json = serde_json::json!({
            "type": "image_url",
            "image_url": {
                "url": "data:image/jpeg;base64,f00bar"
            }
        });

        let result = super::parse_image_url_to_inline_data(&part_json).unwrap();
        assert_eq!(result.mime_type, "image/jpeg");
        assert_eq!(result.data, "f00bar");

        let invalid_type = serde_json::json!({
            "type": "text",
            "text": "hello"
        });
        assert!(super::parse_image_url_to_inline_data(&invalid_type).is_none());

        let invalid_url = serde_json::json!({
            "type": "image_url",
            "image_url": {
                "url": "https://example.com/image.jpg"
            }
        });
        assert!(super::parse_image_url_to_inline_data(&invalid_url).is_none());
    }
