use super::*;
use crate::streaming::{StreamAction, StreamingChunkStage};
use openproxy_types::{OpenAIChoice, OpenAIMessage, OpenAIResponse};
use serde_json::{Value, json};

#[test]
fn test_minimax_exact_user_prompt_example() {
    let input = r#"The Hugging Face org pages return HTML scaffolding only — actual model listings are loaded client-side via JS, so fetch_web_page only sees the empty shell. Let me hit their API directly instead.

<tool_call>
<invoke name="fetch_web_page"><url>https://huggingface.co/api/models?full=false&limit=15&search=dgx</url></invoke>
<invoke name="fetch_web_page"><url>https://huggingface.co/api/models?full=false&author=Qwen</url></invoke>
<invoke name="fetch_web_page"><url>https://huggingface.co/api/models?full=false&author=meta-llama&search=nvfp4</url></invoke>
<invoke name="fetch_web_page"><url>https://huggingface.co/api/models?full=false&limit=15&author=deepseek-ai</url></invoke>
</tool_call>"#;

    let extracted = extract_inline_tools(input);
    assert_eq!(
        extracted.clean_content,
        "The Hugging Face org pages return HTML scaffolding only — actual model listings are loaded client-side via JS, so fetch_web_page only sees the empty shell. Let me hit their API directly instead."
    );
    assert_eq!(extracted.tool_calls.len(), 4);

    assert_eq!(extracted.tool_calls[0].name, "fetch_web_page");
    let args0: Value = serde_json::from_str(&extracted.tool_calls[0].arguments).unwrap();
    assert_eq!(
        args0["url"],
        "https://huggingface.co/api/models?full=false&limit=15&search=dgx"
    );

    assert_eq!(extracted.tool_calls[1].name, "fetch_web_page");
    let args1: Value = serde_json::from_str(&extracted.tool_calls[1].arguments).unwrap();
    assert_eq!(
        args1["url"],
        "https://huggingface.co/api/models?full=false&author=Qwen"
    );

    assert_eq!(extracted.tool_calls[2].name, "fetch_web_page");
    let args2: Value = serde_json::from_str(&extracted.tool_calls[2].arguments).unwrap();
    assert_eq!(
        args2["url"],
        "https://huggingface.co/api/models?full=false&author=meta-llama&search=nvfp4"
    );

    assert_eq!(extracted.tool_calls[3].name, "fetch_web_page");
    let args3: Value = serde_json::from_str(&extracted.tool_calls[3].arguments).unwrap();
    assert_eq!(
        args3["url"],
        "https://huggingface.co/api/models?full=false&limit=15&author=deepseek-ai"
    );
}

#[test]
fn test_tool_call_only_clears_content() {
    let input = r#"<tool_call>
<invoke name="execute_command"><command>ls -la</command></invoke>
</tool_call>"#;

    let mut choice = OpenAIChoice {
        index: 0,
        message: OpenAIMessage {
            role: "assistant".to_string(),
            content: Some(Value::String(input.to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        },
        finish_reason: Some("stop".to_string()),
    };

    extract_inline_tools_from_choice(&mut choice);

    assert_eq!(choice.message.content, None);
    assert_eq!(choice.finish_reason.as_deref(), Some("tool_calls"));
    let tool_calls = choice.message.tool_calls.unwrap();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0]["function"]["name"], "execute_command");
    let args: Value =
        serde_json::from_str(tool_calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["command"], "ls -la");
}

#[test]
fn test_anthropic_parameter_tags() {
    let input = r#"<function_calls>
<invoke name="get_weather">
<parameter name="location">San Francisco, CA</parameter>
<parameter name="unit">celsius</parameter>
</invoke>
</function_calls>"#;

    let extracted = extract_inline_tools(input);
    assert_eq!(extracted.tool_calls.len(), 1);
    assert_eq!(extracted.tool_calls[0].name, "get_weather");
    let args: Value = serde_json::from_str(&extracted.tool_calls[0].arguments).unwrap();
    assert_eq!(args["location"], "San Francisco, CA");
    assert_eq!(args["unit"], "celsius");
}

#[test]
fn test_json_body_inside_invoke() {
    let input = r#"<invoke name="search">{"query": "rust async", "limit": 10}</invoke>"#;

    let extracted = extract_inline_tools(input);
    assert_eq!(extracted.tool_calls.len(), 1);
    assert_eq!(extracted.tool_calls[0].name, "search");
    let args: Value = serde_json::from_str(&extracted.tool_calls[0].arguments).unwrap();
    assert_eq!(args["query"], "rust async");
    assert_eq!(args["limit"], 10);
}

#[test]
fn test_scalar_value_types() {
    let input = r#"<invoke name="config">
<enabled>true</enabled>
<disabled>false</disabled>
<count>42</count>
<ratio>1.5</ratio>
<none_val>null</none_val>
<text_val>hello world</text_val>
</invoke>"#;

    let extracted = extract_inline_tools(input);
    let args: Value = serde_json::from_str(&extracted.tool_calls[0].arguments).unwrap();
    assert_eq!(args["enabled"], true);
    assert_eq!(args["disabled"], false);
    assert_eq!(args["count"], 42);
    assert_eq!(args["ratio"], 1.5);
    assert_eq!(args["none_val"], Value::Null);
    assert_eq!(args["text_val"], "hello world");
}

#[test]
fn test_hermes_json_tool_call() {
    let input = r#"I will search the web for dgx spark.
<tool_call>
{"name": "web_search", "arguments": {"query": "dgx spark"}}
</tool_call>"#;

    let extracted = extract_inline_tools(input);
    assert_eq!(
        extracted.clean_content,
        "I will search the web for dgx spark."
    );
    assert_eq!(extracted.tool_calls.len(), 1);
    assert_eq!(extracted.tool_calls[0].name, "web_search");
    let args: Value = serde_json::from_str(&extracted.tool_calls[0].arguments).unwrap();
    assert_eq!(args["query"], "dgx spark");
}

#[test]
fn test_tool_calls_bracket_marker() {
    let input = r#"Processing your request.
[TOOL_CALLS] [{"name": "fetch_info", "arguments": {"id": 123}}]"#;

    let extracted = extract_inline_tools(input);
    assert_eq!(extracted.clean_content, "Processing your request.");
    assert_eq!(extracted.tool_calls.len(), 1);
    assert_eq!(extracted.tool_calls[0].name, "fetch_info");
}

#[test]
fn test_extract_inline_tools_from_response() {
    let resp = OpenAIResponse {
        id: "chatcmpl-123".to_string(),
        object: "chat.completion".to_string(),
        created: 1234567890,
        model: "minimax-m2.7".to_string(),
        choices: vec![OpenAIChoice {
            index: 0,
            message: OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(Value::String("Calling tool now.\n<tool_call><invoke name=\"ping\"><host>1.1.1.1</host></invoke></tool_call>".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage: None,
    };

    let processed = extract_inline_tools_from_response(resp);
    let choice = &processed.choices[0];
    assert_eq!(choice.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(
        choice.message.content.as_ref().unwrap(),
        &Value::String("Calling tool now.".to_string())
    );
    let tool_calls = choice.message.tool_calls.as_ref().unwrap();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0]["function"]["name"], "ping");
}

#[test]
fn test_streaming_extractor_multi_chunk() {
    let mut extractor = InlineToolStreamExtractor::new();

    // Chunk 1: Normal text
    let chunk1 = json!({
        "choices": [{
            "index": 0,
            "delta": { "content": "Checking the API directly.\n\n" },
            "finish_reason": null
        }]
    })
    .to_string();
    let res1 = extractor.process_chunk(&chunk1);
    assert_eq!(res1, StreamAction::Passthrough);

    // Chunk 2: Opening tag and part of invoke
    let chunk2 = json!({
        "choices": [{
            "index": 0,
            "delta": { "content": "<tool_call>\n<invoke name=\"fetch_web" },
            "finish_reason": null
        }]
    })
    .to_string();
    let res2 = extractor.process_chunk(&chunk2);
    // Should suppress (skip) so client doesn't see partial XML
    assert_eq!(res2, StreamAction::Skip);

    // Chunk 3: Rest of invoke and close tag
    let chunk3 = json!({
        "choices": [{
            "index": 0,
            "delta": { "content": "_page\"><url>https://example.com</url></invoke>\n</tool_call>" },
            "finish_reason": null
        }]
    })
    .to_string();
    let res3 = extractor.process_chunk(&chunk3);
    match res3 {
        StreamAction::Mutate(payload) => {
            let val: Value = serde_json::from_str(&payload).unwrap();
            let tc = &val["choices"][0]["delta"]["tool_calls"];
            assert_eq!(tc[0]["function"]["name"], "fetch_web_page");
            let args: Value =
                serde_json::from_str(tc[0]["function"]["arguments"].as_str().unwrap()).unwrap();
            assert_eq!(args["url"], "https://example.com");
        }
        other => panic!("expected Mutate with tool_calls, got {other:?}"),
    }

    // Chunk 4: Final chunk with finish_reason: "stop" -> converted to "tool_calls"
    let chunk4 = json!({
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "stop"
        }]
    })
    .to_string();
    let res4 = extractor.process_chunk(&chunk4);
    match res4 {
        StreamAction::Mutate(payload) => {
            let val: Value = serde_json::from_str(&payload).unwrap();
            assert_eq!(val["choices"][0]["finish_reason"], "tool_calls");
        }
        other => panic!("expected Mutate with tool_calls finish_reason, got {other:?}"),
    }
}

#[test]
fn test_end_to_end_anthropic_translation_with_inline_tools() {
    let raw_minimax_response = OpenAIResponse {
        id: "chatcmpl-minimax-1".to_string(),
        object: "chat.completion".to_string(),
        created: 1234567890,
        model: "MiniMax-M2.7".to_string(),
        choices: vec![OpenAIChoice {
            index: 0,
            message: OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(Value::String(
                    r#"Checking models now.
<tool_call>
<invoke name="fetch_web_page"><url>https://huggingface.co/api/models?search=dgx</url></invoke>
</tool_call>"#
                        .to_string(),
                )),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage: None,
    };

    // 1. Pipeline extracts inline tools into OpenAI response
    let openai_resp = extract_inline_tools_from_response(raw_minimax_response);
    assert_eq!(
        openai_resp.choices[0].finish_reason.as_deref(),
        Some("tool_calls")
    );

    // 2. Client connecting via Anthropic (/v1/messages) receives translated Anthropic response
    let anthropic_resp = crate::translation::openai_response_to_anthropic(openai_resp);
    assert_eq!(anthropic_resp.stop_reason.as_deref(), Some("tool_use"));

    // Verify content blocks
    assert_eq!(anthropic_resp.content.len(), 2);
    assert_eq!(anthropic_resp.content[0]["type"], "text");
    assert_eq!(anthropic_resp.content[0]["text"], "Checking models now.");

    assert_eq!(anthropic_resp.content[1]["type"], "tool_use");
    assert_eq!(anthropic_resp.content[1]["name"], "fetch_web_page");
    assert_eq!(
        anthropic_resp.content[1]["input"]["url"],
        "https://huggingface.co/api/models?search=dgx"
    );
}

#[test]
fn test_anthropic_native_tool_use_response_translation() {
    let native_anthropic = crate::translation::types::AnthropicResponse {
        id: "msg_123".to_string(),
        response_type: "message".to_string(),
        role: "assistant".to_string(),
        content: vec![
            json!({"type": "text", "text": "I will check the weather."}),
            json!({
                "type": "tool_use",
                "id": "toolu_01A",
                "name": "get_weather",
                "input": {"city": "Tokyo"}
            }),
        ],
        model: "claude-3-5-sonnet".to_string(),
        stop_reason: Some("tool_use".to_string()),
        usage: crate::translation::types::AnthropicUsage {
            input_tokens: 10,
            output_tokens: 20,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        },
    };

    let openai_resp = crate::translation::anthropic_to_openai(&native_anthropic);
    assert_eq!(
        openai_resp.choices[0].finish_reason.as_deref(),
        Some("tool_calls")
    );
    assert_eq!(
        openai_resp.choices[0].message.content.as_ref().unwrap(),
        &Value::String("I will check the weather.".to_string())
    );
    let tc = openai_resp.choices[0].message.tool_calls.as_ref().unwrap();
    assert_eq!(tc.len(), 1);
    assert_eq!(tc[0]["id"], "toolu_01A");
    assert_eq!(tc[0]["function"]["name"], "get_weather");
    let args: Value =
        serde_json::from_str(tc[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["city"], "Tokyo");
}

#[test]
fn test_xml_entities_and_cdata() {
    let input = r#"<tool_call>
<invoke name="fetch_web_page">
<url>https://example.com/api?a=1&amp;b=2&amp;c=3</url>
<content><![CDATA[if (a < b && c > d) { return "ok"; }]]></content>
</invoke>
</tool_call>"#;

    let extracted = extract_inline_tools(input);
    assert_eq!(extracted.tool_calls.len(), 1);
    let args: Value = serde_json::from_str(&extracted.tool_calls[0].arguments).unwrap();
    assert_eq!(args["url"], "https://example.com/api?a=1&b=2&c=3");
    assert_eq!(args["content"], r#"if (a < b && c > d) { return "ok"; }"#);
}

#[test]
fn test_attribute_whitespace_and_variants() {
    let input = r#"<function name = "search_tool" id = 'custom_call_99'>
<query>test search</query>
</function>"#;

    let extracted = extract_inline_tools(input);
    assert_eq!(extracted.tool_calls.len(), 1);
    assert_eq!(extracted.tool_calls[0].name, "search_tool");
    assert_eq!(extracted.tool_calls[0].id, "custom_call_99");
    let args: Value = serde_json::from_str(&extracted.tool_calls[0].arguments).unwrap();
    assert_eq!(args["query"], "test search");
}

#[test]
fn test_consecutive_hermes_json_objects() {
    let input = r#"<tool_call>
{"name": "fetch_one", "arguments": {"x": 1}}
{"name": "fetch_two", "arguments": {"y": 2}}
</tool_call>"#;

    let extracted = extract_inline_tools(input);
    assert_eq!(extracted.tool_calls.len(), 2);
    assert_eq!(extracted.tool_calls[0].name, "fetch_one");
    assert_eq!(extracted.tool_calls[1].name, "fetch_two");
}

#[test]
fn test_streaming_tag_prefix_split_across_chunks() {
    let mut extractor = InlineToolStreamExtractor::new();

    // Chunk 1: ends with partial prefix "<tool_"
    let chunk1 = json!({
        "choices": [{
            "index": 0,
            "delta": { "content": "Checking data: <tool_" },
            "finish_reason": null
        }]
    })
    .to_string();
    let res1 = extractor.process_chunk(&chunk1);
    // Should emit "Checking data: " and buffer "<tool_"
    match res1 {
        StreamAction::Mutate(s) => {
            let val: Value = serde_json::from_str(&s).unwrap();
            assert_eq!(val["choices"][0]["delta"]["content"], "Checking data: ");
        }
        other => panic!("expected Mutate with content before, got {other:?}"),
    }

    // Chunk 2: completes tag and invoke
    let chunk2 = json!({
        "choices": [{
            "index": 0,
            "delta": { "content": "call>\n<invoke name=\"fetch_web_page\"><url>https://example.com</url></invoke>\n</tool_call>" },
            "finish_reason": null
        }]
    }).to_string();
    let res2 = extractor.process_chunk(&chunk2);
    match res2 {
        StreamAction::Mutate(s) => {
            let val: Value = serde_json::from_str(&s).unwrap();
            let tc = &val["choices"][0]["delta"]["tool_calls"];
            assert_eq!(tc[0]["function"]["name"], "fetch_web_page");
        }
        other => panic!("expected Mutate with tool_calls, got {other:?}"),
    }
}

#[test]
fn test_streaming_multi_invoke_split_across_chunks() {
    let mut extractor = InlineToolStreamExtractor::new();

    // Chunk 1: Opens <tool_call> and contains first invoke with its closing tag </invoke>
    let chunk1 = json!({
        "choices": [{
            "index": 0,
            "delta": { "content": "<tool_call>\n<invoke name=\"f1\"><url>u1</url></invoke>\n" },
            "finish_reason": null
        }]
    })
    .to_string();
    let res1 = extractor.process_chunk(&chunk1);
    // Should NOT close early at </invoke>; must stay buffering
    assert_eq!(res1, StreamAction::Skip);

    // Chunk 2: Second invoke and closing </tool_call>
    let chunk2 = json!({
        "choices": [{
            "index": 0,
            "delta": { "content": "<invoke name=\"f2\"><url>u2</url></invoke>\n</tool_call>" },
            "finish_reason": null
        }]
    })
    .to_string();
    let res2 = extractor.process_chunk(&chunk2);
    match res2 {
        StreamAction::Mutate(s) => {
            let val: Value = serde_json::from_str(&s).unwrap();
            let tc = &val["choices"][0]["delta"]["tool_calls"];
            assert_eq!(tc.as_array().unwrap().len(), 2);
            assert_eq!(tc[0]["function"]["name"], "f1");
            assert_eq!(tc[1]["function"]["name"], "f2");
        }
        other => panic!("expected Mutate with 2 tool_calls, got {other:?}"),
    }
}

#[test]
fn test_streaming_text_before_and_after_single_chunk() {
    let mut extractor = InlineToolStreamExtractor::new();

    let chunk = json!({
        "choices": [{
            "index": 0,
            "delta": { "content": "Before text <tool_call><invoke name=\"ping\"><host>1.1.1.1</host></invoke></tool_call> After text" },
            "finish_reason": null
        }]
    }).to_string();
    let res = extractor.process_chunk(&chunk);
    match res {
        StreamAction::Mutate(s) => {
            let val: Value = serde_json::from_str(&s).unwrap();
            let delta = &val["choices"][0]["delta"];
            assert_eq!(delta["content"], "Before text After text");
            assert_eq!(delta["tool_calls"][0]["function"]["name"], "ping");
        }
        other => panic!("expected Mutate with content and tool_calls, got {other:?}"),
    }
}

#[test]
fn test_self_closing_invoke_with_attributes() {
    let input = r#"Starting fetch:
<invoke name="fetch_web_page" url="https://example.com" timeout="30"/>
Done initiating fetch."#;

    let extracted = extract_inline_tools(input);
    assert_eq!(
        extracted.clean_content,
        "Starting fetch:\nDone initiating fetch."
    );
    assert_eq!(extracted.tool_calls.len(), 1);
    assert_eq!(extracted.tool_calls[0].name, "fetch_web_page");
    let args: Value = serde_json::from_str(&extracted.tool_calls[0].arguments).unwrap();
    assert_eq!(args["url"], "https://example.com");
    assert_eq!(args["timeout"], 30);
}

#[test]
fn test_multiple_self_closing_invokes() {
    let input = r#"<invoke name="cmd1" cmd="ls"/>
<invoke name="cmd2" cmd="pwd"/>"#;

    let extracted = extract_inline_tools(input);
    assert_eq!(extracted.clean_content, "");
    assert_eq!(extracted.tool_calls.len(), 2);
    assert_eq!(extracted.tool_calls[0].name, "cmd1");
    let args0: Value = serde_json::from_str(&extracted.tool_calls[0].arguments).unwrap();
    assert_eq!(args0["cmd"], "ls");

    assert_eq!(extracted.tool_calls[1].name, "cmd2");
    let args1: Value = serde_json::from_str(&extracted.tool_calls[1].arguments).unwrap();
    assert_eq!(args1["cmd"], "pwd");
}

#[test]
fn test_long_tag_prefix_split_across_streaming_chunks() {
    let mut extractor = InlineToolStreamExtractor::new();

    // Chunk 1: Ends with opening tag > 20 chars without closing '>'
    let chunk1 = json!({
        "choices": [{
            "index": 0,
            "delta": { "content": "Querying system: <invoke name=\"fetch_detailed_system_telemetry\"" },
            "finish_reason": null
        }]
    }).to_string();
    let res1 = extractor.process_chunk(&chunk1);
    match res1 {
        StreamAction::Mutate(s) => {
            let val: Value = serde_json::from_str(&s).unwrap();
            assert_eq!(val["choices"][0]["delta"]["content"], "Querying system: ");
        }
        other => panic!("expected Mutate with text before, got {other:?}"),
    }

    // Chunk 2: Closes '>' and body
    let chunk2 = json!({
        "choices": [{
            "index": 0,
            "delta": { "content": "><node>primary</node></invoke>" },
            "finish_reason": null
        }]
    })
    .to_string();
    let res2 = extractor.process_chunk(&chunk2);
    match res2 {
        StreamAction::Mutate(s) => {
            let val: Value = serde_json::from_str(&s).unwrap();
            let tc = &val["choices"][0]["delta"]["tool_calls"];
            assert_eq!(tc[0]["function"]["name"], "fetch_detailed_system_telemetry");
            let args: Value =
                serde_json::from_str(tc[0]["function"]["arguments"].as_str().unwrap()).unwrap();
            assert_eq!(args["node"], "primary");
        }
        other => panic!("expected Mutate with tool_calls, got {other:?}"),
    }
}

#[test]
fn test_multiple_tool_calls_in_single_streaming_chunk() {
    let mut extractor = InlineToolStreamExtractor::new();

    let chunk = json!({
        "choices": [{
            "index": 0,
            "delta": { "content": "<tool_call><invoke name=\"t1\"><x>1</x></invoke></tool_call> middle <tool_call><invoke name=\"t2\"><y>2</y></invoke></tool_call>" },
            "finish_reason": null
        }]
    }).to_string();
    let res = extractor.process_chunk(&chunk);
    match res {
        StreamAction::Mutate(s) => {
            let val: Value = serde_json::from_str(&s).unwrap();
            let delta = &val["choices"][0]["delta"];
            assert_eq!(delta["content"], "middle");
            let tc = delta["tool_calls"].as_array().unwrap();
            assert_eq!(tc.len(), 2);
            assert_eq!(tc[0]["function"]["name"], "t1");
            assert_eq!(tc[1]["function"]["name"], "t2");
        }
        other => panic!("expected Mutate with 2 tool_calls and middle text, got {other:?}"),
    }
}

#[test]
fn test_streaming_chunk_with_both_tool_call_and_stop_finish_reason() {
    let mut extractor = InlineToolStreamExtractor::new();

    let chunk = json!({
        "choices": [{
            "index": 0,
            "delta": { "content": "<tool_call><invoke name=\"final_action\"><done>true</done></invoke></tool_call>" },
            "finish_reason": "stop"
        }]
    }).to_string();
    let res = extractor.process_chunk(&chunk);
    match res {
        StreamAction::Mutate(s) => {
            let val: Value = serde_json::from_str(&s).unwrap();
            assert_eq!(val["choices"][0]["finish_reason"], "tool_calls");
            let tc = &val["choices"][0]["delta"]["tool_calls"];
            assert_eq!(tc[0]["function"]["name"], "final_action");
        }
        other => {
            panic!("expected Mutate with tool_calls and finish_reason: tool_calls, got {other:?}")
        }
    }
}

#[test]
fn test_multibyte_utf8_adjacent_to_tags() {
    let input = "¡Hola! 🚀<tool_call><invoke name=\"translate\"><text>hola</text></invoke></tool_call>✨ Adiós mundo 🌍";
    let extracted = extract_inline_tools(input);
    assert_eq!(extracted.clean_content, "¡Hola! 🚀✨ Adiós mundo 🌍");
    assert_eq!(extracted.tool_calls.len(), 1);
    assert_eq!(extracted.tool_calls[0].name, "translate");
}

#[test]
fn test_hermes_json_top_level_arguments() {
    let input = r#"<tool_call>
{"name": "calculator", "operation": "multiply", "a": 6, "b": 7}
</tool_call>"#;

    let extracted = extract_inline_tools(input);
    assert_eq!(extracted.tool_calls.len(), 1);
    assert_eq!(extracted.tool_calls[0].name, "calculator");
    let args: Value = serde_json::from_str(&extracted.tool_calls[0].arguments).unwrap();
    assert_eq!(args["operation"], "multiply");
    assert_eq!(args["a"], 6);
    assert_eq!(args["b"], 7);
}

#[test]
fn test_bare_self_closing_invoke_in_stream() {
    let mut extractor = InlineToolStreamExtractor::new();

    let chunk = json!({
        "choices": [{
            "index": 0,
            "delta": { "content": "Checking ping: <invoke name=\"ping\" host=\"1.1.1.1\"/>" },
            "finish_reason": null
        }]
    })
    .to_string();
    let res = extractor.process_chunk(&chunk);
    match res {
        StreamAction::Mutate(s) => {
            let val: Value = serde_json::from_str(&s).unwrap();
            let delta = &val["choices"][0]["delta"];
            assert_eq!(delta["content"], "Checking ping:");
            let tc = &delta["tool_calls"];
            assert_eq!(tc[0]["function"]["name"], "ping");
            let args: Value =
                serde_json::from_str(tc[0]["function"]["arguments"].as_str().unwrap()).unwrap();
            assert_eq!(args["host"], "1.1.1.1");
        }
        other => panic!("expected Mutate with ping tool_call, got {other:?}"),
    }
}
