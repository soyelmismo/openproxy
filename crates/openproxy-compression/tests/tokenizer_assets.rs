//! Opt-in verification against the pinned public tokenizer artifacts.
//!
//! The multi-megabyte vocabularies are never committed. Materialize them with
//! `crates/openproxy-compression/scripts/fetch_tokenizers.py` and run:
//!
//! ```text
//! OPENPROXY_TOKENIZER_DIR=/path/to/versioned-assets \
//!     cargo test -p openproxy-compression --test tokenizer_assets
//! ```
//!
//! When `OPENPROXY_TOKENIZER_DIR` is absent these tests skip. When it is set,
//! missing/corrupt artifacts fail loudly (never silently skipped).

use std::path::PathBuf;

use openproxy_compression::token_estimate::{
    ModelFamily, count_tokens_for_model, estimate_request_prompt_tokens, family_for_model,
    has_family_tokenizer, validate_tokenizer_assets,
};
use openproxy_types::OpenAIRequest;
use openproxy_types::message::OpenAIMessage;

fn assets_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("OPENPROXY_TOKENIZER_DIR")?;
    let dir = PathBuf::from(dir);
    validate_tokenizer_assets(&dir).unwrap_or_else(|problems| {
        panic!("OPENPROXY_TOKENIZER_DIR is set but its artifacts are invalid: {problems:#?}")
    });
    Some(dir)
}

fn msg(role: &str, content: &str) -> OpenAIMessage {
    OpenAIMessage {
        role: role.to_string(),
        content: Some(serde_json::Value::String(content.to_string())),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: serde_json::Map::new(),
    }
}

const TEXTS: [&str; 6] = [
    "Der schnelle braune Fuchs springt über den faulen Hund. The quick brown fox jumps over the lazy dog 123 times.",
    "The quick brown fox jumps over the lazy dog.",
    "function hello_world() { return \"¡Hola!\"; }",
    "こんにちは世界、テストです。Bonjour le monde。",
    "fn main() { let v: Vec<u8> = vec![1,2,3]; println!(\"{:?}\", v); }",
    "According to Smith (2019, p. 42), \"the results were conclusive\" — see also Jones et al.",
];

#[test]
fn pinned_artifacts_match_reference_counts() {
    let Some(_) = assets_dir() else {
        return;
    };
    // Reference counts computed with the pinned public vocabularies and the
    // family-specific pre-tokenizer (see token_estimate.rs).
    let cases: [(ModelFamily, &str, [u32; 6]); 7] = [
        (
            ModelFamily::Claude,
            "claude-opus-5-5",
            [29, 10, 14, 18, 30, 24],
        ),
        (
            ModelFamily::Kimi,
            "moonshotai/Kimi-K2.7-Code",
            [31, 10, 13, 16, 28, 26],
        ),
        (
            ModelFamily::Qwen3,
            "kiro/qwen3-coder-next",
            [30, 10, 12, 10, 27, 28],
        ),
        (
            ModelFamily::DeepSeekV32,
            "kiro/deepseek-v3.2",
            [27, 10, 14, 15, 29, 25],
        ),
        (
            ModelFamily::Glm53,
            "zai-org/GLM-5.3",
            [28, 10, 12, 11, 27, 25],
        ),
        (
            ModelFamily::MiniMaxM3,
            "MiniMaxAI/MiniMax-M3",
            [28, 10, 12, 10, 28, 25],
        ),
        (
            ModelFamily::OpenAiO200k,
            "surplus/gpt-5.4",
            [27, 10, 12, 11, 27, 26],
        ),
    ];
    for (family, model, expected) in cases {
        assert_eq!(
            family_for_model(model),
            family,
            "family dispatch for {model}"
        );
        assert!(
            has_family_tokenizer(model),
            "pinned tokenizer for {model} did not load"
        );
        for (i, text) in TEXTS.iter().enumerate() {
            let got = count_tokens_for_model(Some(model), text);
            assert_eq!(
                got, expected[i],
                "family {family:?} text[{i}] mismatch: got {got}, expected {}",
                expected[i]
            );
        }
    }
}

#[test]
fn qwen_rendered_prompt_counts_roles_tools_and_generation_prompt() {
    let Some(_) = assets_dir() else {
        return;
    };
    let simple = OpenAIRequest {
        model: "qwen3-coder-next".to_string(),
        messages: vec![
            msg("system", "You are a helpful assistant."),
            msg("user", "What is the weather in Madrid?"),
        ],
        ..Default::default()
    };
    assert_eq!(
        estimate_request_prompt_tokens(&simple, Some("qwen3-coder-next")),
        26,
        "simple rendered prompt"
    );

    // The resolved target id drives tokenization; the client-facing combo
    // alias in `request.model` must not select a different template.
    let aliased = OpenAIRequest {
        model: "combo:kiro-qwen".to_string(),
        ..simple.clone()
    };
    assert_eq!(
        estimate_request_prompt_tokens(&aliased, Some("qwen3-coder-next")),
        26,
        "resolved target id must win over the combo alias"
    );

    let tool = serde_json::json!({
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get the current weather for a city.",
            "parameters": {
                "type": "object",
                "properties": {
                    "city": {"type": "string", "description": "City name"},
                    "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]}
                },
                "required": ["city"]
            }
        }
    });
    let with_tool = OpenAIRequest {
        tools: Some(vec![tool.clone()]),
        ..simple.clone()
    };
    assert_eq!(
        estimate_request_prompt_tokens(&with_tool, Some("qwen3-coder-next")),
        314,
        "rendered prompt with one tool"
    );

    let mut history = simple;
    history.messages.push(OpenAIMessage {
        role: "assistant".to_string(),
        content: Some(serde_json::Value::Null),
        name: None,
        tool_call_id: None,
        tool_calls: Some(vec![serde_json::json!({
            "id": "call_1",
            "type": "function",
            "function": {
                "name": "get_weather",
                "arguments": "{\"city\":\"Madrid\",\"unit\":\"celsius\"}"
            }
        })]),
        extra: serde_json::Map::new(),
    });
    history.messages.push(msg(
        "tool",
        "{\"temperature\": 21, \"condition\": \"sunny\"}",
    ));
    history.tools = Some(vec![tool]);
    assert_eq!(
        estimate_request_prompt_tokens(&history, Some("qwen3-coder-next")),
        374,
        "rendered prompt with tool-call history"
    );

    let multilingual = OpenAIRequest {
        model: "qwen3-coder-next".to_string(),
        messages: vec![msg(
            "user",
            "你好世界，请用中文回答。こんにちは。Änderungen: señor, café.",
        )],
        ..Default::default()
    };
    assert_eq!(
        estimate_request_prompt_tokens(&multilingual, Some("qwen3-coder-next")),
        25,
        "rendered multilingual prompt"
    );
}

#[test]
fn qwen_template_is_not_used_for_other_qwen_versions() {
    let Some(_) = assets_dir() else {
        return;
    };
    let request = OpenAIRequest {
        model: "qwen2.5-coder".to_string(),
        messages: vec![msg("user", "hello")],
        ..Default::default()
    };
    assert_eq!(family_for_model("qwen2.5-coder"), ModelFamily::Fallback);
    assert!(!has_family_tokenizer("qwen2.5-coder"));
    // Falls back to the estimated path, never the pinned Coder-Next template.
    let estimated = estimate_request_prompt_tokens(&request, Some("qwen2.5-coder"));
    assert!(estimated >= 4, "fallback estimate should include framing");
}

#[test]
fn qwen_template_is_gated_to_the_openai_chat_rail() {
    let Some(_) = assets_dir() else {
        return;
    };
    use openproxy_compression::token_estimate::{
        PromptRail, estimate_request_prompt_tokens_for_rail,
    };

    let request = OpenAIRequest {
        model: "qwen3-coder-next".to_string(),
        messages: vec![
            msg("system", "You are a helpful assistant."),
            msg("user", "What is the weather in Madrid?"),
        ],
        ..Default::default()
    };

    // Identity OpenAI rail: the rendered prompt is the prompt upstream sees.
    assert_eq!(
        estimate_request_prompt_tokens_for_rail(
            &request,
            Some("qwen3-coder-next"),
            PromptRail::OpenAiChat
        ),
        26
    );
    // Translated rail (Kiro and every non-OpenAI target rewrites the body):
    // the template no longer describes the upstream prompt, so the estimate
    // must not claim it.
    let translated = estimate_request_prompt_tokens_for_rail(
        &request,
        Some("qwen3-coder-next"),
        PromptRail::Translated,
    );
    assert_ne!(
        translated, 26,
        "the pinned template must not be used on translated rails"
    );
    assert!(translated >= 4);
}

#[test]
fn qwen_template_falls_back_when_it_cannot_represent_the_request() {
    let Some(_) = assets_dir() else {
        return;
    };

    let base = OpenAIRequest {
        model: "qwen3-coder-next".to_string(),
        messages: vec![msg("user", "hello")],
        ..Default::default()
    };
    let plain = estimate_request_prompt_tokens(&base, Some("qwen3-coder-next"));

    // `functions`/`response_format` have no slot in the pinned template.
    // Dropping them silently would undercount, so the heuristic path counts
    // them on top of the messages instead of rendering a partial prompt.
    let mut with_functions = base.clone();
    with_functions.extra.insert(
        "functions".to_string(),
        serde_json::json!([{
            "name": "get_weather",
            "description": "Get the current weather for a city.",
            "parameters": {
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }
        }]),
    );
    let with_functions_count =
        estimate_request_prompt_tokens(&with_functions, Some("qwen3-coder-next"));
    assert!(
        with_functions_count > plain,
        "legacy `functions` must be counted, not silently dropped"
    );

    let mut with_response_format = base;
    with_response_format.extra.insert(
        "response_format".to_string(),
        serde_json::json!({
            "type": "json_schema",
            "json_schema": {
                "name": "result",
                "schema": {"type": "object", "properties": {"answer": {"type": "string"}}}
            }
        }),
    );
    let with_response_format_count =
        estimate_request_prompt_tokens(&with_response_format, Some("qwen3-coder-next"));
    assert!(
        with_response_format_count > plain,
        "`response_format` must be counted, not silently dropped"
    );
}

#[test]
fn qwen_template_ignores_look_alike_model_ids() {
    let Some(_) = assets_dir() else {
        return;
    };
    let request = OpenAIRequest {
        model: "not-qwen3-coder-next-unknown".to_string(),
        messages: vec![msg("user", "hello")],
        ..Default::default()
    };
    // The look-alike id must not activate either the artifact or the template.
    assert_eq!(
        family_for_model("not-qwen3-coder-next-unknown"),
        ModelFamily::Fallback
    );
    assert!(!has_family_tokenizer("not-qwen3-coder-next-unknown"));
    let estimate = estimate_request_prompt_tokens(&request, Some("not-qwen3-coder-next-unknown"));
    assert!(estimate >= 4);
}
