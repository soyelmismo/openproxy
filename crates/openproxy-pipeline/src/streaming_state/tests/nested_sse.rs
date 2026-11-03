use super::super::normalizer::ReasoningNormalizer;
use crate::streaming::{StreamAction, StreamingChunkStage};

#[test]
fn test_reasoning_normalizer_unwraps_nested_openai_sse_chunk() {
    let mut normalizer = ReasoningNormalizer::new();
    let nested = r#"{"id":"chatcmpl-outer","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"data: {\"id\":\"chatcmpl-inner\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"thinking\",\"content\":\"\"}}]}\n\n"}}]}"#;
    let action = normalizer.process_chunk(nested);
    let StreamAction::Mutate(mutated) = action else {
        panic!("expected StreamAction::Mutate, got {action:?}");
    };
    assert!(mutated.contains("\"reasoning_content\":\"thinking\""));
    assert!(mutated.contains("\"content\":\"\""));
    assert!(!mutated.contains("data:"));
}

#[test]
fn test_reasoning_normalizer_unwraps_nested_cf_response_chunk() {
    let mut normalizer = ReasoningNormalizer::new();
    let nested = r#"{"id":"chatcmpl-outer","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"data: {\"response\":\"¡Hola!\"}\n\n"}}]}"#;
    let action = normalizer.process_chunk(nested);
    let StreamAction::Mutate(mutated) = action else {
        panic!("expected StreamAction::Mutate, got {action:?}");
    };
    assert!(mutated.contains("\"content\":\"¡Hola!\""));
    assert!(!mutated.contains("data:"));
}

#[test]
fn test_reasoning_normalizer_unwraps_nested_done_chunk() {
    let mut normalizer = ReasoningNormalizer::new();
    let nested = r#"{"id":"chatcmpl-outer","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"data: [DONE]\n\n"}}]}"#;
    let action = normalizer.process_chunk(nested);
    let StreamAction::Mutate(mutated) = action else {
        panic!("expected StreamAction::Mutate, got {action:?}");
    };
    assert!(mutated.contains("\"content\":\"\""));
    assert!(!mutated.contains("[DONE]"));
}
