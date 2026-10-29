use super::*;
use serde_json::json;

fn wrapped(model: &str, req_body: serde_json::Value) -> serde_json::Value {
    let adapter = AntigravityAdapter::new();
    let req: openproxy_types::OpenAIRequest = serde_json::from_value(req_body).unwrap();
    let model_id = ModelId::new(model);
    let target = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget::default(),
        model: openproxy_types::Model {
            model_id: model_id.clone(),
            ..Default::default()
        },
        api_key: String::new(),
        api_key_label: None,
        custom_meta: None,
    };
    let body = adapter
        .format_request(TargetFormat::Gemini, &req, &model_id, &req.messages, true)
        .unwrap();
    let body = adapter
        .wrap_request_body(body, TargetFormat::Gemini, &model_id, &target)
        .unwrap();
    serde_json::from_slice::<serde_json::Value>(&body).unwrap()["request"].clone()
}

/// Regression: the typed Gemini serializer used to emit `generation_config`
/// (snake_case), so the adapter's `generationConfig` lookup missed entirely and
/// the Claude thinking clamp never ran. Upstream then rejected the request with
/// `400 max_tokens must be greater than thinking.budget_tokens` — the exact
/// error seen in production for claude-opus-4-6-thinking.
#[test]
fn claude_thinking_keeps_max_output_tokens_above_budget() {
    for (max_tokens, effort) in [
        (4096u64, "medium"),
        (4096, "high"),
        (2048, "low"),
        (8192, "high"),
        (1024, "max"),
        (256, "high"),
    ] {
        let req = wrapped(
            "claude-opus-4-6-thinking",
            json!({
                "model": "claude-opus-4-6-thinking",
                "max_tokens": max_tokens,
                "reasoning_effort": effort,
                "messages": [{"role": "user", "content": "hi"}],
            }),
        );

        let gen_cfg = req
            .get("generationConfig")
            .unwrap_or_else(|| panic!("generationConfig missing (snake_case leak?): {req}"));

        let budget = gen_cfg["thinkingConfig"]["thinkingBudget"]
            .as_i64()
            .unwrap_or_else(|| panic!("thinkingBudget missing: {gen_cfg}"));
        let max_out = gen_cfg["maxOutputTokens"]
            .as_i64()
            .unwrap_or_else(|| panic!("maxOutputTokens missing: {gen_cfg}"));

        assert!(
            max_out > budget,
            "max_tokens={max_tokens} effort={effort}: maxOutputTokens({max_out}) must exceed \
             thinkingBudget({budget}) or Anthropic returns 400"
        );
        assert!(budget >= 1024, "budget floor is 1024, got {budget}");
    }
}

/// The Gemini wire format is camelCase. A snake_case key is silently dropped by
/// the upstream as an unknown field, so every one of these must be renamed.
#[test]
fn gemini_wire_keys_are_camel_case() {
    let req = wrapped(
        "gemini-3.8-flash-tiered",
        json!({
            "model": "gemini-3.8-flash-tiered",
            "max_tokens": 2048,
            "reasoning_effort": "low",
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "user", "content": [
                    {"type": "text", "text": "what is this"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGVsbG8="}}
                ]}
            ],
        }),
    );

    for snake in [
        "generation_config",
        "system_instruction",
        "max_output_tokens",
        "stop_sequences",
        "inline_data",
        "mime_type",
    ] {
        assert!(
            req.get(snake).is_none(),
            "snake_case key `{snake}` leaked to the wire: {req}"
        );
    }

    assert!(
        req.get("generationConfig").is_some(),
        "missing generationConfig"
    );
    assert!(
        req.get("systemInstruction").is_some(),
        "missing systemInstruction"
    );

    let gen_cfg = &req["generationConfig"];
    assert!(
        gen_cfg.get("maxOutputTokens").is_some(),
        "missing maxOutputTokens: {gen_cfg}"
    );

    // The image part must carry `inlineData`/`mimeType`, not snake_case.
    let contents = req["contents"].as_array().expect("contents array");
    let image_part = contents
        .iter()
        .flat_map(|c| c["parts"].as_array().cloned().unwrap_or_default())
        .find(|p| p.get("inlineData").is_some())
        .expect("image part must use inlineData");
    assert_eq!(image_part["inlineData"]["mimeType"], "image/png");
}
