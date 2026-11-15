use openproxy_compression::token_estimate::{estimate_completion_tokens, estimate_prompt_tokens};
use openproxy_types::OpenAIMessage;

#[test]
fn text_accounting_is_bpe_not_char_division() {
    // cl100k_base encodes this to two tokens, not ceil(11 / 4) = three.
    assert_eq!(estimate_completion_tokens("Hello world"), 2);
    // A Chinese sentence demonstrates why characters/2 is not tokenization.
    assert_eq!(estimate_completion_tokens("你好世界"), 5);
    assert_eq!(estimate_completion_tokens("<|endoftext|>"), 7);
}

#[test]
fn compression_reports_bpe_savings_not_character_savings() -> Result<(), serde_json::Error> {
    let mut messages: Vec<OpenAIMessage> = serde_json::from_value(serde_json::json!([
        {"role":"user", "content":"hello   world   this   is   a   test   \n".repeat(50)}
    ]))?;
    let stats = openproxy_compression::apply_compression(
        &mut messages,
        openproxy_compression::CompressionMode::Lite,
    );
    assert_eq!(stats.original_tokens, 604);
    assert_eq!(stats.compressed_tokens, 604);
    assert_eq!(stats.savings_pct_opt(), None);
    assert!(stats.savings_pct_chars > 0.0);
    assert!(stats.compressed_chars < stats.original_chars);
    assert_eq!(
        stats.compressed_tokens,
        estimate_prompt_tokens(&messages) as usize
    );
    Ok(())
}

#[test]
fn message_tool_names_and_reasoning_are_counted() -> Result<(), serde_json::Error> {
    let plain: OpenAIMessage = serde_json::from_value(serde_json::json!({
        "role": "assistant", "content": null
    }))?;
    let rich: OpenAIMessage = serde_json::from_value(serde_json::json!({
        "role": "assistant", "content": null,
        "name": "research_agent",
        "reasoning_content": "Check every invoice before responding.",
        "tool_calls": [{"type":"function", "function": {
            "name": "search_invoice_archive", "arguments": "{}"
        }}]
    }))?;
    assert!(estimate_prompt_tokens(&[rich]) > estimate_prompt_tokens(&[plain]) + 10);
    Ok(())
}

#[test]
fn request_accounting_includes_tools_and_output_schema_but_not_controls()
-> Result<(), serde_json::Error> {
    use openproxy_compression::token_estimate::estimate_request_prompt_tokens;
    use openproxy_types::OpenAIRequest;
    let request: OpenAIRequest = serde_json::from_value(serde_json::json!({
        "model": "test", "messages": [{"role":"user", "content":"Hello world"}],
        "tools": [{"type":"function", "function":{"name":"lookup", "description": "search every invoice".repeat(100), "parameters":{"type":"object", "properties":{"query":{"type":"string"}}}}}],
        "response_format":{"type":"json_schema", "json_schema":{"name":"result", "schema":{"type":"object", "properties":{"answer":{"type":"string"}}}}}
    }))?;
    let full = estimate_request_prompt_tokens(&request);
    assert!(full > estimate_prompt_tokens(&request.messages) + 100);
    let mut control = request;
    control.model = "another-model".into();
    control.max_tokens = Some(130000);
    control.temperature = Some(0.9);
    assert_eq!(estimate_request_prompt_tokens(&control), full);
    Ok(())
}

#[test]
fn images_are_not_counted_as_base64_text() -> Result<(), serde_json::Error> {
    let message: OpenAIMessage = serde_json::from_value(serde_json::json!({
        "role":"user", "content":[{"type":"image_url", "image_url":{"url":format!("data:image/png;base64,{}", "A".repeat(100000))}}]
    }))?;
    assert_eq!(estimate_prompt_tokens(&[message]), 804);
    Ok(())
}
