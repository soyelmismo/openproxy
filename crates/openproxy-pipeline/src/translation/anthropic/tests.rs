use super::diagnostics::translate_anthropic_tool_choice_to_openai;
use super::identity::{
    CLAUDE_AGENT_SDK_IDENTITY, CLAUDE_CODE_CLI_IDENTITY, normalize_claude_client_identity,
};
use serde_json::json;

#[test]
fn test_translate_anthropic_tool_choice_to_openai() {
    let anthropic_tc = json!({
        "type": "tool",
        "name": "get_weather"
    });

    let openai_tc = translate_anthropic_tool_choice_to_openai(anthropic_tc);

    assert_eq!(
        openai_tc,
        json!({
            "type": "function",
            "function": { "name": "get_weather" }
        })
    );

    let fallback_tc = json!({"type": "any"});
    assert_eq!(
        translate_anthropic_tool_choice_to_openai(fallback_tc.clone()),
        fallback_tc
    );
}

#[test]
fn test_normalize_claude_client_identity() {
    assert_eq!(
        normalize_claude_client_identity(CLAUDE_AGENT_SDK_IDENTITY),
        CLAUDE_CODE_CLI_IDENTITY
    );
    let custom_prompt = "You are a specialized coding agent.";
    assert_eq!(
        normalize_claude_client_identity(custom_prompt),
        custom_prompt
    );
}
